// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Raising windows from Glide commands while other things happen.

use std::time::Duration;

use super::{Action, Harness, Schedule, screen};
use crate::actor::layout::LayoutCommand;
use crate::model::Direction;
use crate::testing::sim::{FocusReport, Quiet};

/// Open each app with its windows, in order, then start Glide. The last app is
/// frontmost and its last window has the keyboard focus.
fn row(schedule: Schedule, apps: &[(&str, &[&str])]) -> Harness {
    let mut sim = Harness::new(schedule, screen(), 1);
    for &(name, windows) in apps {
        let pid = sim.launch_app(name);
        for window in windows {
            sim.open_window(pid, window);
        }
    }
    sim.run();
    sim.start_glide();
    sim.run();
    sim.clear_actions();
    sim
}

/// The tiled windows from left to right.
fn left_to_right(sim: &Harness) -> Vec<String> {
    let scene = sim.scene();
    let mut titles = sim.tiled(1);
    titles.sort_by(|a, b| {
        let (a, b) = (scene.frame(a.as_str()), scene.frame(b.as_str()));
        a.origin.x.total_cmp(&b.origin.x)
    });
    titles
}

/// Give Glide the focus commands that move the selection to `title`, without
/// letting anything run in between.
fn focus_command_to(sim: &mut Harness, title: &str) {
    let order = left_to_right(sim);
    let position = |title: &str| order.iter().position(|t| t == title).unwrap();
    let from = position(&sim.selected().expect("nothing is selected"));
    let to = position(title);
    let direction = if to < from {
        Direction::Left
    } else {
        Direction::Right
    };
    for _ in 0..from.abs_diff(to) {
        sim.command(LayoutCommand::MoveFocus(direction));
    }
}

/// The titles of the windows Glide raised, in order.
fn raised(sim: &Harness) -> Vec<String> {
    sim.actions()
        .into_iter()
        .filter_map(|action| match action {
            Action::Raised(title) => Some(title),
            Action::Wrote(_) => None,
        })
        .collect()
}

#[test]
fn focus_moves_to_a_window_of_a_background_app() {
    Schedule::each(|schedule| {
        let mut sim = row(schedule, &[("A", &["A1"]), ("B", &["B1"])]);
        assert_eq!(sim.key_window().as_deref(), Some("B1"), "{schedule:?}");

        focus_command_to(&mut sim, "A1");
        sim.run();
        assert_eq!(sim.key_window().as_deref(), Some("A1"), "{schedule:?}");
        assert_eq!(sim.selected().as_deref(), Some("A1"), "{schedule:?}");
        assert_eq!(raised(&sim), ["A1"], "{schedule:?}");
    });
}

const THREE_APPS: &[(&str, &[&str])] = &[("A", &["A1"]), ("B", &["B1"]), ("C", &["C1"])];

/// Take steps until the window server has given `title` the keyboard focus or
/// nothing is left to do.
fn step_until_key_window(sim: &mut Harness, title: &str) {
    while sim.key_window().as_deref() != Some(title) && sim.step() {}
}

/// The windows other than the one with the keyboard focus, left to right.
fn others(sim: &Harness) -> Vec<String> {
    let key = sim.key_window().unwrap();
    left_to_right(sim).into_iter().filter(|t| *t != key).collect()
}

/// Glide is told to focus a window of one app. The user clicks a window of
/// another app after the window server has given the first one the keyboard
/// focus and `steps` more steps have been taken.
fn click_after_focus_is_requested(schedule: Schedule, steps: usize) {
    let mut sim = row(schedule, THREE_APPS);
    let [clicked, commanded] = others(&sim).try_into().unwrap();

    focus_command_to(&mut sim, &commanded);
    step_until_key_window(&mut sim, &commanded);
    for _ in 0..steps {
        sim.step();
    }
    sim.click_window(&clicked);
    sim.run();
    let context = format!("{schedule:?} after {steps} steps");
    assert_eq!(sim.key_window().as_deref(), Some(&*clicked), "{context}");
    assert_eq!(sim.selected().as_deref(), Some(&*clicked), "{context}");
}

#[test]
fn user_activation_during_a_raise_wins() {
    for steps in [0, 1, 3, 6, 12, 24] {
        Schedule::sample(|schedule| click_after_focus_is_requested(schedule, steps));
    }
}

