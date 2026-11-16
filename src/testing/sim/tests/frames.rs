// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Frame writes racing against changes the user makes.

use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use test_log::test;

use super::{Harness, TwoDesktops};
use crate::testing::sim::{Action, Schedule};

/// How many times Glide wrote to the window titled `title`.
fn writes(sim: &Harness, title: &str) -> usize {
    sim.actions().iter().filter(|a| **a == Action::Wrote(title.into())).count()
}

#[test]
fn stale_notification_from_an_earlier_write_is_ignored() {
    Schedule::each(|schedule| {
        let TwoDesktops { mut sim, a, .. } = TwoDesktops::new(schedule);
        // Two layout changes with nothing settling in between.
        sim.open_window(a, "A3");
        sim.open_window(a, "A4");
        sim.run();

        assert_eq!(sim.tiled(1), ["A1", "A2", "A3", "A4"], "{schedule:?}");
        let scene = sim.scene();
        // Glide picks where the new windows go.
        for title in ["A2", "A3", "A4"] {
            scene.assert("A1").same_size_as(title);
        }
        assert_eq!(scene.frame("A1").size.width, 250., "{schedule:?}");

        // Each layout change writes a window's size and position at most
        // once.
        for title in ["A1", "A2", "A3", "A4"] {
            let n = writes(&sim, title);
            assert!(n <= 4, "{schedule:?}: {title} was written {n} times");
        }
    });
}

#[test]
fn user_move_during_a_write_settles() {
    Schedule::each(|schedule| {
        let TwoDesktops { mut sim, a, .. } = TwoDesktops::new(schedule);
        sim.open_window(a, "A3");
        let moved = CGRect::new(CGPoint::new(40., 60.), CGSize::new(300., 500.));
        sim.set_window_frame("A1", moved);
        sim.run();

        assert_eq!(sim.tiled(1), ["A1", "A2", "A3"], "{schedule:?}");
        let scene = sim.scene();
        scene.assert("A1").left_of("A2").same_size_as("A2");
        scene.assert("A2").left_of("A3").same_size_as("A3");
    });
}

#[test]
fn window_closes_while_a_write_is_queued() {
    Schedule::each(|schedule| {
        let TwoDesktops { mut sim, a, .. } = TwoDesktops::new(schedule);
        sim.open_window(a, "A3");
        sim.close_window("A1");
        sim.run();

        assert_eq!(sim.tiled(1), ["A2", "A3"], "{schedule:?}");
        let scene = sim.scene();
        scene.assert("A2").left_of("A3").same_size_as("A3");
    });
}
