// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::BTreeSet;
use std::ops::{Deref, DerefMut};

use objc2_core_foundation::CGRect;

use crate::actor::layout::LayoutCommand;
use crate::testing::sim::{Schedule, Sim};

/// A simulation that checks Glide's invariants whenever it settles.
pub(super) struct Harness {
    pub(super) sim: Sim,
    /// The windows that may not take the frame Glide gives them.
    pub(super) frame_mismatch_allowed: BTreeSet<String>,
    /// The windows Glide is expected to leave floating.
    pub(super) floating: BTreeSet<String>,
    /// How many inputs the user had given when the system last settled.
    pub(super) inputs_at_settle: usize,
    /// The length of the selection history when a focus command was the only
    /// input since the system last settled.
    pub(super) focus_command: Option<usize>,
}

impl Harness {
    pub(super) fn new(schedule: Schedule, screen: CGRect, desktops: usize) -> Harness {
        Harness {
            sim: Sim::new(schedule, screen, desktops),
            frame_mismatch_allowed: BTreeSet::new(),
            floating: BTreeSet::new(),
            inputs_at_settle: 0,
            focus_command: None,
        }
    }

    /// Deliver messages until every inbox is empty, then check what must hold
    /// of Glide in any settled system.
    pub(super) fn run(&mut self) {
        let first_action = self.sim.mac.borrow().actions().len();
        self.sim.run();
        if self.sim.glide_running {
            self.check_invariants(first_action);
        }
        self.inputs_at_settle = self.sim.inputs();
        self.focus_command = None;
    }

    /// Give Glide a layout command, as a key binding would.
    pub(super) fn command(&mut self, command: LayoutCommand) {
        use LayoutCommand::*;
        let moves_focus = matches!(
            command,
            MoveFocus(_) | FocusNext | FocusPrev | ToggleFocusFloating
        );
        let only_input = self.sim.inputs() == self.inputs_at_settle;
        self.sim.command(command);
        self.focus_command =
            (moves_focus && only_input).then(|| self.sim.selection_history().len());
    }

    /// Say that Glide leaves the window floating. The invariants about tiled
    /// windows don't apply to it, and the scenario checks it isn't tiled.
    pub(super) fn expect_floating(&mut self, title: &str) {
        self.floating.insert(title.to_owned());
    }

    /// Say that the window may not take the frame Glide gives it, because
    /// its app adjusts frames. The invariants about tiled windows fitting the
    /// screen without overlapping don't apply to it.
    pub(super) fn allow_frame_mismatch(&mut self, title: &str) {
        self.frame_mismatch_allowed.insert(title.to_owned());
    }
}

impl Deref for Harness {
    type Target = Sim;

    fn deref(&self) -> &Sim {
        &self.sim
    }
}

impl DerefMut for Harness {
    fn deref_mut(&mut self) -> &mut Sim {
        &mut self.sim
    }
}