/// The user clicks a window of one app after Glide was told to focus another,
/// possibly before Glide asked macOS for the focus. The focus command may then
/// land after the click, so either window can end up with the focus, but Glide
/// must agree with macOS about which.
#[test]
fn click_racing_a_raise_leaves_glide_and_macos_agreeing() {
    for steps in [0, 1, 3, 6, 12, 24] {
        Schedule::sample(|schedule| {
            let mut sim = row(schedule, THREE_APPS);
            let [clicked, commanded] = others(&sim).try_into().unwrap();
            focus_command_to(&mut sim, &commanded);
            for _ in 0..steps {
                sim.step();
            }
            sim.click_window(&clicked);
            sim.run();
            let context = format!("{schedule:?} after {steps} steps");
            let key = sim.key_window().unwrap();
            assert!(key == clicked || key == commanded, "{context}: {key}");
            assert_eq!(sim.selected(), Some(key), "{context}");
        });
    }
}

const FOCUS_SEQUENCES: [&[Direction]; 4] = [
    &[Direction::Left, Direction::Left],
    &[Direction::Left, Direction::Right],
    &[Direction::Right, Direction::Left, Direction::Right],
    &[
        Direction::Left,
        Direction::Right,
        Direction::Left,
        Direction::Left,
    ],
];

/// Give Glide a focus command for each direction, letting the system take
/// `gap` steps after each, and check that the window the last one asked for
/// has the keyboard focus and is selected. Moving past either end does nothing.
fn focus_across_apps(schedule: Schedule, directions: &[Direction], gap: usize) {
    let mut sim = row(schedule, THREE_APPS);
    let order = left_to_right(&sim);
    let mut at = order.iter().position(|t| Some(t) == sim.key_window().as_ref()).unwrap();
    for &direction in directions {
        sim.command(LayoutCommand::MoveFocus(direction));
        at = match direction {
            Direction::Left => at.saturating_sub(1),
            _ => (at + 1).min(order.len() - 1),
        };
        for _ in 0..gap {
            sim.step();
        }
    }
    sim.run();
    let context = format!("{schedule:?} after {directions:?} with {gap} steps between");
    assert_eq!(sim.key_window().as_ref(), Some(&order[at]), "{context}");
    assert_eq!(sim.selected().as_ref(), Some(&order[at]), "{context}");
}

#[test]
fn raises_across_apps_end_on_the_last_one() {
    for directions in FOCUS_SEQUENCES {
        Schedule::sample(|schedule| focus_across_apps(schedule, directions, 0));
    }
}

#[test]
#[ignore]
// TODO: Fails under Seeded(17), which needs `GLIDE_SIM_SEED_FACTOR=3`, for
// [Right, Left, Right] with 15 steps between commands. The focus raise for the first command is reported as a user focus
// change, and it reaches the reactor after the second command has moved the
// selection on. It selects the first window again, so the third command starts
// from the wrong window and the last window requested never gets the focus.
// Fix by moving from the window Glide is focusing while commands come quickly,
// and from the window with the keyboard focus once the two have disagreed for
// long enough that the user can see it.
fn raises_across_apps_with_time_between_end_on_the_last_one() {
    for directions in FOCUS_SEQUENCES {
        for gap in [1, 3, 7, 15, 40] {
            Schedule::sample(|schedule| focus_across_apps(schedule, directions, gap));
        }
    }
}

/// Raising the tiled windows when focus leaves a floating window asks app A to
/// raise A1 and A2 in one request, with the selected one last.
#[test]
fn only_the_last_window_of_a_raise_is_reported() {
    for selected in ["A1", "A2"] {
        Schedule::each(|schedule| {
            let mut sim = row(schedule, &[("A", &["A1", "A2"]), ("B", &["B1", "B2"])]);
            // Float B2, the window with the keyboard focus, then select a tiled
            // window of app A.
            sim.expect_floating("B2");
            sim.command(LayoutCommand::ToggleWindowFloating);
            sim.run();
            sim.click_window(selected);
            sim.run();
            // Focus the floating window, then go back to the tiled windows.
            sim.command(LayoutCommand::ToggleFocusFloating);
            sim.run();
            assert_eq!(sim.key_window().as_deref(), Some("B2"), "{schedule:?}");
            sim.clear_actions();
            let reports = sim.focus_reports_from_app_actors().len();
            sim.command(LayoutCommand::ToggleFocusFloating);
            sim.run();

            let context = format!("{schedule:?} selecting {selected}");
            assert_eq!(sim.key_window().as_deref(), Some(selected), "{context}");
            assert_eq!(sim.selected().as_deref(), Some(selected), "{context}");
            let raised = raised(&sim);
            let a_raises: Vec<_> = raised.iter().filter(|t| t.starts_with('A')).collect();
            assert_eq!(a_raises.len(), 2, "{context}: {raised:?}");
            assert_eq!(a_raises[1], selected, "{context}: {raised:?}");

            // Only the selected window's change is reported as the user's. The
            // two apps' actors report in either order.
            let other = if selected == "A1" { "A2" } else { "A1" };
            let reports = &sim.focus_reports_from_app_actors()[reports..];
            let (from_a, from_b): (Vec<_>, Vec<_>) =
                reports.iter().cloned().partition(|report| match report {
                    FocusReport::App(name, _) | FocusReport::Window(name, _) => {
                        name.starts_with('A')
                    }
                });
            assert_eq!(
                from_b,
                [FocusReport::Window("B1".into(), Quiet::Yes)],
                "{context}"
            );
            assert_eq!(
                from_a,
                [
                    FocusReport::App("A".into(), Quiet::Yes),
                    FocusReport::Window(other.into(), Quiet::Yes),
                    FocusReport::Window(selected.into(), Quiet::No),
                ],
                "{context}"
            );
        });
    }
}

