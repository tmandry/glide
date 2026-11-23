// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Apps that don't behave like the plain windows the other scenarios use.

use accessibility_sys::kAXDialogSubrole;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use test_log::test;

use super::{Harness, screen};
use crate::actor::layout::LayoutCommand;
use crate::model::Direction;
use crate::testing::sim::{Action, Schedule, WindowTraits};

/// App A has a window A1, and opens A2 with `traits`. Glide is running and
/// the system has settled.
fn with_second_window(schedule: Schedule, traits: WindowTraits) -> Harness {
    let mut sim = Harness::new(schedule, screen(), 1);
    let a = sim.launch_app("A");
    sim.open_window(a, "A1");
    sim.run();
    sim.start_glide();
    sim.run();
    sim.open_window_with(a, "A2", traits);
    sim.allow_frame_mismatch("A2");
    sim.run();
    sim
}

fn frame(x: f64, y: f64, width: f64, height: f64) -> CGRect {
    CGRect::new(CGPoint::new(x, y), CGSize::new(width, height))
}

#[test]
fn a_window_wider_than_its_tile_settles() {
    Schedule::each(|schedule| {
        let traits = WindowTraits {
            min_size: Some(CGSize::new(700., 0.)),
            ..Default::default()
        };
        let sim = with_second_window(schedule, traits);
        assert_eq!(sim.tiled(1), ["A1", "A2"], "{schedule:?}");
        // The app keeps the width it needs and its neighbor keeps its tile,
        // so the window runs past the edge of the screen.
        let scene = sim.scene();
        assert_eq!(scene.frame("A1"), frame(0., 0., 500., 1000.), "{schedule:?}");
        assert_eq!(scene.frame("A2"), frame(500., 0., 700., 1000.), "{schedule:?}");
    });
}

#[test]
fn a_window_that_rounds_its_size_down_settles() {
    Schedule::each(|schedule| {
        let traits = WindowTraits {
            size_increment: Some(CGSize::new(7., 7.)),
            ..Default::default()
        };
        let sim = with_second_window(schedule, traits);
        assert_eq!(sim.tiled(1), ["A1", "A2"], "{schedule:?}");
        // The tile is 500 by 1000, which rounds down to 497 by 994.
        let scene = sim.scene();
        assert_eq!(scene.frame("A1"), frame(0., 0., 500., 1000.), "{schedule:?}");
        assert_eq!(scene.frame("A2"), frame(500., 0., 497., 994.), "{schedule:?}");
    });
}

/// App A has windows A1 and A2 and Glide is running.
fn two_windows(schedule: Schedule) -> Harness {
    let mut sim = Harness::new(schedule, screen(), 1);
    let a = sim.launch_app("A");
    sim.open_window(a, "A1");
    sim.open_window(a, "A2");
    sim.run();
    sim.start_glide();
    sim.run();
    sim
}

// TODO: Fails under GlideFirst. Glide hears about the minimized window but only
// updates its list of visible windows, so the window stays in the layout. See
// https://github.com/tmandry/glide/issues/209.
#[test]
#[ignore]
fn minimize_and_restore() {
    Schedule::each(|schedule| {
        let mut sim = two_windows(schedule);
        assert_eq!(sim.tiled(1), ["A1", "A2"], "{schedule:?}");

        sim.minimize_window("A1");
        sim.run();
        assert_eq!(sim.tiled(1), ["A2"], "{schedule:?}");
        sim.scene().assert("A2").fills_screen();
        assert_eq!(sim.key_window().as_deref(), Some("A2"), "{schedule:?}");

        sim.restore_window("A1");
        sim.run();
        assert_eq!(sim.tiled(1), ["A1", "A2"], "{schedule:?}");
        sim.scene().assert("A1").left_of("A2").same_size_as("A2");
        assert_eq!(sim.key_window().as_deref(), Some("A1"), "{schedule:?}");
    });
}

#[test]
fn a_window_minimized_before_glide_starts_is_not_tiled() {
    Schedule::each(|schedule| {
        let mut sim = Harness::new(schedule, screen(), 1);
        let a = sim.launch_app("A");
        sim.open_window(a, "A1");
        sim.open_window(a, "A2");
        sim.minimize_window("A1");
        sim.run();
        sim.start_glide();
        sim.run();
        assert_eq!(sim.tiled(1), ["A2"], "{schedule:?}");
        sim.scene().assert("A2").fills_screen();
    });
}

