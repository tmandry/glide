// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Actors in the window manager.
//!
//! Each actor manages some important resource, like an external application or
//! the layout state. The flow of events between these actors defines the
//! overall behavior of the window manager.

use tokio::sync::mpsc::error::SendError;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tracing::Span;

pub mod app;
pub mod dock;
pub mod group_bars;
pub mod layout;
pub mod mouse;
pub mod notification_center;
pub mod raise;
pub mod reactor;
pub mod server;
pub mod space_manager;
pub mod status;
pub mod window_server;
pub mod wm_controller;

pub struct Sender<Event>(UnboundedSender<(Span, Event)>);
pub type Receiver<Event> = UnboundedReceiver<(Span, Event)>;

pub fn channel<Event>() -> (Sender<Event>, Receiver<Event>) {
    let (tx, rx) = unbounded_channel();
    (Sender(tx), rx)
}

impl<Event> Sender<Event> {
    pub fn send(&self, event: Event) {
        // Most of the time we can ignore send errors, they just indicate the
        // app is shutting down.
        _ = self.try_send(event)
    }

    pub fn try_send(&self, event: Event) -> Result<(), SendError<(Span, Event)>> {
        self.0.send((Span::current(), event))
    }
}

impl<Event> Clone for Sender<Event> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
