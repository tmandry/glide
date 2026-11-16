// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::ops::{Deref, DerefMut};

use objc2_core_foundation::CGRect;

use crate::testing::sim::{Schedule, Sim};

/// A simulation that checks Glide's invariants whenever it settles.
pub(super) struct Harness {
    pub(super) sim: Sim,
}

impl Harness {
    pub(super) fn new(schedule: Schedule, screen: CGRect, desktops: usize) -> Harness {
        Harness {
            sim: Sim::new(schedule, screen, desktops),
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
