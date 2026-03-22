// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The SpaceManager sits between the WindowServer and the Reactor on the
//! reactor thread. It will eventually own space/screen enablement state that
//! currently lives in WmController; for now it is a passthrough.

use tracing::instrument;

use crate::actor::reactor;

#[derive(Debug)]
pub enum Event {
    /// Forwarded directly to the reactor.
    ReactorEvent(reactor::Event),
}

pub type Sender = crate::actor::Sender<Event>;
pub type Receiver = crate::actor::Receiver<Event>;

pub fn channel() -> (Sender, Receiver) {
    crate::actor::channel()
}

pub struct SpaceManager {
    reactor_tx: reactor::Sender,
}

impl SpaceManager {
    pub fn new(reactor_tx: reactor::Sender) -> Self {
        Self { reactor_tx }
    }

    pub async fn run(mut self, mut rx: Receiver) {
        while let Some((span, event)) = rx.recv().await {
            let _span = span.entered();
            self.on_event(event);
        }
    }

    #[instrument(skip(self))]
    fn on_event(&mut self, event: Event) {
        match event {
            Event::ReactorEvent(e) => self.reactor_tx.send(e),
        }
    }
}
