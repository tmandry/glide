// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Glide's parachute icon.

use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_app_kit::NSImage;
use objc2_foundation::NSData;

/// Load the parachute in color, or else as a template image that the system
/// tints to match its surroundings.
pub fn parachute(color: bool) -> Option<Retained<NSImage>> {
    let svg = if color {
        include_str!("../../site/src/assets/parachute-small.svg")
    } else {
        include_str!("../../site/src/assets/parachute-nocolor.svg")
    };
    let image = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(svg.as_bytes()))?;
    image.setTemplate(!color);
    Some(image)
}
