// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Identify the current login session, and read its Secure Input state.

use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{mem, ptr};

use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_graphics::{CGError, CGSessionCopyCurrentDictionary};
use serde::{Deserialize, Serialize};

use super::window_server::{GlobalNotifier, on_global_event};

/// An opaque identifier for a login session.
///
/// Space ids and other window server state are only meaningful within the
/// login session that produced them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginSessionId(String);

const UNIQUE_ID_KEY: &str = "CGSSessionUniqueSessionUUID";
const AUDIT_ID_KEY: &str = "kCGSSessionAuditIDKey";

impl LoginSessionId {
    pub fn current() -> Option<Self> {
        Self::from_session_dict(&*current_session_dict()?, boot_time)
    }

    fn from_session_dict(
        dict: &CFDictionary<CFString, CFType>,
        boot_time: impl FnOnce() -> Option<SystemTime>,
    ) -> Option<Self> {
        let unique_id = dict
            .get(&CFString::from_static_str(UNIQUE_ID_KEY))
            .and_then(|v| v.downcast::<CFString>().ok());
        if let Some(unique_id) = unique_id {
            return Some(Self(unique_id.to_string()));
        }
        // The audit session id distinguishes logins within one boot, but is
        // reused across boots, so qualify it with the boot time.
        let audit_id = dict
            .get(&CFString::from_static_str(AUDIT_ID_KEY))?
            .downcast::<CFNumber>()
            .ok()?
            .as_i64()?;
        let boot = boot_time()?.duration_since(UNIX_EPOCH).ok()?;
        Some(Self(format!(
            "{}.{:06}-{audit_id}",
            boot.as_secs(),
            boot.subsec_micros()
        )))
    }

    #[cfg(test)]
    pub fn new_for_test(id: &str) -> Self {
        Self(id.to_owned())
    }
}

fn current_session_dict() -> Option<CFRetained<CFDictionary<CFString, CFType>>> {
    let dict = CGSessionCopyCurrentDictionary()?;
    // SAFETY: The session dictionary has string keys.
    Some(unsafe { CFRetained::cast_unchecked(dict) })
}

/// The time the system booted.
pub fn boot_time() -> Option<SystemTime> {
    // SAFETY: timeval is plain old data.
    let mut tv: libc::timeval = unsafe { mem::zeroed() };
    let mut size = mem::size_of::<libc::timeval>();
    // SAFETY: kern.boottime is a struct timeval, and `tv` and `size` describe a
    // buffer of that size.
    let ret = unsafe {
        libc::sysctlbyname(
            c"kern.boottime".as_ptr(),
            (&raw mut tv).cast(),
            &mut size,
            ptr::null_mut(),
            0,
        )
    };
    if ret != 0 || size != mem::size_of::<libc::timeval>() {
        return None;
    }
    let secs = u64::try_from(tv.tv_sec).ok()?;
    let micros = u32::try_from(tv.tv_usec).ok()?;
    Some(UNIX_EPOCH + Duration::new(secs, micros * 1000))
}

/// Secure Event Input, which keeps keystrokes from reaching event taps.
///
/// Apps turn it on while the user types into a password field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SecureInput {
    /// The process that most recently turned Secure Input on, as the window
    /// server reports it.
    ///
    /// This can be stale: it isn't updated when a process that isn't the
    /// frontmost app turns Secure Input on.
    pub likely_holder: Option<libc::pid_t>,
}

const SECURE_INPUT_PID_KEY: &str = "kCGSSessionSecureInputPID";

/// Return the Secure Input state, or `None` if it is off.
pub fn secure_input() -> Option<SecureInput> {
    // SAFETY: The function takes no arguments and only reads system state.
    if unsafe { IsSecureEventInputEnabled() } == 0 {
        return None;
    }
    let likely_holder = current_session_dict().and_then(|dict| {
        let pid = dict
            .get(&CFString::from_static_str(SECURE_INPUT_PID_KEY))?
            .downcast::<CFNumber>()
            .ok()?
            .as_i32()?;
        (pid > 0).then_some(pid)
    });
    Some(SecureInput { likely_holder })
}

/// The window server events sent when Secure Input turns on for the session,
/// and when it turns off. Neither is sent when another process turns it on
/// while it is already on. Found by watching the events sent on macOS 26.
const SECURE_INPUT_ON_EVENT: u32 = 752;
const SECURE_INPUT_OFF_EVENT: u32 = 753;

