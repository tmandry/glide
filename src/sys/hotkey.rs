// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Key combinations that trigger commands, and the syntax for writing them.
//!
//! A hotkey is written as zero or more modifiers followed by a key, separated
//! by `+`, for example `Alt + Shift + KeyH`. Key names are the [UI Events
//! `code` values](https://www.w3.org/TR/uievents-code/), which name physical
//! key positions on a US QWERTY keyboard.

use std::fmt;
use std::str::FromStr;

use keycode::{KeyMap, KeyMappingCode};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A key together with the modifiers that must be held when it is pressed.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Hotkey {
    pub modifiers: Modifiers,
    pub key_code: KeyCode,
}

bitflags::bitflags! {
    /// The modifier keys a hotkey requires.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct Modifiers: u8 {
        const SHIFT = 1 << 0;
        const CONTROL = 1 << 1;
        const ALT = 1 << 2;
        const META = 1 << 3;
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
}

impl KeyCode {
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
            modifiers |= match modifier {
                "Shift" => Modifiers::SHIFT,
                "Ctrl" => Modifiers::CONTROL,
                "Alt" | "Option" => Modifiers::ALT,
                "Meta" | "Cmd" | "Command" => Modifiers::META,
                _ => return Err(ParseError::UnknownModifier(modifier.to_owned())),
            };
        }
        Ok(modifiers)
    }
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
        let names = [
            (Modifiers::CONTROL, "Ctrl"),
            (Modifiers::ALT, "Alt"),
            (Modifiers::META, "Meta"),
            (Modifiers::SHIFT, "Shift"),
        ];
        let mut first = true;
        for (modifier, name) in names {
            if self.contains(modifier) {
                if !first {
                    f.write_str(" + ")?;
                }
                f.write_str(name)?;
                first = false;
            }
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
