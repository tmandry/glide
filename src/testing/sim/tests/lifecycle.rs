// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Windows and apps appearing, disappearing and moving between desktops.

use test_log::test;

use super::{Harness, TwoDesktops, screen};
use crate::testing::sim::{Action, Schedule};

#[test]
fn main_window_change_before_window_created() {
    Schedule::each(|schedule| {
        let TwoDesktops { mut sim, a, .. } = TwoDesktops::new(schedule);
        sim.open_window_main_first(a, "A3");
        sim.run();

        assert_eq!(sim.tiled(1), ["A1", "A2", "A3"], "{schedule:?}");
        let scene = sim.scene();
        scene.assert("A1").left_of("A2").same_size_as("A2");
        scene.assert("A2").left_of("A3").same_size_as("A3");
    });
}

#[test]
fn window_opens_and_closes_before_the_first_read() {
    Schedule::each(|schedule| {
        let TwoDesktops { mut sim, a, .. } = TwoDesktops::new(schedule);
        let before = sim.scene().draw();
        sim.open_window(a, "A3");
        sim.close_window("A3");
        sim.run();

        assert_eq!(sim.tiled(1), ["A1", "A2"], "{schedule:?}");
        assert_eq!(sim.scene().draw(), before, "{schedule:?}");
    });
}

// TODO: Fails under GlideFirst. Glide reads B's window list before the window
// server has put B1 on a desktop, and never lays B1 out. Check whether a real
// app can list a window the window server hasn't placed, outside Mission
// Control, before treating this as a bug.
#[test]
#[ignore]
fn app_launches_during_startup() {
    Schedule::each(|schedule| {
        let mut sim = Harness::new(schedule, screen(), 1);
        let a = sim.launch_app("A");
        sim.open_window(a, "A1");
        sim.run();
        sim.start_glide();
        let b = sim.launch_app("B");
        sim.open_window(b, "B1");
        sim.run();

        assert_eq!(sim.tiled(1), ["A1", "B1"], "{schedule:?}");
        sim.scene().assert("A1").same_size_as("B1");
        let scene = sim.scene();
        let (a1, b1) = (scene.frame("A1"), scene.frame("B1"));
        assert!(
            a1.size.width + b1.size.width >= screen().size.width - 2.,
            "{schedule:?}: {}",
            scene.draw()
        );
    });
}

#[test]
fn desktop_round_trip_keeps_windows() {
    Schedule::each(|schedule| {
        let mut sim = Harness::new(schedule, screen(), 2);
        let a = sim.launch_app("A");
        sim.open_window(a, "A1");
        sim.open_window(a, "A2");
        sim.switch_desktop(2);
        sim.open_window(a, "A3");
        sim.open_window(a, "A4");
        sim.switch_desktop(1);
        sim.run();
        sim.start_glide();
        sim.run();

        let before = sim.scene().draw();
        assert_eq!(sim.tiled(1), ["A1", "A2"], "{schedule:?}");
        sim.clear_actions();

        sim.switch_desktop(2);
        sim.run();
        sim.switch_desktop(1);
        sim.run();

        assert_eq!(sim.scene().draw(), before, "{schedule:?}");
        assert_eq!(sim.tiled(1), ["A1", "A2"], "{schedule:?}");
        assert_eq!(sim.tiled(2), ["A3", "A4"], "{schedule:?}");
        // Only A3 and A4, which Glide has not laid out yet, need writes.
        for title in ["A1", "A2"] {
            let wrote = Action::Wrote(title.into());
            assert!(
                !sim.actions().contains(&wrote),
                "{schedule:?}: {:?}",
                sim.actions()
            );
        }
    });
}

// TODO: Fails under GlideFirst, for the same reason as
// `app_launches_during_startup`: Glide reads A's window list before the window
// server has put A1 on a desktop.
#[test]
#[ignore]
fn glide_starts_before_macos_has_caught_up() {
    Schedule::each(|schedule| {
        let mut sim = Harness::new(schedule, screen(), 1);
        let a = sim.launch_app("A");
        sim.open_window(a, "A1");
        sim.start_glide();
        sim.run();
        assert_eq!(sim.tiled(1), ["A1"], "{schedule:?}");
    });
}
