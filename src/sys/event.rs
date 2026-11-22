// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::cell::Cell;

use objc2_app_kit::NSEvent;
use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{
    CGDisplayHideCursor, CGDisplayShowCursor, CGError, CGWarpMouseCursorPosition,
    kCGNullDirectDisplay,
};
use serde::{Deserialize, Serialize};
use tracing::info_span;

use super::event_tap::CreateError;
use super::hotkey::{Hotkey, HotkeyTap};
use super::screen::CoordinateConverter;
use crate::actor::wm_controller::{Sender, WmCommand, WmEvent};

/// Sends a command to the WM controller when one of its hotkeys is pressed.
pub struct HotkeyManager {
    _tap: HotkeyTap,
}

struct Binding {
    cmd: WmCommand,
    presses: Cell<u64>,
}

impl HotkeyManager {
    pub fn new(
        events_tx: Sender,
        hotkeys: impl IntoIterator<Item = (Hotkey, WmCommand)>,
    ) -> Result<Self, CreateError> {
        let bindings = hotkeys
            .into_iter()
            .map(|(hotkey, cmd)| (hotkey, Binding { cmd, presses: Cell::new(0) }));
        let tap = HotkeyTap::new(bindings, move |hotkey, binding| {
            let seq = binding.presses.get() + 1;
            binding.presses.set(seq);
            let span = info_span!("hotkey::press", key_code = ?hotkey.key_code, ?seq);
            _ = events_tx.send((span, WmEvent::Command(binding.cmd.clone())));
        })?;
        Ok(HotkeyManager { _tap: tap })
    }
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