fn dialog() -> WindowTraits {
    WindowTraits {
        subrole: Some(kAXDialogSubrole),
        ..Default::default()
    }
}

/// App B has a dialog D. App A, which is frontmost, has windows A1 and A2.
/// Glide is running and the system has settled.
fn dialog_and_two_windows(schedule: Schedule) -> Harness {
    let mut sim = Harness::new(schedule, screen(), 1);
    let b = sim.launch_app("B");
    sim.open_window_with(b, "D", dialog());
    sim.expect_floating("D");
    let a = sim.launch_app("A");
    sim.open_window(a, "A1");
    sim.open_window(a, "A2");
    sim.run();
    sim.start_glide();
    sim.run();
    sim
}

#[test]
fn a_dialog_is_not_tiled() {
    Schedule::each(|schedule| {
        let sim = dialog_and_two_windows(schedule);
        assert_eq!(sim.tiled(1), ["A1", "A2"], "{schedule:?}");
        sim.scene().assert("A1").left_of("A2").same_size_as("A2");
        // Glide leaves the dialog with the size its app gave it.
        let dialog = sim.scene().frame("D");
        assert_eq!(dialog.size, CGSize::new(400., 300.), "{schedule:?}");
    });
}

#[test]
fn a_dialog_opened_after_glide_starts_is_not_tiled() {
    Schedule::each(|schedule| {
        let mut sim = two_windows(schedule);
        let b = sim.launch_app("B");
        sim.run();
        sim.expect_floating("D");
        sim.open_window_with(b, "D", dialog());
        sim.run();
        assert_eq!(sim.tiled(1), ["A1", "A2"], "{schedule:?}");
        sim.scene().assert("A1").left_of("A2").same_size_as("A2");
    });
}

#[test]
fn raising_a_dialog_gives_it_the_keyboard_focus() {
    Schedule::each(|schedule| {
        let mut sim = dialog_and_two_windows(schedule);
        sim.command(LayoutCommand::ToggleFocusFloating);
        sim.run();
        assert_eq!(sim.key_window().as_deref(), Some("D"), "{schedule:?}");
        assert_eq!(sim.tiled(1), ["A1", "A2"], "{schedule:?}");
    });
}

// TODO: Fails under every schedule. Glide doesn't wait for B to be activated
// before raising a dialog, so the raise only puts D in front of B's other
// windows and D stays behind A1 and A2. Check whether activating the app with
// a window brings that window forward on real macOS before treating this as a
// bug.
#[test]
#[ignore]
fn a_raised_dialog_comes_to_the_front() {
    Schedule::each(|schedule| {
        let mut sim = dialog_and_two_windows(schedule);
        sim.command(LayoutCommand::ToggleFocusFloating);
        sim.run();
        sim.scene().assert("D").in_front_of("A1").in_front_of("A2");
    });
}

#[test]
fn raising_a_standard_window_waits_for_its_app_to_activate() {
    Schedule::each(|schedule| {
        let mut sim = Harness::new(schedule, screen(), 1);
        let a = sim.launch_app("A");
        sim.open_window(a, "A1");
        let b = sim.launch_app("B");
        sim.open_window(b, "B1");
        sim.run();
        sim.start_glide();
        sim.run();
        assert_eq!(sim.key_window().as_deref(), Some("B1"), "{schedule:?}");
        // Glide can discover the two apps in either order.
        let scene = sim.scene();
        let direction = if scene.frame("A1").origin.x < scene.frame("B1").origin.x {
            Direction::Left
        } else {
            Direction::Right
        };
        sim.clear_actions();
        sim.command(LayoutCommand::MoveFocus(direction));
        sim.run();
        assert_eq!(sim.key_window().as_deref(), Some("A1"), "{schedule:?}");
        assert_eq!(sim.actions(), [Action::Raised("A1".into())], "{schedule:?}");
        // The raise brings A1 in front of other apps' windows only once A is
        // active.
        sim.scene().assert("A1").in_front_of("B1");
    });
}
