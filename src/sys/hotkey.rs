// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Key combinations that trigger commands, and the syntax for writing them.
//!
//! A hotkey is written as zero or more modifiers followed by a key, separated
//! by `+`, for example `Alt + Shift + KeyH`. A modifier name with `Left` or
//! `Right` on the end, like `AltLeft`, only matches the key on that side. Key
//! names are the [UI Events `code` values](https://www.w3.org/TR/uievents-code/),
//! which name physical key positions on a US QWERTY keyboard.

use std::fmt;
use std::str::FromStr;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use keycode::{KeyMap, KeyMappingCode};
use objc2_core_foundation::CFRunLoop;
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventTapLocation, CGEventTapPlacement, CGEventType,
};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tracing::warn;

use super::event_tap::{CreateError, Disposition, EventTap};
use super::run_loop::WakeupHandle;

/// A key together with the modifiers that must be held when it is pressed.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Hotkey {
    pub modifiers: Modifiers,
    pub key_code: KeyCode,
}

bitflags::bitflags! {
    /// A set of modifier keys, with a flag for each side of the keyboard.
    ///
    /// In a hotkey, a modifier with both of its flags set accepts the key on
    /// either side. In the modifiers held during a key press, it means both
    /// keys are held, or that the event didn't say which one is.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct Modifiers: u8 {
        const SHIFT_LEFT = 1 << 0;
        const SHIFT_RIGHT = 1 << 1;
        const CONTROL_LEFT = 1 << 2;
        const CONTROL_RIGHT = 1 << 3;
        const ALT_LEFT = 1 << 4;
        const ALT_RIGHT = 1 << 5;
        const META_LEFT = 1 << 6;
        const META_RIGHT = 1 << 7;

        const SHIFT = Self::SHIFT_LEFT.bits() | Self::SHIFT_RIGHT.bits();
        const CONTROL = Self::CONTROL_LEFT.bits() | Self::CONTROL_RIGHT.bits();
        const ALT = Self::ALT_LEFT.bits() | Self::ALT_RIGHT.bits();
        const META = Self::META_LEFT.bits() | Self::META_RIGHT.bits();
    }
}

/// Each modifier with its left and right flags, in the order they are
/// displayed.
const MODIFIER_KEYS: [(&str, Modifiers, Modifiers); 4] = [
    ("Ctrl", Modifiers::CONTROL_LEFT, Modifiers::CONTROL_RIGHT),
    ("Alt", Modifiers::ALT_LEFT, Modifiers::ALT_RIGHT),
    ("Meta", Modifiers::META_LEFT, Modifiers::META_RIGHT),
    ("Shift", Modifiers::SHIFT_LEFT, Modifiers::SHIFT_RIGHT),
];

impl Modifiers {
    /// Return whether a hotkey requiring these modifiers matches a key press
    /// made while the `held` modifiers were down.
    ///
    /// Every modifier the hotkey requires must be held, and no others. A
    /// modifier that names a side must be held on that side only.
    pub fn matches(self, held: Modifiers) -> bool {
        MODIFIER_KEYS.iter().all(|&(_, left, right)| {
            let both = left | right;
            let (required, held) = (self & both, held & both);
            if required == both {
                !held.is_empty()
            } else {
                required == held
            }
        })
    }

    /// Return whether some set of held modifiers matches both `self` and
    /// `other`.
    fn overlaps(self, other: Modifiers) -> bool {
        MODIFIER_KEYS.iter().all(|&(_, left, right)| {
            let both = left | right;
            let (a, b) = (self & both, other & both);
            a == b || (a == both && !b.is_empty()) || (b == both && !a.is_empty())
        })
    }
}

/// A physical key, identified by its UI Events `code` value.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyCode(KeyMappingCode);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("unknown key \"{0}\"")]
    UnknownKey(String),
    #[error("unknown modifier \"{0}\"")]
    UnknownModifier(String),
    #[error("modifier \"{0}\" conflicts with an earlier one")]
    ConflictingModifier(String),
}

impl KeyCode {
    /// Return whether this is a modifier key, which macOS doesn't report with
    /// key-down events.
    fn is_modifier(self) -> bool {
        use KeyMappingCode::*;
        matches!(
            self.0,
            ShiftLeft
                | ShiftRight
                | ControlLeft
                | ControlRight
                | AltLeft
                | AltRight
                | MetaLeft
                | MetaRight
                | CapsLock
                | Fn
        )
    }

