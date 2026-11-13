// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Safe wrapper around listen-only `CGEventTap`s.
//!
//! ## Example
//!
//! ```no_run
//! # use objc2_core_foundation::CFRunLoop;
//! # use objc2_core_graphics::{CGEvent, CGEventTapLocation, CGEventTapPlacement, CGEventType};
//! # use glide_wm::sys::event_tap::EventTap;
//!
//! let tap = EventTap::new(
//!     CGEventTapLocation::SessionEventTap,
//!     CGEventTapPlacement::HeadInsertEventTap,
//!     &[CGEventType::LeftMouseDown],
//!     |_event_type, event| println!("{:?}", CGEvent::location(Some(event))),
//! )?;
//! tap.enable();
//!
//! CFRunLoop::run();
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::cell::{Cell, OnceCell};
use std::ffi::c_void;
use std::fmt;
use std::ptr::NonNull;
use std::rc::Rc;

use objc2_core_foundation::{
    CFMachPort, CFRetained, CFRunLoop, CFRunLoopSource, kCFRunLoopCommonModes,
};
use objc2_core_graphics::{
    CGEvent, CGEventMask, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventTapProxy, CGEventType,
};
use tracing::{error, warn};

/// Returned when the system refuses to create an event tap.
///
/// The most common cause is missing accessibility permission.
#[derive(Debug)]
pub struct CreateError;

impl fmt::Display for CreateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Could not create event tap")
    }
}

impl std::error::Error for CreateError {}

/// A listen-only tap on the system event stream.
///
/// The tap is attached to the run loop of the thread that creates it, and its
/// callback is called from that run loop while it is running. Dropping the tap
/// detaches it and stops any further callbacks.
///
/// This type is not `Send`, since it is tied to the run loop of the thread that
/// created it.
pub struct EventTap {
    callback: *mut (),
    dtor: unsafe fn(*mut ()),
    shared: Rc<Shared>,
    mach_port: CFRetained<CFMachPort>,
    source: CFRetained<CFRunLoopSource>,
    run_loop: CFRetained<CFRunLoop>,
}

static_assertions::assert_not_impl_any!(EventTap: Send);

/// State reachable both from the tap and from the callback the system holds.
struct Shared {
    /// Whether the closure is on the stack. Check this before freeing the
    /// closure, so that dropping the tap from inside its own callback doesn't
    /// free a closure that is still running.
    in_callback: Cell<bool>,

    /// Whether the tap is meant to be delivering events. The system reports a
    /// tap it disables through the callback, and it reports a call to
    /// `CGEventTapEnable(.., false)` the same way, so check this to tell a
    /// disable we asked for from one we didn't.
    want_enabled: Cell<bool>,

    /// The tap's mach port, filled in once `CGEventTapCreate` has returned it.
    /// The callback needs it to re-enable a tap the system has disabled.
    mach_port: OnceCell<CFRetained<CFMachPort>>,
}

/// The boxed closure handed to the system as the tap's `user_info`.
struct Callback<F> {
    shared: Rc<Shared>,
    func: F,
}

impl EventTap {
    /// Create a listen-only tap for the given event types on the current
    /// thread's run loop.
    ///
    /// Since the tap is listen-only, the callback observes events but cannot
    /// modify or discard them.
    ///
    /// The returned tap is enabled, but nothing is delivered until the run loop
    /// runs. The callback only sees the event types it subscribed to: if the
    /// system disables the tap for taking too long to respond, the tap
    /// re-enables itself and logs a warning instead of reporting it.
    pub fn new<F>(
        location: CGEventTapLocation,
        placement: CGEventTapPlacement,
        events: &[CGEventType],
        callback: F,
    ) -> Result<Self, CreateError>
    where
        F: Fn(CGEventType, &CGEvent) + 'static,
    {
        let mask = events.iter().fold(0 as CGEventMask, |mask, event_type| {
            let bit = 1u64
                .checked_shl(event_type.0)
                .expect("event type does not fit in a CGEventMask");
            mask | bit
        });

        let shared = Rc::new(Shared {
            in_callback: Cell::new(false),
            want_enabled: Cell::new(true),
            mach_port: OnceCell::new(),
        });
        let callback = Box::into_raw(Box::new(Callback {
            shared: Rc::clone(&shared),
            func: callback,
        }));

        // SAFETY: `tap_callback::<F>` is a correct callback for a tap created
        // with a `Callback<F>` as its `user_info`. The box outlives the tap: it
        // is only freed in `Drop`, after the tap has been invalidated.
        let mach_port = unsafe {
            CGEvent::tap_create(
                location,
                placement,
                CGEventTapOptions::ListenOnly,
                mask,
                Some(tap_callback::<F>),
                callback as *mut c_void,
            )
        };

        let Some(mach_port) = mach_port else {
            // SAFETY: The pointer came from `Box::into_raw` above. Tap creation
            // failed, so it was not shared with the system.
            drop(unsafe { Box::from_raw(callback) });
            return Err(CreateError);
        };

        // Publish the port before the run loop can dispatch anything, so the
        // callback can always re-enable a tap the system disables.
        let _ = shared.mach_port.set(mach_port.clone());

        let source = CFMachPort::new_run_loop_source(None, Some(&mach_port), 0)
            .expect("CFMachPortCreateRunLoopSource returned NULL");
        let run_loop = CFRunLoop::current().expect("no current run loop");
        run_loop.add_source(Some(&source), unsafe { kCFRunLoopCommonModes });

        Ok(EventTap {
            callback: callback as *mut (),
            dtor: destruct::<Callback<F>>,
            shared,
            mach_port,
            source,
            run_loop,
        })
    }

