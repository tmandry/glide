// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Everything the app actor needs from the system: the accessibility
//! interface of its app, and the few other calls it makes.
//!
//! The app actor uses the names in this module. In tests they resolve to the
//! fakes in [`fake`], which have the same shape and forward every call to a
//! [`fake::System`] the test installs.

#[cfg(test)]
pub mod fake;

#[cfg(not(test))]
mod real {
    use std::time::{Duration, Instant};

    pub use accessibility::{AXAttribute, AXUIElement, AXUIElementActions, AXUIElementAttributes};
    use objc2::rc::Retained;
    use objc2_app_kit::NSRunningApplication;
    use objc2_core_foundation::CFRetained;

    pub use crate::sys::app::AXUIElementExt;
    use crate::sys::app::{NSRunningApplicationExt, ProcessInfo, pid_t};
    pub use crate::sys::event::get_mouse_state;
    pub use crate::sys::observer::Observer;
    pub use crate::sys::window_server::make_key_window;

    /// An owned reference to an accessibility element.
    pub type Element = CFRetained<AXUIElement>;

    pub type RunningApp = Retained<NSRunningApplication>;

    pub fn running_app(pid: pid_t) -> Option<RunningApp> {
        NSRunningApplication::with_process_id(pid)
    }

    /// Return whether the process is an XPC service.
    pub fn is_xpc(pid: pid_t) -> Result<bool, ()> {
        ProcessInfo::for_pid(pid).map(|info| info.is_xpc)
    }

    pub fn now() -> Instant {
        Instant::now()
    }

    /// Block the thread for `duration`.
    pub fn sleep(duration: Duration) {
        std::thread::sleep(duration);
    }
}

#[cfg(test)]
pub use fake::*;
#[cfg(not(test))]
pub use real::*;