    /// The macOS virtual keycode of this key, or `None` if Mac keyboards don't
    /// have it.
    pub fn mac_keycode(self) -> Option<u16> {
        // Chromium's table, which the keycode crate is generated from, leaves
        // these keys unmapped on macOS.
        match self.0 {
            KeyMappingCode::Fn => Some(0x3F),
            KeyMappingCode::Lang1 => Some(0x68),
            KeyMappingCode::Lang2 => Some(0x66),
            code => Some(KeyMap::from(code).mac).filter(|&mac| mac != 0xFFFF),
        }
    }
}

impl FromStr for Hotkey {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        let (modifiers, key_code) = match s.rsplit_once('+') {
            Some((modifiers, key_code)) => (modifiers.parse()?, key_code),
            None => (Modifiers::empty(), s),
        };
        Ok(Hotkey {
            modifiers,
            key_code: key_code.trim().parse()?,
        })
    }
}

impl FromStr for Modifiers {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        let mut modifiers = Modifiers::empty();
        for modifier in s.split('+').map(str::trim) {
            let (name, side) = if let Some(name) = modifier.strip_suffix("Left") {
                (name, Side::Left)
            } else if let Some(name) = modifier.strip_suffix("Right") {
                (name, Side::Right)
            } else {
                (modifier, Side::Either)
            };
            let (left, right) = match name {
                "Shift" => (Modifiers::SHIFT_LEFT, Modifiers::SHIFT_RIGHT),
                "Ctrl" | "Control" => (Modifiers::CONTROL_LEFT, Modifiers::CONTROL_RIGHT),
                "Alt" | "Option" => (Modifiers::ALT_LEFT, Modifiers::ALT_RIGHT),
                "Meta" | "Cmd" | "Command" => (Modifiers::META_LEFT, Modifiers::META_RIGHT),
                _ => return Err(ParseError::UnknownModifier(modifier.to_owned())),
            };
            let flags = match side {
                Side::Either => left | right,
                Side::Left => left,
                Side::Right => right,
            };
            let earlier = modifiers & (left | right);
            if !earlier.is_empty() && earlier != flags {
                return Err(ParseError::ConflictingModifier(modifier.to_owned()));
            }
            modifiers |= flags;
        }
        Ok(modifiers)
    }
}

enum Side {
    Either,
    Left,
    Right,
}

impl FromStr for KeyCode {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        let unknown = || ParseError::UnknownKey(s.to_owned());
        let code = match s {
            "OSLeft" => KeyMappingCode::MetaLeft,
            "OSRight" => KeyMappingCode::MetaRight,
            "LaunchMediaPlayer" => KeyMappingCode::MediaSelect,
            "VolumeDown" => KeyMappingCode::AudioVolumeDown,
            "VolumeMute" => KeyMappingCode::AudioVolumeMute,
            "VolumeUp" => KeyMappingCode::AudioVolumeUp,
            _ if s.len() == 1 => {
                let c = s.chars().next().unwrap();
                let name = match c {
                    'A'..='Z' => format!("Key{c}"),
                    '0'..='9' => format!("Digit{c}"),
                    _ => return Err(unknown()),
                };
                name.parse().map_err(|()| unknown())?
            }
            _ => s.parse().map_err(|()| unknown())?,
        };
        Ok(KeyCode(code))
    }
}

impl fmt::Display for Hotkey {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if !self.modifiers.is_empty() {
            write!(f, "{} + ", self.modifiers)?;
        }
        write!(f, "{}", self.key_code)
    }
}

impl fmt::Debug for Hotkey {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for Modifiers {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut first = true;
        for (name, left, right) in MODIFIER_KEYS {
            let side = match (self.contains(left), self.contains(right)) {
                (true, true) => "",
                (true, false) => "Left",
                (false, true) => "Right",
                (false, false) => continue,
            };
            if !first {
                f.write_str(" + ")?;
            }
            write!(f, "{name}{side}")?;
            first = false;
        }
        Ok(())
    }
}

