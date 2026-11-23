// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

use objc2_app_kit::{NSAlert, NSModalResponse, NSWindowCollectionBehavior};

use crate::ui::icon;

/// Runs the alert modally on every space, including those of fullscreen apps.
///
/// Glide usually isn't the active app when it shows an alert, e.g. when it
/// starts at login, and the alert would otherwise open behind the frontmost
/// app. This needs the accessory activation policy: a regular app's windows
/// can't join other apps' fullscreen spaces.
///
/// The alert shows Glide's parachute as its icon, since the app icon is missing
/// when Glide runs outside its app bundle.
pub fn run_modal(alert: &NSAlert) -> NSModalResponse {
    if let Some(icon) = icon::parachute(true) {
        // SAFETY: The icon is not `None`.
        unsafe { alert.setIcon(Some(&icon)) };
    }
    alert.window().setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary,
    );
    alert.runModal()
}
