// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! What must hold of Glide whenever the system has settled, whatever the
//! scenario.

use std::collections::BTreeMap;

use objc2_core_foundation::CGRect;

use super::Harness;
use crate::actor::app::WINDOW_NOTIFICATIONS;
use crate::testing::sim::Action;

/// How many accessibility writes Glide may make to one window before the
/// system settles. Setting a frame takes two. More than this means Glide is
/// fighting the app or itself.
const MAX_WRITES_PER_WINDOW: usize = 8;

impl Harness {
    /// Panic if the settled system breaks an invariant. `first_action` is the
    /// index of the first action taken since the system last settled.
    pub(super) fn check_invariants(&self, first_action: usize) {
        let mac = self.sim.mac.borrow();
        let fail = |what: String| -> ! {
            panic!("{what}\n{}", self.scene().draw());
        };

        let during_mission_control = self.actions_during_mission_control();
        if !during_mission_control.is_empty() {
            fail(format!(
                "Glide acted on windows while Mission Control was open: \
                 {during_mission_control:?}"
            ));
        }

        let mut writes: BTreeMap<&str, usize> = BTreeMap::new();
        for (action, _) in mac.actions().iter().skip(first_action) {
            if let Action::Wrote(title) = action {
                *writes.entry(title).or_default() += 1;
            }
        }
        if let Some((title, n)) = writes.iter().find(|&(_, &n)| n > MAX_WRITES_PER_WINDOW) {
            fail(format!("Glide wrote to {title} {n} times"));
        }

        for (title, names) in mac.window_notifications() {
            let mut wanted = WINDOW_NOTIFICATIONS.to_vec();
            wanted.sort();
            if names != wanted {
                fail(format!(
                    "Glide is observing {names:?} on {title}, not {wanted:?}"
                ));
            }
        }

        // Mission Control leaves Glide's layout behind until it closes.
        if mac.mission_control_open() {
            return;
        }
        let window_server = mac.window_server();
        let screen = window_server.screen();
        let on_screen: BTreeMap<String, CGRect> = window_server
            .windows_on_screen()
            .iter()
            .map(|w| (mac.title(w.id).unwrap().to_owned(), w.frame))
            .collect();
        let tiled = self.tiled(window_server.current_desktop_number());
        let mut shown: Vec<&String> = on_screen.keys().collect();
        shown.sort();
        if tiled.iter().collect::<Vec<_>>() != shown {
            fail(format!(
                "Glide has tiled {tiled:?} on the desktop being shown, which has {shown:?}"
            ));
        }

        let area = |r: &CGRect| r.size.width * r.size.height;
        for (i, (a, frame)) in on_screen.iter().enumerate() {
            if !contains(&screen, frame) {
                fail(format!("{a} is partly off screen at {frame:?}"));
            }
            for (b, other) in on_screen.iter().skip(i + 1) {
                if overlaps(frame, other) {
                    fail(format!("{a} and {b} overlap"));
                }
            }
        }
        // Frames are rounded to whole pixels, which can leave a gap of a pixel
        // beside each window.
        let covered: f64 = on_screen.values().map(area).sum();
        let rounding = on_screen.len() as f64 * (screen.size.width + screen.size.height);
        if !on_screen.is_empty() && covered < area(&screen) - rounding {
            fail("the tiled windows leave part of the screen empty".to_owned());
        }
    }
}

fn contains(outer: &CGRect, inner: &CGRect) -> bool {
    inner.origin.x >= outer.origin.x
        && inner.origin.y >= outer.origin.y
        && inner.origin.x + inner.size.width <= outer.origin.x + outer.size.width
        && inner.origin.y + inner.size.height <= outer.origin.y + outer.size.height
}

fn overlaps(a: &CGRect, b: &CGRect) -> bool {
    a.origin.x < b.origin.x + b.size.width
        && b.origin.x < a.origin.x + a.size.width
        && a.origin.y < b.origin.y + b.size.height
        && b.origin.y < a.origin.y + a.size.height
}