impl fmt::Display for KeyCode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl fmt::Debug for KeyCode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl Serialize for Hotkey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Hotkey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// A tap that consumes key presses matching a set of hotkeys.
///
/// The system holds up every key press until the tap has looked at it, so the
/// tap runs on a thread of its own. Dropping the `HotkeyTap` stops the tap and
/// waits for its thread to exit.
pub struct HotkeyTap {
    stop: WakeupHandle,
    thread: Option<JoinHandle<()>>,
}

type Bindings<T> = FxHashMap<u16, Vec<(Hotkey, T)>>;

impl HotkeyTap {
    /// Start a tap that calls `on_press` from the tap's thread whenever one of
    /// `hotkeys` is pressed, with the value bound to it.
    ///
    /// The key press is kept from the rest of the system. Repeats from holding
    /// the key down are passed through without calling `on_press`.
    pub fn new<T: Send + 'static>(
        hotkeys: impl IntoIterator<Item = (Hotkey, T)>,
        on_press: impl Fn(Hotkey, &T) + Send + 'static,
    ) -> Result<Self, CreateError> {
        let bindings = bindings_by_keycode(hotkeys);
        let (tx, rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("hotkeys".into())
            .spawn(move || {
                let tap = EventTap::new_active(
                    CGEventTapLocation::SessionEventTap,
                    CGEventTapPlacement::HeadInsertEventTap,
                    &[CGEventType::KeyDown],
                    move |_, event| on_key_down(&bindings, &on_press, event),
                );
                let tap = match tap {
                    Ok(tap) => tap,
                    Err(err) => {
                        _ = tx.send(Err(err));
                        return;
                    }
                };
                let stop = WakeupHandle::for_current_thread(0, || {
                    CFRunLoop::current().expect("no current run loop").stop();
                });
                _ = tx.send(Ok(stop));
                CFRunLoop::run();
                drop(tap);
            })
            .expect("failed to spawn the hotkey thread");
        match rx.recv().expect("hotkey thread exited without reporting") {
            Ok(stop) => Ok(HotkeyTap { stop, thread: Some(thread) }),
            Err(err) => {
                _ = thread.join();
                Err(err)
            }
        }
    }
}

impl Drop for HotkeyTap {
    fn drop(&mut self) {
        self.stop.wake();
        if let Some(thread) = self.thread.take() {
            _ = thread.join();
        }
    }
}

fn bindings_by_keycode<T>(hotkeys: impl IntoIterator<Item = (Hotkey, T)>) -> Bindings<T> {
    let mut bindings = Bindings::<T>::default();
    for (hotkey, value) in hotkeys {
        let Some(keycode) = hotkey.key_code.mac_keycode() else {
            warn!("Ignoring hotkey {hotkey}: Mac keyboards don't have this key");
            continue;
        };
        if hotkey.key_code.is_modifier() {
            warn!("Ignoring hotkey {hotkey}: a modifier key can't be the key of a hotkey");
            continue;
        }
        let entry = bindings.entry(keycode).or_default();
        if entry.iter().any(|(bound, _)| bound.modifiers == hotkey.modifiers) {
            warn!("Ignoring hotkey {hotkey}: it is bound more than once");
            continue;
        }
        entry.push((hotkey, value));
    }
    // A press can match more than one hotkey, such as one that accepts either
    // side of a modifier and one that names the side. Check the one that names
    // more sides first.
    for entry in bindings.values_mut() {
        entry.sort_by_key(|(hotkey, _)| {
            (hotkey.modifiers.bits().count_ones(), hotkey.modifiers.bits())
        });
        for (i, (a, _)) in entry.iter().enumerate() {
            for (b, _) in &entry[i + 1..] {
                if a.modifiers.bits().count_ones() == b.modifiers.bits().count_ones()
                    && a.modifiers.overlaps(b.modifiers)
                {
                    warn!("Hotkeys {a} and {b} can both match a press; {a} takes precedence");
                }
            }
        }
    }
    bindings
}

