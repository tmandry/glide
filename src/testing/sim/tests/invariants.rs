// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! What must hold of Glide whenever the system has settled, whatever the
//! scenario.

use std::collections::BTreeMap;

use objc2_core_foundation::CGRect;

use super::Harness;
use crate::actor::app::{SET_WINDOW_FRAME_RETRIES, WINDOW_NOTIFICATIONS};
use crate::actor::reactor::MAX_FRAME_ATTEMPTS;
use crate::testing::sim::Action;

/// How many accessibility writes Glide may make to one window before the
/// system settles. Setting a frame takes two. More than this means Glide is
/// fighting the app or itself.
const MAX_WRITES_PER_WINDOW: usize = 8;

impl Harness {
    /// Panic if the settled system breaks an invariant. `first_action` is the
    /// index of the first action taken since the system last settled.
    pub(super) fn check_invariants(&mut self, first_action: usize) {
        // Glide has no layout to select in until it has tiled a window there.
        let settled_layout = !self.sim.mac.borrow().mission_control_open()
            && !self.tiled(self.current_desktop()).is_empty();
        let selected = if settled_layout {
            self.selected()
        } else {
            None
        };
        let mac = self.sim.mac.borrow();
        let fail = |what: String| -> ! {
            panic!("{what}\n{}", self.scene().draw());
        };

        // A focus command moves the selection at most once, to the window it
        // focuses, and back again if the raise fails.
        if let Some(from) = self.focus_command
            && self.inputs() == self.inputs_at_settle + 1
        {
            let history = self.selection_history();
            let moved_to = &history[from..];
            let moved_back = moved_to.len() == 2
                && from > 0
                && moved_to[1] == history[from - 1]
                && mac.key_window() == Some(moved_to[1].as_str());
            if moved_to.len() > 1 && !moved_back {
                fail(format!("one focus command moved the selection to {moved_to:?}"));
            }
        }

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
        // Glide keeps placing a window that doesn't take its frame until it
        // has given up on it.
        let max_writes = |title: &str| {
            if self.frame_mismatch_allowed.contains(title) {
                MAX_FRAME_ATTEMPTS as usize * SET_WINDOW_FRAME_RETRIES * 2
            } else {
                MAX_WRITES_PER_WINDOW
            }
        };
        if let Some((title, n)) = writes.iter().find(|&(&title, &n)| n > max_writes(title)) {
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

        // Mission Control leaves Glide's layout behind until it closes, and a
        // hung app can't take the frames Glide gives its windows.
        if mac.mission_control_open() || mac.any_app_hung() {
            return;
        }
        let window_server = mac.window_server();
        let screen = window_server.screen();
        let on_screen: BTreeMap<String, CGRect> = window_server
            .windows_on_screen()
            .iter()
            .map(|w| (mac.title(w.id).unwrap().to_owned(), w.frame))
            .filter(|(title, _)| !self.floating.contains(title))
            .collect();
        let tiled = self.tiled(window_server.current_desktop_number());
        let mut shown: Vec<&String> = on_screen.keys().collect();
        shown.sort();
        if tiled.iter().collect::<Vec<_>>() != shown {
            fail(format!(
                "Glide has tiled {tiled:?} on the desktop being shown, which has {shown:?}"
            ));
        }

        let key_window = mac.key_window().map(str::to_owned);
        if key_window.as_ref().is_some_and(|key| tiled.contains(key)) && selected != key_window {
            fail(format!(
                "Glide has {selected:?} selected, but {key_window:?} has the keyboard focus"
            ));
        }

        // A window that doesn't take its frame can stick out, overlap its
        // neighbors or leave a gap.
        let allowed = |title: &str| self.frame_mismatch_allowed.contains(title);
        let area = |r: &CGRect| r.size.width * r.size.height;
        for (i, (a, frame)) in on_screen.iter().enumerate() {
            if !allowed(a) && !contains(&screen, frame) {
                fail(format!("{a} is partly off screen at {frame:?}"));
            }
            for (b, other) in on_screen.iter().skip(i + 1) {
                if !allowed(a) && !allowed(b) && overlaps(frame, other) {
                    fail(format!("{a} and {b} overlap"));
                }
            }
        }
        // Frames are rounded to whole pixels, which can leave a gap of a pixel
        // beside each window.
        let covered: f64 = on_screen.values().map(area).sum();
        let rounding = on_screen.len() as f64 * (screen.size.width + screen.size.height);
        if on_screen.keys().all(|title| !allowed(title))
            && !on_screen.is_empty()
            && covered < area(&screen) - rounding
        {
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
