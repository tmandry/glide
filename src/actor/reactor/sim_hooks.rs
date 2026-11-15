// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Access to the reactor for the simulated system in [`crate::sim`].

use super::{Event, Reactor};
use crate::actor::raise;
use crate::sys::screen::SpaceId;
use crate::sys::window_server::WindowId;

impl Reactor {
    /// Handle one event, as the reactor's run loop does.
    pub(crate) fn step(&mut self, event: Event) {
        self.handle_event(event);
    }

    /// Send raise requests to `tx` instead of dropping them.
    pub(crate) fn set_raise_manager(&mut self, tx: raise::Sender) {
        self.raise_manager_tx = tx;
    }

    /// Return the windows tiled on `space`.
    pub(crate) fn tiled_windows(&self, space: SpaceId) -> Vec<WindowId> {
        self.layout.tiled_windows(space)
    }
}