#[test]
fn an_activation_after_its_deadline_is_not_quiet() {
    Schedule::each(|schedule| {
        let mut sim = row(schedule, &[("A", &["A1", "A2"]), ("B", &["B1", "B2"])]);
        sim.expect_floating("B2");
        sim.command(LayoutCommand::ToggleWindowFloating);
        sim.run();
        sim.click_window("A2");
        sim.run();
        sim.command(LayoutCommand::ToggleFocusFloating);
        sim.run();
        let reports = sim.focus_reports_from_app_actors().len();

        // A is slow enough that A's activation comes after Glide has given up
        // on the raise, but not so slow that Glide gives up before asking for
        // it. Depending on the schedule, Glide gives up before or after A has
        // raised A1 and A2.
        let a = sim.app("A");
        sim.set_app_delay(a, Duration::from_millis(75));
        sim.command(LayoutCommand::ToggleFocusFloating);
        sim.run();

        assert_eq!(sim.key_window().as_deref(), Some("A2"), "{schedule:?}");
        let reports = &sim.focus_reports_from_app_actors()[reports..];
        let activated = [
            FocusReport::Window("B1".into(), Quiet::Yes),
            FocusReport::App("A".into(), Quiet::No),
        ];
        let raised = [
            FocusReport::Window("A1".into(), Quiet::Yes),
            FocusReport::Window("A2".into(), Quiet::No),
        ];
        assert_eq!(reports[..2], activated, "{schedule:?}");
        assert!(
            reports[2..].is_empty() || reports[2..] == raised,
            "{schedule:?}: {reports:?}"
        );
    });
}

#[test]
fn the_user_activating_an_app_after_a_failed_activation_moves_the_selection() {
    Schedule::each(|schedule| {
        let mut sim = row(schedule, &[("A", &["A1", "A2"]), ("B", &["B1", "B2"])]);
        sim.expect_floating("B2");
        sim.command(LayoutCommand::ToggleWindowFloating);
        sim.run();
        sim.click_window("A2");
        sim.run();
        sim.command(LayoutCommand::ToggleFocusFloating);
        sim.run();

        // Glide raises A1 and A2 to go back to the tiled windows, but A never
        // becomes active.
        let a = sim.app("A");
        sim.set_ignores_make_key(a, true);
        sim.command(LayoutCommand::ToggleFocusFloating);
        // Glide gives up on the raise. Its selection and the keyboard focus
        // disagree until the user acts, so don't check the invariants yet.
        sim.sim.run();
        sim.set_ignores_make_key(a, false);
        // The user clicks once A's activation would no longer count as part of
        // the raise.
        sim.sim.wait(Duration::from_secs(2));

        sim.click_window("A1");
        sim.run();
        assert_eq!(sim.key_window().as_deref(), Some("A1"), "{schedule:?}");
        assert_eq!(sim.selected().as_deref(), Some("A1"), "{schedule:?}");
    });
}

#[test]
fn raise_of_a_closing_window_settles() {
    for steps in [0, 1, 3, 6, 12, 24, 48] {
        Schedule::sample(|schedule| {
            let mut sim = row(schedule, THREE_APPS);
            let [_, commanded] = others(&sim).try_into().unwrap();
            focus_command_to(&mut sim, &commanded);
            for _ in 0..steps {
                sim.step();
            }
            sim.close_window(&commanded);
            sim.run();
            let context = format!("{schedule:?} after {steps} steps");
            assert!(!sim.tiled(1).contains(&commanded), "{context}");
            let selected = sim.selected();
            assert!(
                selected.is_some() && selected.as_ref() != Some(&commanded),
                "{context}"
            );
        });
    }
}