fn on_key_down<T>(
    bindings: &Bindings<T>,
    on_press: &impl Fn(Hotkey, &T),
    event: &CGEvent,
) -> Disposition {
    if CGEvent::integer_value_field(Some(event), CGEventField::KeyboardEventAutorepeat) != 0 {
        return Disposition::Pass;
    }
    let keycode = CGEvent::integer_value_field(Some(event), CGEventField::KeyboardEventKeycode);
    let Some(candidates) = u16::try_from(keycode).ok().and_then(|k| bindings.get(&k)) else {
        return Disposition::Pass;
    };
    let pressed = pressed_modifiers(CGEvent::flags(Some(event)));
    match candidates.iter().find(|(hotkey, _)| hotkey.modifiers.matches(pressed)) {
        Some((hotkey, value)) => {
            on_press(*hotkey, value);
            Disposition::Consume
        }
        None => Disposition::Pass,
    }
}

/// Device-dependent flags for the left and right modifier keys, from
/// `IOLLEvent.h`.
mod device_flags {
    pub const LEFT_CONTROL: u64 = 0x0000_0001;
    pub const LEFT_SHIFT: u64 = 0x0000_0002;
    pub const RIGHT_SHIFT: u64 = 0x0000_0004;
    pub const LEFT_COMMAND: u64 = 0x0000_0008;
    pub const RIGHT_COMMAND: u64 = 0x0000_0010;
    pub const LEFT_ALT: u64 = 0x0000_0020;
    pub const RIGHT_ALT: u64 = 0x0000_0040;
    pub const RIGHT_CONTROL: u64 = 0x0000_2000;
}

