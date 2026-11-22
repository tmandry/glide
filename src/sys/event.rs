// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

use livesplit_hotkey::{ConsumePreference, Hook};
use objc2_app_kit::NSEvent;
use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{
    CGDisplayHideCursor, CGDisplayShowCursor, CGError, CGWarpMouseCursorPosition,
    kCGNullDirectDisplay,
};
use serde::{Deserialize, Serialize};
use tracing::{info_span, warn};

use super::hotkey::{Hotkey, Modifiers};
use super::screen::CoordinateConverter;
use crate::actor::reactor::Command;
use crate::actor::wm_controller::{Sender, WmCommand, WmEvent};

pub struct HotkeyManager {
    hook: Hook,
    events_tx: Sender,
}

impl HotkeyManager {
    pub fn new(events_tx: Sender) -> Result<Self, livesplit_hotkey::Error> {
        let hook = Hook::with_consume_preference(ConsumePreference::MustConsume)?;
        Ok(HotkeyManager { hook, events_tx })
    }

    pub fn register(&self, hotkey: Hotkey, cmd: Command) {
        self.register_wm(hotkey, WmCommand::ReactorCommand(cmd))
    }

    pub fn register_wm(&self, hotkey: Hotkey, cmd: WmCommand) {
        let Some(livesplit_hotkey) = to_livesplit(hotkey) else {
            warn!("Ignoring hotkey {hotkey}: key is not available on macOS");
            return;
        };
        let events_tx = self.events_tx.clone();
        let key_code = hotkey.key_code;
        let mut seq = 0;
        self.hook
            .register(livesplit_hotkey, move || {
                seq += 1;
                let span = info_span!("hotkey::press", ?key_code, ?seq);
                events_tx.send((span, WmEvent::Command(cmd.clone()))).unwrap()
            })
            .unwrap();
    }
}

fn to_livesplit(hotkey: Hotkey) -> Option<livesplit_hotkey::Hotkey> {
    hotkey.key_code.mac_keycode()?;
    let key_code = hotkey.key_code.to_string().parse().ok()?;
    let mut modifiers = livesplit_hotkey::Modifiers::empty();
    for (ours, theirs) in [
        (Modifiers::SHIFT, livesplit_hotkey::Modifiers::SHIFT),
        (Modifiers::CONTROL, livesplit_hotkey::Modifiers::CONTROL),
        (Modifiers::ALT, livesplit_hotkey::Modifiers::ALT),
        (Modifiers::META, livesplit_hotkey::Modifiers::META),
    ] {
        if hotkey.modifiers.contains(ours) {
            modifiers |= theirs;
        }
    }
    Some(livesplit_hotkey::Hotkey { key_code, modifiers })
}

/// The state of the left mouse button.
#[derive(Serialize, Deserialize, Debug, Copy, Clone, Eq, PartialEq)]
pub enum MouseState {
    Down,
    Up,
}

pub fn get_mouse_state() -> MouseState {
    let left_button = NSEvent::pressedMouseButtons() & 0x1 != 0;
    if left_button {
        MouseState::Down
    } else {
        MouseState::Up
    }
}

pub fn get_mouse_pos(converter: CoordinateConverter) -> Option<CGPoint> {
    let ns_loc = NSEvent::mouseLocation();
    converter.convert_point(ns_loc)
}

pub fn warp_mouse(point: CGPoint) -> Result<(), CGError> {
    cg_result(CGWarpMouseCursorPosition(point))
}

/// Hide the mouse. Note that this will have no effect unless
/// [`window_server::allow_hide_mouse`] was called or this application is
/// focused.
pub fn hide_mouse() -> Result<(), CGError> {
    cg_result(CGDisplayHideCursor(kCGNullDirectDisplay))
}

pub fn show_mouse() -> Result<(), CGError> {
    cg_result(CGDisplayShowCursor(kCGNullDirectDisplay))
}

fn cg_result(err: CGError) -> Result<(), CGError> {
    if err == CGError::Success {
        Ok(())
    } else {
        Err(err)
    }
}