/// Call `callback` on the current thread whenever Secure Input turns on or
/// off, until the returned notifiers are dropped.
pub fn on_secure_input_change(
    callback: impl Fn() + Clone + 'static,
) -> Result<[GlobalNotifier; 2], CGError> {
    let on_callback = callback.clone();
    Ok([
        on_global_event(SECURE_INPUT_ON_EVENT, move |_, _| on_callback())?,
        on_global_event(SECURE_INPUT_OFF_EVENT, move |_, _| callback())?,
    ])
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn IsSecureEventInputEnabled() -> u8;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dict(entries: &[(&str, &CFType)]) -> CFRetained<CFDictionary<CFString, CFType>> {
        let keys: Vec<_> = entries.iter().map(|(k, _)| CFString::from_str(k)).collect();
        let keys: Vec<&CFString> = keys.iter().map(|k| &**k).collect();
        let values: Vec<&CFType> = entries.iter().map(|(_, v)| *v).collect();
        CFDictionary::from_slices(&keys, &values)
    }

    fn boot_at(secs: u64, micros: u32) -> impl FnOnce() -> Option<SystemTime> {
        move || Some(UNIX_EPOCH + Duration::new(secs, micros * 1000))
    }

    fn no_boot_time() -> Option<SystemTime> {
        panic!("boot time should not be read")
    }

    #[test]
    fn uses_unique_session_id() {
        let uuid = CFString::from_static_str("E89828BC-6E1F-4DA5-AFE6-86C08CA44BF7");
        let audit = CFNumber::new_i64(100020);
        let dict = dict(&[(UNIQUE_ID_KEY, &uuid), (AUDIT_ID_KEY, &audit)]);
        assert_eq!(
            LoginSessionId::from_session_dict(&dict, no_boot_time),
            Some(LoginSessionId::new_for_test(
                "E89828BC-6E1F-4DA5-AFE6-86C08CA44BF7"
            ))
        );
    }

    #[test]
    fn falls_back_to_audit_id_and_boot_time() {
        let audit = CFNumber::new_i64(100020);
        let dict = dict(&[(AUDIT_ID_KEY, &audit)]);
        assert_eq!(
            LoginSessionId::from_session_dict(&dict, boot_at(1788148747, 133228)),
            Some(LoginSessionId::new_for_test("1788148747.133228-100020"))
        );
    }

    #[test]
    fn fallback_distinguishes_boots_and_logins() {
        let id = |audit_id, boot_secs| {
            let audit = CFNumber::new_i64(audit_id);
            LoginSessionId::from_session_dict(
                &dict(&[(AUDIT_ID_KEY, &audit)]),
                boot_at(boot_secs, 0),
            )
        };
        assert_ne!(id(100020, 1000), id(100020, 2000));
        assert_ne!(id(100020, 1000), id(100021, 1000));
    }

    #[test]
    fn no_id_without_audit_id_or_boot_time() {
        assert_eq!(LoginSessionId::from_session_dict(&dict(&[]), no_boot_time), None);
        let audit = CFNumber::new_i64(100020);
        let dict = dict(&[(AUDIT_ID_KEY, &audit)]);
        assert_eq!(LoginSessionId::from_session_dict(&dict, || None), None);
    }

    // The tests below read the real session, which not every test environment
    // has.

    #[test]
    fn current_session_is_stable() {
        let Some(id) = LoginSessionId::current() else { return };
        assert_eq!(Some(id), LoginSessionId::current());
    }

    #[test]
    fn fallback_works_on_this_system() {
        let Some(real) = current_session_dict() else { return };
        let audit = real.get(&CFString::from_static_str(AUDIT_ID_KEY)).expect("audit id missing");
        let dict = dict(&[(AUDIT_ID_KEY, &audit)]);
        let id = LoginSessionId::from_session_dict(&dict, boot_time);
        assert!(id.is_some());
        assert_eq!(id, LoginSessionId::from_session_dict(&dict, boot_time));
    }

    #[test]
    fn boot_time_is_in_the_past() {
        let boot = boot_time().unwrap();
        assert!(boot > UNIX_EPOCH && boot < SystemTime::now());
    }
}