    /// Start delivering events to the callback.
    pub fn enable(&self) {
        self.shared.want_enabled.set(true);
        CGEvent::tap_enable(&self.mach_port, true);
    }

    /// Stop delivering events to the callback.
    pub fn disable(&self) {
        self.shared.want_enabled.set(false);
        CGEvent::tap_enable(&self.mach_port, false);
    }

    /// Return whether the tap is currently enabled.
    ///
    /// The system disables taps that take too long to respond, but the tap
    /// re-enables itself when that happens, so this only stays false after a
    /// call to [`EventTap::disable`].
    pub fn is_enabled(&self) -> bool {
        CGEvent::tap_is_enabled(&self.mach_port)
    }
}

impl Drop for EventTap {
    fn drop(&mut self) {
        // Stop the flow of events, then detach from the run loop, before
        // freeing the callback.
        self.disable();
        self.run_loop
            .remove_source(Some(&self.source), unsafe { kCFRunLoopCommonModes });
        self.mach_port.invalidate();
        if self.shared.in_callback.get() {
            // We were dropped by our own callback, which is still on the
            // stack. Leak the closure and the state it shares with us, rather
            // than freeing memory the callback is about to touch again.
            warn!("Event tap dropped from inside its callback; leaking the callback");
            return;
        }
        // SAFETY: The pointer came from `Box::into_raw` in `new` and has not
        // been freed. The tap is invalidated and detached from the only run
        // loop it was attached to, which belongs to this thread, and the check
        // above rules out a callback on this thread's stack, so no callback is
        // running or can start.
        unsafe { (self.dtor)(self.callback) };
    }
}

unsafe fn destruct<T>(ptr: *mut ()) {
    drop(unsafe { Box::from_raw(ptr as *mut T) });
}

/// Bridge from the C callback to the Rust closure.
unsafe extern "C-unwind" fn tap_callback<F>(
    _proxy: CGEventTapProxy,
    event_type: CGEventType,
    event: NonNull<CGEvent>,
    info: *mut c_void,
) -> *mut CGEvent
where
    F: Fn(CGEventType, &CGEvent) + 'static,
{
    // SAFETY: `info` is the `user_info` we passed to `tap_create`, which is a
    // pointer to a `Callback<F>` that is kept alive for as long as this
    // function can run. Only shared references to it are ever created.
    let callback = unsafe { &*(info as *const Callback<F>) };

    // The system disables a tap that it considers unresponsive, and reports it
    // here instead of through the events the caller asked for. Turn the tap
    // back on and keep the notification to ourselves, so that a caller's match
    // on event types only ever sees events it subscribed to.
    if matches!(
        event_type,
        CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput
    ) {
        if !callback.shared.want_enabled.get() {
            // A tap disabled through `EventTap::disable` is reported here too.
            // Leave it off.
            return event.as_ptr();
        }
        match callback.shared.mach_port.get() {
            Some(mach_port) => {
                warn!("Event tap was disabled ({event_type:?}); re-enabling it");
                CGEvent::tap_enable(mach_port, true);
            }
            // Unreachable: `new` publishes the port before the run loop can
            // dispatch to this function.
            None => error!("Event tap was disabled ({event_type:?}) before it was registered"),
        }
        return event.as_ptr();
    }

    let was_in_callback = callback.shared.in_callback.replace(true);
    // SAFETY: The system passes an event that is valid for the duration of the
    // callback. We do not hold on to the reference past this call.
    (callback.func)(event_type, unsafe { event.as_ref() });
    callback.shared.in_callback.set(was_in_callback);
    // Return the event unchanged to pass it through without transferring
    // ownership. Listen-only taps ignore the return value.
    event.as_ptr()
}
