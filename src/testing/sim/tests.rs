// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

use expect_test::expect;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use test_log::test;

use super::{Action, Schedule, Sim};
use crate::sys::app::pid_t;

fn screen() -> CGRect {
    CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))
}

const SCHEDULES: [Schedule; 2] = [Schedule::GlideFirst, Schedule::MacFirst];

/// App A has windows A1 and A2 on desktop 1, and app B has B1 and B2 on
/// desktop 2. Desktop 1 is shown and Glide is running.
struct TwoDesktops {
    sim: Sim,
    b: pid_t,
}

impl TwoDesktops {
    fn new(schedule: Schedule) -> TwoDesktops {
        let mut sim = Sim::new(schedule, screen(), 2);
        let a = sim.launch_app("A");
        sim.open_window(a, "A1");
        sim.open_window(a, "A2");
        sim.switch_desktop(2);
        let b = sim.launch_app("B");
        sim.open_window(b, "B1");
        sim.open_window(b, "B2");
        sim.switch_desktop(1);
        sim.run();
        sim.start_glide();
        sim.run();
        sim.clear_actions();
        TwoDesktops { sim, b }
    }
}

#[test]
fn windows_are_tiled_on_the_desktop_they_are_on() {
    for schedule in SCHEDULES {
        let TwoDesktops { mut sim, .. } = TwoDesktops::new(schedule);
        assert_eq!(sim.tiled(1), ["A1", "A2"], "{schedule:?}");
        sim.scene().assert("A1").left_of("A2").same_size_as("A2");

        sim.switch_desktop(2);
        sim.run();
        assert_eq!(sim.tiled(1), ["A1", "A2"], "{schedule:?}");
        assert_eq!(sim.tiled(2), ["B1", "B2"], "{schedule:?}");
        // Glide can discover B1 and B2 in either order.
        sim.scene().assert("B1").same_size_as("B2");
    }
}

#[test]
fn a_new_window_is_tiled_next_to_the_others() {
    for schedule in SCHEDULES {
        let mut sim = Sim::new(schedule, screen(), 1);
        let a = sim.launch_app("A");
        sim.open_window(a, "A1");
        sim.run();
        sim.start_glide();
        sim.run();
        sim.scene().assert("A1").fills_screen();

        sim.open_window(a, "A2");
        sim.run();
        expect![[r#"
            ┌───────────┬───────────┐
            │ A1        │ A2        │
            │           │           │
            │           │           │
            │           │           │
            │           │           │
            │           │           │
            │           │           │
            │           │           │
            │           │           │
            │           │           │
            │           │           │
            └───────────┴───────────┘
            front to back: A2, A1
        "#]]
        .assert_eq(&sim.scene().draw());
    }
}

#[test]
fn glide_leaves_windows_alone_while_mission_control_is_open() {
    for schedule in SCHEDULES {
        let TwoDesktops { mut sim, .. } = TwoDesktops::new(schedule);
        sim.open_mission_control();
        sim.run();
        sim.switch_desktop(2);
        sim.run();
        sim.close_window("A2");
        sim.run();
        sim.switch_desktop(1);
        sim.run();
        assert_eq!(sim.actions_during_mission_control(), [], "{schedule:?}");

        sim.close_mission_control();
        sim.run();
        assert_eq!(sim.tiled(1), ["A1"], "{schedule:?}");
        sim.scene().assert("A1").fills_screen();
        assert_eq!(sim.current_desktop(), 1, "{schedule:?}");
    }
}

#[test]
fn leaving_mission_control_onto_another_desktop() {
    for schedule in SCHEDULES {
        let TwoDesktops { mut sim, .. } = TwoDesktops::new(schedule);
        sim.open_mission_control();
        sim.run();
        sim.close_window("A2");
        sim.switch_desktop(2);
        sim.run();
        sim.close_mission_control();
        sim.run();
        assert_eq!(sim.actions_during_mission_control(), [], "{schedule:?}");
        assert_eq!(sim.current_desktop(), 2, "{schedule:?}");
        assert_eq!(sim.tiled(2), ["B1", "B2"], "{schedule:?}");
        assert!(
            !sim.actions().contains(&Action::Wrote("A1".into())),
            "{schedule:?}: A1 isn't on screen: {:?}",
            sim.actions()
        );

        sim.switch_desktop(1);
        sim.run();
        assert_eq!(sim.tiled(1), ["A1"], "{schedule:?}");
        sim.scene().assert("A1").fills_screen();
    }
}

/// Mission Control shows desktop 2 and app B opens B3 there. Mission Control
/// is then closed onto desktop 1.
fn window_created_in_mission_control(schedule: Schedule) {
    let TwoDesktops { mut sim, b } = TwoDesktops::new(schedule);
    sim.open_mission_control();
    sim.run();
    sim.switch_desktop(2);
    sim.run();
    sim.open_window(b, "B3");
    sim.run();
    sim.switch_desktop(1);
    sim.run();
    sim.close_mission_control();
    sim.run();

    assert_eq!(sim.actions_during_mission_control(), [], "{schedule:?}");
    assert_eq!(sim.tiled(1), ["A1", "A2"], "{schedule:?}");
    assert_eq!(sim.current_desktop(), 1, "{schedule:?}");

    sim.switch_desktop(2);
    sim.run();
    assert_eq!(sim.tiled(2), ["B1", "B2", "B3"], "{schedule:?}");
}

#[test]
fn window_created_in_mission_control_joins_its_desktop_when_placed_first() {
    window_created_in_mission_control(Schedule::MacFirst);
}

#[test]
fn window_created_in_mission_control_joins_its_desktop_when_seen_before_placed() {
    window_created_in_mission_control(Schedule::GlideFirst);
}

#[test]
fn window_created_in_mission_control_joins_its_desktop_in_any_order() {
    for seed in 1..=200 {
        window_created_in_mission_control(Schedule::Seeded(seed));
    }
}

/// A2 has the keyboard focus and is dragged to desktop 2 in Mission Control,
/// which is then closed onto desktop 1.
fn focused_window_dragged_to_another_desktop(schedule: Schedule) {
    let TwoDesktops { mut sim, .. } = TwoDesktops::new(schedule);
    sim.open_mission_control();
    sim.run();
    sim.drag_window_to_desktop("A2", 2);
    sim.run();
    sim.close_mission_control();
    sim.run();
    assert_eq!(sim.actions_during_mission_control(), [], "{schedule:?}");
    assert_eq!(sim.current_desktop(), 1, "{schedule:?}");
    assert_eq!(sim.tiled(1), ["A1"], "{schedule:?}");
    sim.scene().assert("A1").fills_screen();

    sim.switch_desktop(2);
    sim.run();
    assert_eq!(sim.tiled(2), ["A2", "B1", "B2"], "{schedule:?}");
}

#[test]
fn focused_window_dragged_to_another_desktop_in_mission_control() {
    for schedule in SCHEDULES {
        focused_window_dragged_to_another_desktop(schedule);
    }
}
