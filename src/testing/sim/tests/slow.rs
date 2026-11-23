// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Scenarios where macOS or an app takes a long time.

use std::time::Duration;

use super::{Harness, screen};
use crate::actor::layout::LayoutCommand;
use crate::model::Direction;
use crate::sys::app::pid_t;
use crate::testing::sim::Schedule;

/// Apps A and B have one window each, side by side. B1 has the keyboard
/// focus and Glide is running.
fn two_apps(schedule: Schedule) -> (Harness, pid_t, pid_t) {
    let mut sim = Harness::new(schedule, screen(), 1);
    let a = sim.launch_app("A");
    sim.open_window(a, "A1");
    let b = sim.launch_app("B");
    sim.open_window(b, "B1");
    sim.run();
    sim.start_glide();
    sim.run();
    sim.clear_actions();
    (sim, a, b)
}

/// The focus command that moves from B1 to A1.
fn focus_a1(sim: &Harness) -> LayoutCommand {
    let a1_is_left = sim.scene().frame("A1").origin.x < sim.scene().frame("B1").origin.x;
    LayoutCommand::MoveFocus(if a1_is_left {
        Direction::Left
    } else {
        Direction::Right
    })
}

#[test]
fn focus_moves_to_a_slow_app() {
    Schedule::each(|schedule| {
        let (mut sim, a, _) = two_apps(schedule);
        // Glide gives up on raising A1 before A gets to it.
        sim.set_app_delay(a, Duration::from_secs(1));
        let command = focus_a1(&sim);
        sim.command(command);
        sim.run();

        assert_eq!(sim.key_window().as_deref(), Some("A1"), "{schedule:?}");
        assert_eq!(sim.selected().as_deref(), Some("A1"), "{schedule:?}");
    });
}

#[test]
fn a_hung_app_does_not_hold_up_other_apps() {
    Schedule::each(|schedule| {
        let (mut sim, a, b) = two_apps(schedule);
        sim.set_app_hung(a, true);
        sim.open_window(b, "B2");
        sim.run();
        assert_eq!(sim.tiled(1), ["A1", "B1", "B2"], "{schedule:?}");
        sim.scene().assert("B1").same_size_as("B2");
        let third = screen().size.width / 3.0;
        assert!(
            (sim.scene().frame("B2").size.width - third).abs() < 1.0,
            "{schedule:?}"
        );
    });
}

// TODO: Glide doesn't write A1's frame again after the write to the hung app
// timed out, so A1 keeps its old frame under every schedule. The simulated
// app drops a request that timed out. Check whether a real app still applies
// it once it responds again before treating this as a bug.
#[test]
#[ignore]
fn layout_recovers_after_an_app_stops_hanging() {
    Schedule::each(|schedule| {
        let (mut sim, a, b) = two_apps(schedule);
        sim.set_app_hung(a, true);
        sim.open_window(b, "B2");
        sim.run();
        sim.set_app_hung(a, false);
        sim.run();
        sim.scene().assert("A1").same_size_as("B1").same_size_as("B2");
    });
}