/// Return the modifiers held according to an event's flags.
///
/// A modifier whose flags don't say which side is held, as in some synthetic
/// events, is reported as held on both sides.
fn pressed_modifiers(flags: CGEventFlags) -> Modifiers {
    use device_flags::*;
    let keys = [
        (
            CGEventFlags::MaskShift,
            LEFT_SHIFT,
            RIGHT_SHIFT,
            Modifiers::SHIFT_LEFT,
            Modifiers::SHIFT_RIGHT,
        ),
        (
            CGEventFlags::MaskControl,
            LEFT_CONTROL,
            RIGHT_CONTROL,
            Modifiers::CONTROL_LEFT,
            Modifiers::CONTROL_RIGHT,
        ),
        (
            CGEventFlags::MaskAlternate,
            LEFT_ALT,
            RIGHT_ALT,
            Modifiers::ALT_LEFT,
            Modifiers::ALT_RIGHT,
        ),
        (
            CGEventFlags::MaskCommand,
            LEFT_COMMAND,
            RIGHT_COMMAND,
            Modifiers::META_LEFT,
            Modifiers::META_RIGHT,
        ),
    ];
    let mut modifiers = Modifiers::empty();
    for (mask, left_flag, right_flag, left, right) in keys {
        let held_left = flags.0 & left_flag != 0;
        let held_right = flags.0 & right_flag != 0;
        if held_left {
            modifiers |= left;
        }
        if held_right {
            modifiers |= right;
        }
        if !held_left && !held_right && flags.contains(mask) {
            modifiers |= left | right;
        }
    }
    modifiers
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hotkey(s: &str) -> Hotkey {
        s.parse().unwrap()
    }

    #[test]
    fn parse_modifiers_and_key() {
        let hk = hotkey("Alt + Shift + KeyH");
        assert_eq!(hk.modifiers, Modifiers::ALT | Modifiers::SHIFT);
        assert_eq!(hk.key_code, "KeyH".parse().unwrap());
        assert_eq!(hk.key_code.mac_keycode(), Some(0x04));
    }

    #[test]
    fn parse_key_alone() {
        let hk = hotkey("F5");
        assert_eq!(hk.modifiers, Modifiers::empty());
        assert_eq!(hk.key_code.mac_keycode(), Some(0x60));
    }

    #[test]
    fn option_is_alt() {
        assert_eq!(hotkey("Option + H"), hotkey("Alt + H"));
    }

    #[test]
    fn cmd_is_meta() {
        assert_eq!(hotkey("Cmd + H"), hotkey("Meta + H"));
        assert_eq!(hotkey("Command + H"), hotkey("Meta + H"));
    }

    #[test]
    fn whitespace_around_tokens_is_optional() {
        assert_eq!(hotkey("Ctrl+Alt+KeyL"), hotkey("Ctrl + Alt + KeyL"));
        assert_eq!(hotkey("  Ctrl +  KeyL "), hotkey("Ctrl + KeyL"));
    }

    #[test]
    fn display_is_canonical() {
        assert_eq!(
            hotkey("Shift + Meta + Option + Ctrl + H").to_string(),
            "Ctrl + Alt + Meta + Shift + KeyH"
        );
        assert_eq!(hotkey("1").to_string(), "Digit1");
    }

    #[test]
    fn display_round_trips() {
        for s in [
            "Alt + KeyH",
            "Ctrl + Alt + Equal",
            "Meta + Shift + ArrowLeft",
            "PageUp",
        ] {
            assert_eq!(hotkey(s).to_string(), s);
        }
    }

    #[test]
    fn serde_round_trips() {
        let hk = hotkey("Alt + Shift + KeyE");
        let s = serde_json::to_string(&hk).unwrap();
        assert_eq!(s, "\"Alt + Shift + KeyE\"");
        assert_eq!(serde_json::from_str::<Hotkey>(&s).unwrap(), hk);
    }

    #[test]
    fn unknown_names_are_errors() {
        assert_eq!(
            "Hyper + KeyH".parse::<Hotkey>(),
            Err(ParseError::UnknownModifier("Hyper".into()))
        );
        assert_eq!(
            "Alt + Foo".parse::<Hotkey>(),
            Err(ParseError::UnknownKey("Foo".into()))
        );
        assert_eq!(
            "alt + KeyH".parse::<Hotkey>(),
            Err(ParseError::UnknownModifier("alt".into()))
        );
        assert_eq!(
            "Alt + h".parse::<Hotkey>(),
            Err(ParseError::UnknownKey("h".into()))
        );
        assert_eq!(
            "Alt + + H".parse::<Hotkey>(),
            Err(ParseError::UnknownModifier("".into()))
        );
    }

    #[test]
    fn keys_missing_on_mac_parse_without_a_keycode() {
        assert_eq!(hotkey("F24").key_code.mac_keycode(), None);
    }

    #[test]
    fn parse_sided_modifiers() {
        let hk = hotkey("AltLeft + ShiftRight + KeyA");
        assert_eq!(hk.modifiers, Modifiers::ALT_LEFT | Modifiers::SHIFT_RIGHT);
        assert_eq!(hotkey("OptionRight + A"), hotkey("AltRight + A"));
        assert_eq!(hotkey("ControlLeft + A"), hotkey("CtrlLeft + A"));
        assert_eq!(hotkey("CmdRight + A"), hotkey("MetaRight + A"));
        assert_eq!(hotkey("CommandLeft + A"), hotkey("MetaLeft + A"));
        assert_eq!(hotkey("Control + A"), hotkey("Ctrl + A"));
    }

    #[test]
    fn sided_modifiers_display_round_trip() {
        for s in [
            "CtrlRight + Alt + KeyK",
            "AltLeft + KeyA",
            "MetaRight + ShiftLeft + Digit1",
        ] {
            assert_eq!(hotkey(s).to_string(), s);
        }
    }

    #[test]
    fn conflicting_sides_are_errors() {
        for s in [
            "AltLeft + AltRight + A",
            "Alt + AltLeft + A",
            "ShiftRight + Shift + A",
        ] {
            assert!(
                matches!(s.parse::<Hotkey>(), Err(ParseError::ConflictingModifier(_))),
                "{s}"
            );
        }
        assert_eq!(hotkey("Alt + Option + A"), hotkey("Alt + A"));
        assert_eq!(hotkey("AltLeft + OptionLeft + A"), hotkey("AltLeft + A"));
    }

    #[test]
    fn either_side_matches_any_held() {
        let alt = Modifiers::ALT;
        assert!(alt.matches(Modifiers::ALT_LEFT));
        assert!(alt.matches(Modifiers::ALT_RIGHT));
        assert!(alt.matches(Modifiers::ALT));
        assert!(!alt.matches(Modifiers::empty()));
        assert!(!alt.matches(Modifiers::ALT_LEFT | Modifiers::CONTROL_LEFT));
    }

    #[test]
    fn named_side_matches_only_that_side() {
        let alt_left = Modifiers::ALT_LEFT;
        assert!(alt_left.matches(Modifiers::ALT_LEFT));
        assert!(!alt_left.matches(Modifiers::ALT_RIGHT));
        assert!(!alt_left.matches(Modifiers::ALT));
        assert!(!alt_left.matches(Modifiers::empty()));
        assert!(!alt_left.matches(Modifiers::ALT_LEFT | Modifiers::SHIFT_RIGHT));
        let mixed = Modifiers::ALT_LEFT | Modifiers::SHIFT;
        assert!(mixed.matches(Modifiers::ALT_LEFT | Modifiers::SHIFT_RIGHT));
    }

    #[test]
    fn no_modifiers_match_only_none_held() {
        assert!(Modifiers::empty().matches(Modifiers::empty()));
        assert!(!Modifiers::empty().matches(Modifiers::META_LEFT));
    }

    #[test]
    fn pressed_modifiers_read_sides() {
        let flags = CGEventFlags(
            CGEventFlags::MaskAlternate.0 | CGEventFlags::MaskShift.0 | device_flags::RIGHT_ALT,
        );
        assert_eq!(pressed_modifiers(flags), Modifiers::ALT_RIGHT | Modifiers::SHIFT);
        let flags = CGEventFlags(
            CGEventFlags::MaskControl.0
                | device_flags::LEFT_CONTROL
                | device_flags::RIGHT_CONTROL
                | CGEventFlags::MaskAlphaShift.0,
        );
        assert_eq!(pressed_modifiers(flags), Modifiers::CONTROL);
        let flags = CGEventFlags(device_flags::LEFT_COMMAND);
        assert_eq!(pressed_modifiers(flags), Modifiers::META_LEFT);
    }

    #[test]
    fn sided_binding_is_checked_before_either_side() {
        let bindings = bindings_by_keycode([(hotkey("Alt + H"), 1), (hotkey("AltLeft + H"), 2)]);
        let first_match = |held: Modifiers| {
            bindings[&0x04]
                .iter()
                .find(|(hk, _)| hk.modifiers.matches(held))
                .map(|(_, v)| *v)
        };
        assert_eq!(first_match(Modifiers::ALT_LEFT), Some(2));
        assert_eq!(first_match(Modifiers::ALT_RIGHT), Some(1));
    }

    #[test]
    fn duplicate_bindings_keep_the_first() {
        let bindings = bindings_by_keycode([
            (hotkey("Alt + H"), 1),
            (hotkey("Option + KeyH"), 2),
            (hotkey("Alt + Shift + H"), 3),
            (hotkey("F24"), 4),
        ]);
        assert_eq!(bindings.len(), 1);
        let values: Vec<_> = bindings[&0x04].iter().map(|(_, v)| *v).collect();
        assert_eq!(values, [1, 3]);
    }

    #[test]
    fn modifier_keys_are_not_bound() {
        let bindings = bindings_by_keycode([
            (hotkey("ShiftLeft"), 1),
            (hotkey("Ctrl + AltRight"), 2),
            (hotkey("CapsLock"), 3),
            (hotkey("Fn"), 4),
        ]);
        assert!(bindings.is_empty());
    }

    #[test]
    fn overlapping_bindings_of_equal_specificity_are_ordered_by_modifiers() {
        let order = |hotkeys: [&'static str; 2]| {
            let bindings = bindings_by_keycode(hotkeys.map(|h| (hotkey(h), h)));
            bindings[&0x04].iter().map(|(_, v)| *v).collect::<Vec<_>>()
        };
        let a = "AltLeft + Shift + H";
        let b = "Alt + ShiftLeft + H";
        assert_eq!(order([a, b]), order([b, a]));
    }

    #[test]
    fn overlaps() {
        let m = |s: &str| hotkey(&format!("{s} + H")).modifiers;
        assert!(m("AltLeft + Shift").overlaps(m("Alt + ShiftLeft")));
        assert!(m("Alt").overlaps(m("AltRight")));
        assert!(!m("AltLeft").overlaps(m("AltRight")));
        assert!(!m("Alt").overlaps(m("Alt + Shift")));
    }

    #[test]
    fn livesplit_names_are_compatible() {
        for &(name, canonical, mac) in LIVESPLIT_KEYS {
            let key: KeyCode = name.parse().unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(key.to_string(), canonical, "{name}");
            assert_eq!(key.mac_keycode(), Some(mac), "{name}");
        }
    }

    /// Every key name livesplit-hotkey 0.8 accepted that it mapped to a macOS
    /// virtual keycode, with its canonical name and that keycode.
    const LIVESPLIT_KEYS: &[(&str, &str, u16)] = &[
        ("Backquote", "Backquote", 0x32),
        ("Backslash", "Backslash", 0x2A),
        ("BracketLeft", "BracketLeft", 0x21),
        ("BracketRight", "BracketRight", 0x1E),
        ("Comma", "Comma", 0x2B),
        ("Digit0", "Digit0", 0x1D),
        ("0", "Digit0", 0x1D),
        ("Digit1", "Digit1", 0x12),
        ("1", "Digit1", 0x12),
        ("Digit2", "Digit2", 0x13),
        ("2", "Digit2", 0x13),
        ("Digit3", "Digit3", 0x14),
        ("3", "Digit3", 0x14),
        ("Digit4", "Digit4", 0x15),
        ("4", "Digit4", 0x15),
        ("Digit5", "Digit5", 0x17),
        ("5", "Digit5", 0x17),
        ("Digit6", "Digit6", 0x16),
        ("6", "Digit6", 0x16),
        ("Digit7", "Digit7", 0x1A),
        ("7", "Digit7", 0x1A),
        ("Digit8", "Digit8", 0x1C),
        ("8", "Digit8", 0x1C),
        ("Digit9", "Digit9", 0x19),
        ("9", "Digit9", 0x19),
        ("Equal", "Equal", 0x18),
        ("IntlBackslash", "IntlBackslash", 0x0A),
        ("IntlRo", "IntlRo", 0x5E),
        ("IntlYen", "IntlYen", 0x5D),
        ("KeyA", "KeyA", 0x00),
        ("A", "KeyA", 0x00),
        ("KeyB", "KeyB", 0x0B),
        ("B", "KeyB", 0x0B),
        ("KeyC", "KeyC", 0x08),
        ("C", "KeyC", 0x08),
        ("KeyD", "KeyD", 0x02),
        ("D", "KeyD", 0x02),
        ("KeyE", "KeyE", 0x0E),
        ("E", "KeyE", 0x0E),
        ("KeyF", "KeyF", 0x03),
        ("F", "KeyF", 0x03),
        ("KeyG", "KeyG", 0x05),
        ("G", "KeyG", 0x05),
        ("KeyH", "KeyH", 0x04),
        ("H", "KeyH", 0x04),
        ("KeyI", "KeyI", 0x22),
        ("I", "KeyI", 0x22),
        ("KeyJ", "KeyJ", 0x26),
        ("J", "KeyJ", 0x26),
        ("KeyK", "KeyK", 0x28),
        ("K", "KeyK", 0x28),
        ("KeyL", "KeyL", 0x25),
        ("L", "KeyL", 0x25),
        ("KeyM", "KeyM", 0x2E),
        ("M", "KeyM", 0x2E),
        ("KeyN", "KeyN", 0x2D),
        ("N", "KeyN", 0x2D),
        ("KeyO", "KeyO", 0x1F),
        ("O", "KeyO", 0x1F),
        ("KeyP", "KeyP", 0x23),
        ("P", "KeyP", 0x23),
        ("KeyQ", "KeyQ", 0x0C),
        ("Q", "KeyQ", 0x0C),
        ("KeyR", "KeyR", 0x0F),
        ("R", "KeyR", 0x0F),
        ("KeyS", "KeyS", 0x01),
        ("S", "KeyS", 0x01),
        ("KeyT", "KeyT", 0x11),
        ("T", "KeyT", 0x11),
        ("KeyU", "KeyU", 0x20),
        ("U", "KeyU", 0x20),
        ("KeyV", "KeyV", 0x09),
        ("V", "KeyV", 0x09),
        ("KeyW", "KeyW", 0x0D),
        ("W", "KeyW", 0x0D),
        ("KeyX", "KeyX", 0x07),
        ("X", "KeyX", 0x07),
        ("KeyY", "KeyY", 0x10),
        ("Y", "KeyY", 0x10),
        ("KeyZ", "KeyZ", 0x06),
        ("Z", "KeyZ", 0x06),
        ("Minus", "Minus", 0x1B),
        ("Period", "Period", 0x2F),
        ("Quote", "Quote", 0x27),
        ("Semicolon", "Semicolon", 0x29),
        ("Slash", "Slash", 0x2C),
        ("AltLeft", "AltLeft", 0x3A),
        ("AltRight", "AltRight", 0x3D),
        ("Backspace", "Backspace", 0x33),
        ("CapsLock", "CapsLock", 0x39),
        ("ContextMenu", "ContextMenu", 0x6E),
        ("ControlLeft", "ControlLeft", 0x3B),
        ("ControlRight", "ControlRight", 0x3E),
        ("Enter", "Enter", 0x24),
        ("MetaLeft", "MetaLeft", 0x37),
        ("OSLeft", "MetaLeft", 0x37),
        ("MetaRight", "MetaRight", 0x36),
        ("OSRight", "MetaRight", 0x36),
        ("ShiftLeft", "ShiftLeft", 0x38),
        ("ShiftRight", "ShiftRight", 0x3C),
        ("Space", "Space", 0x31),
        ("Tab", "Tab", 0x30),
        ("Lang1", "Lang1", 0x68),
        ("Lang2", "Lang2", 0x66),
        ("Delete", "Delete", 0x75),
        ("End", "End", 0x77),
        ("Home", "Home", 0x73),
        ("Insert", "Insert", 0x72),
        ("PageDown", "PageDown", 0x79),
        ("PageUp", "PageUp", 0x74),
        ("ArrowDown", "ArrowDown", 0x7D),
        ("ArrowLeft", "ArrowLeft", 0x7B),
        ("ArrowRight", "ArrowRight", 0x7C),
        ("ArrowUp", "ArrowUp", 0x7E),
        ("NumLock", "NumLock", 0x47),
        ("Numpad0", "Numpad0", 0x52),
        ("Numpad1", "Numpad1", 0x53),
        ("Numpad2", "Numpad2", 0x54),
        ("Numpad3", "Numpad3", 0x55),
        ("Numpad4", "Numpad4", 0x56),
        ("Numpad5", "Numpad5", 0x57),
        ("Numpad6", "Numpad6", 0x58),
        ("Numpad7", "Numpad7", 0x59),
        ("Numpad8", "Numpad8", 0x5B),
        ("Numpad9", "Numpad9", 0x5C),
        ("NumpadAdd", "NumpadAdd", 0x45),
        ("NumpadComma", "NumpadComma", 0x5F),
        ("NumpadDecimal", "NumpadDecimal", 0x41),
        ("NumpadDivide", "NumpadDivide", 0x4B),
        // livesplit also matched 0x34, the Enter key on PowerBook keyboards.
        ("NumpadEnter", "NumpadEnter", 0x4C),
        ("NumpadEqual", "NumpadEqual", 0x51),
        ("NumpadMultiply", "NumpadMultiply", 0x43),
        ("NumpadSubtract", "NumpadSubtract", 0x4E),
        ("Escape", "Escape", 0x35),
        ("F1", "F1", 0x7A),
        ("F2", "F2", 0x78),
        ("F3", "F3", 0x63),
        ("F4", "F4", 0x76),
        ("F5", "F5", 0x60),
        ("F6", "F6", 0x61),
        ("F7", "F7", 0x62),
        ("F8", "F8", 0x64),
        ("F9", "F9", 0x65),
        ("F10", "F10", 0x6D),
        ("F11", "F11", 0x67),
        ("F12", "F12", 0x6F),
        ("F13", "F13", 0x69),
        ("F14", "F14", 0x6B),
        ("F15", "F15", 0x71),
        ("F16", "F16", 0x6A),
        ("F17", "F17", 0x40),
        ("F18", "F18", 0x4F),
        ("F19", "F19", 0x50),
        ("F20", "F20", 0x5A),
        ("Fn", "Fn", 0x3F),
        ("AudioVolumeDown", "AudioVolumeDown", 0x49),
        ("VolumeDown", "AudioVolumeDown", 0x49),
        ("AudioVolumeMute", "AudioVolumeMute", 0x4A),
        ("VolumeMute", "AudioVolumeMute", 0x4A),
        ("AudioVolumeUp", "AudioVolumeUp", 0x48),
        ("VolumeUp", "AudioVolumeUp", 0x48),
    ];
}
