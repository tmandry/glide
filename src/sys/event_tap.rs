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

use std::cell::Cell;
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
use tracing::warn;

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
    in_callback: Rc<Cell<bool>>,
    mach_port: CFRetained<CFMachPort>,
    source: CFRetained<CFRunLoopSource>,
    run_loop: CFRetained<CFRunLoop>,
}

static_assertions::assert_not_impl_any!(EventTap: Send);

/// The boxed closure handed to the system as the tap's `user_info`.
struct Callback<F> {
    /// Whether `func` is on the stack. Check this before freeing the closure,
    /// so that dropping the tap from inside its own callback doesn't free a
    /// closure that is still running.
    in_callback: Rc<Cell<bool>>,
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
    /// runs. Note that the system may disable a tap that takes too long to
    /// respond, in which case the callback is called with
    /// [`CGEventType::TapDisabledByTimeout`].
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

        let in_callback = Rc::new(Cell::new(false));
        let callback = Box::into_raw(Box::new(Callback {
            in_callback: Rc::clone(&in_callback),
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

        let source = CFMachPort::new_run_loop_source(None, Some(&mach_port), 0)
            .expect("CFMachPortCreateRunLoopSource returned NULL");
        let run_loop = CFRunLoop::current().expect("no current run loop");
        run_loop.add_source(Some(&source), unsafe { kCFRunLoopCommonModes });

        Ok(EventTap {
            callback: callback as *mut (),
            dtor: destruct::<Callback<F>>,
            in_callback,
            mach_port,
            source,
            run_loop,
        })
    }

    /// Start delivering events to the callback.
    pub fn enable(&self) {
        CGEvent::tap_enable(&self.mach_port, true);
    }

    /// Stop delivering events to the callback.
    pub fn disable(&self) {
        CGEvent::tap_enable(&self.mach_port, false);
    }

    /// Return whether the tap is currently enabled.
    ///
    /// The system disables taps that take too long to respond, so this can
    /// become false without [`EventTap::disable`] being called.
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
        if self.in_callback.get() {
            // We were dropped by our own callback, which is still on the stack.
            // Leaking it is better than freeing it out from under itself.
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
    let was_in_callback = callback.in_callback.replace(true);
    // SAFETY: The system passes an event that is valid for the duration of the
    // callback. We do not hold on to the reference past this call.
    (callback.func)(event_type, unsafe { event.as_ref() });
    callback.in_callback.set(was_in_callback);
    // Return the event unchanged to pass it through without transferring
    // ownership. Listen-only taps ignore the return value.
    event.as_ptr()
}
