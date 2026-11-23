// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

use expect_test::expect;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use test_log::test;

use self::harness::Harness;
use super::{Action, Schedule};
use crate::actor::layout::LayoutCommand;
use crate::model::Direction;
use crate::sys::app::pid_t;

mod apps;
mod frames;
mod harness;
mod invariants;
mod lifecycle;
mod raises;
mod slow;

fn screen() -> CGRect {
    CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))
}

/// App A has windows A1 and A2 on desktop 1, and app B has B1 and B2 on
/// desktop 2. Desktop 1 is shown and Glide is running.
struct TwoDesktops {
    sim: Harness,
    a: pid_t,
    b: pid_t,
}

impl TwoDesktops {
    fn new(schedule: Schedule) -> TwoDesktops {
        let mut sim = Harness::new(schedule, screen(), 2);
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
        TwoDesktops { sim, a, b }
    }
}

#[test]
fn windows_are_tiled_on_the_desktop_they_are_on() {
    Schedule::each(|schedule| {
        let TwoDesktops { mut sim, .. } = TwoDesktops::new(schedule);
        assert_eq!(sim.tiled(1), ["A1", "A2"], "{schedule:?}");
        sim.scene().assert("A1").left_of("A2").same_size_as("A2");

        sim.switch_desktop(2);
        sim.run();
        assert_eq!(sim.tiled(1), ["A1", "A2"], "{schedule:?}");
        assert_eq!(sim.tiled(2), ["B1", "B2"], "{schedule:?}");
        // Glide can discover B1 and B2 in either order.
        sim.scene().assert("B1").same_size_as("B2");
    });
}

#[test]
fn a_new_window_is_tiled_next_to_the_others() {
    Schedule::each(|schedule| {
        let mut sim = Harness::new(schedule, screen(), 1);
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
    });
}

#[test]
fn glide_leaves_windows_alone_while_mission_control_is_open() {
    Schedule::each(|schedule| {
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
    });
}

#[test]
fn leaving_mission_control_onto_another_desktop() {
    Schedule::each(|schedule| {
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
    });
}

/// Mission Control shows desktop 2 and app B opens B3 there. Mission Control
/// is then closed onto desktop 1.
fn window_created_in_mission_control(schedule: Schedule) {
    let TwoDesktops { mut sim, b, .. } = TwoDesktops::new(schedule);
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
    Schedule::each(window_created_in_mission_control);
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
    Schedule::each(focused_window_dragged_to_another_desktop);
}

#[test]
fn a_focus_command_gives_the_window_the_keyboard_focus() {
    Schedule::each(|schedule| {
        let TwoDesktops { mut sim, .. } = TwoDesktops::new(schedule);
        assert_eq!(sim.key_window().as_deref(), Some("A2"), "{schedule:?}");

        sim.command(LayoutCommand::MoveFocus(Direction::Left));
        sim.run();
        assert_eq!(sim.key_window().as_deref(), Some("A1"), "{schedule:?}");
        assert_eq!(sim.selected().as_deref(), Some("A1"), "{schedule:?}");
        assert_eq!(sim.actions(), [Action::Raised("A1".into())], "{schedule:?}");
    });
}

#[test]
fn glide_selects_the_window_the_user_clicks() {
    Schedule::each(|schedule| {
        let TwoDesktops { mut sim, .. } = TwoDesktops::new(schedule);
        sim.click_window("A1");
        sim.run();
        assert_eq!(sim.selected().as_deref(), Some("A1"), "{schedule:?}");
        assert_eq!(sim.actions(), [], "{schedule:?}");
    });
}

#[test]
fn a_window_moved_with_no_drag_is_put_back() {
    Schedule::each(|schedule| {
        let TwoDesktops { mut sim, .. } = TwoDesktops::new(schedule);
        let before = sim.scene().draw();
        let moved = CGRect::new(CGPoint::new(40., 60.), CGSize::new(500., 1000.));
        sim.set_window_frame("A1", moved);
        sim.run();
        assert_eq!(sim.scene().draw(), before, "{schedule:?}");
    });
}

#[test]
fn another_app_launching_does_not_disturb_the_layout() {
    Schedule::each(|schedule| {
        let mut sim = Harness::new(schedule, screen(), 1);
        let a = sim.launch_app("A");
        sim.open_window(a, "A1");
        sim.open_window(a, "A2");
        sim.run();
        sim.start_glide();
        sim.run();

        let b = sim.launch_app("B");
        sim.run();
        sim.open_window(b, "B1");
        sim.open_window(b, "B2");
        sim.run();
        assert_eq!(sim.tiled(1), ["A1", "A2", "B1", "B2"], "{schedule:?}");
        sim.scene()
            .assert("A1")
            .same_size_as("A2")
            .same_size_as("B1")
            .same_size_as("B2");
    });
}

#[test]
fn the_layout_returns_after_another_app_window_comes_and_goes() {
    Schedule::each(|schedule| {
        let mut sim = Harness::new(schedule, screen(), 1);
        let a = sim.launch_app("A");
        sim.open_window(a, "A1");
        sim.open_window(a, "A2");
        let b = sim.launch_app("B");
        sim.run();
        sim.click_window("A2");
        sim.run();
        sim.start_glide();
        sim.run();
        let before = sim.scene().draw();

        sim.open_window(b, "B1");
        sim.run();
        sim.close_window("B1");
        sim.run();
        sim.click_window("A2");
        sim.run();
        assert_eq!(sim.scene().draw(), before, "{schedule:?}");
    });
}

#[test]
fn switching_desktops_raises_nothing_when_the_order_already_matches() {
    Schedule::each(|schedule| {
        let TwoDesktops { mut sim, .. } = TwoDesktops::new(schedule);
        sim.switch_desktop(2);
        sim.run();
        sim.switch_desktop(1);
        sim.run();
        let raised = |a: &Action| matches!(a, Action::Raised(_));
        assert!(
            !sim.actions().iter().any(raised),
            "{schedule:?}: {:?}",
            sim.actions()
        );
    });
}

// TODO: Fails once A1 is minimized, as in `apps::minimize_and_restore`. See
// https://github.com/tmandry/glide/issues/209.
#[test]
#[ignore]
fn restoring_a_window_shows_its_desktop() {
    Schedule::each(|schedule| {
        let TwoDesktops { mut sim, .. } = TwoDesktops::new(schedule);
        sim.minimize_window("A1");
        sim.run();
        sim.switch_desktop(2);
        sim.run();

        sim.restore_window("A1");
        sim.run();
        assert_eq!(sim.current_desktop(), 1, "{schedule:?}");
        assert_eq!(sim.key_window().as_deref(), Some("A1"), "{schedule:?}");
        assert_eq!(sim.selected().as_deref(), Some("A1"), "{schedule:?}");
    });
}
