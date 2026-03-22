// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The SpaceManager manages which spaces are enabled and acts as a filter
//! for SpaceChanged events before they get to the Reactor, removing disabled
//! SpaceIds so that the Reactor does not manage them.

use std::sync::Arc;

use objc2_core_foundation::CGRect;
use tracing::{debug, info, instrument, warn};

use crate::actor::wm_controller::WmEvent;
use crate::actor::{group_bars, mouse, reactor, status, window_server, wm_controller};
use crate::collections::HashSet;
use crate::config::Config;
use crate::sys::screen::{CoordinateConverter, ScreenId, SpaceId};
use crate::sys::window_server::WindowsOnScreen;

#[derive(Debug)]
pub enum Event {
    // From WindowServer
    ScreenParametersChanged {
        screens: Vec<ScreenId>,
        frames: Vec<CGRect>,
        spaces: Vec<Option<SpaceId>>,
        scale_factors: Vec<f64>,
        converter: CoordinateConverter,
        on_screen: WindowsOnScreen,
    },
    SpaceChanged(Vec<Option<SpaceId>>, WindowsOnScreen),
    /// Forwarded directly to the reactor.
    ReactorEvent(reactor::Event),

    // From WmController
    ToggleSpace(ScreenId),
    ToggleGlobalEnabled,
    LoginWindowActive(bool),
    ExposeActive(bool),
    ReactorCommand(reactor::Command),
    ConfigUpdated(Arc<Config>),
}

pub type Sender = crate::actor::Sender<Event>;
pub type Receiver = crate::actor::Receiver<Event>;

pub fn channel() -> (Sender, Receiver) {
    crate::actor::channel()
}

pub struct SpaceManager {
    one_space: bool,
    config: Arc<Config>,
    reactor_tx: reactor::Sender,
    ws_tx: window_server::Sender,
    wm_tx: wm_controller::Sender,
    status_tx: status::Sender,
    group_indicators_tx: group_bars::Sender,
    mouse_tx: mouse::Sender,
    starting_space: Option<SpaceId>,
    cur_space: Vec<Option<SpaceId>>,
    cur_screen_id: Vec<ScreenId>,
    disabled_spaces: HashSet<SpaceId>,
    enabled_spaces: HashSet<SpaceId>,
    login_window_active: bool,
    expose_active: bool,
    is_globally_enabled: bool,
    hotkeys_active: bool,
}

impl SpaceManager {
    #[expect(clippy::too_many_arguments)]
    pub fn new(
        one_space: bool,
        config: Arc<Config>,
        reactor_tx: reactor::Sender,
        ws_tx: window_server::Sender,
        wm_tx: wm_controller::Sender,
        status_tx: status::Sender,
        group_indicators_tx: group_bars::Sender,
        mouse_tx: mouse::Sender,
    ) -> Self {
        let is_globally_enabled = true;
        status_tx.send(status::Event::GlobalEnabledChanged(is_globally_enabled));
        Self {
            one_space,
            config,
            reactor_tx,
            ws_tx,
            wm_tx,
            status_tx,
            group_indicators_tx,
            mouse_tx,
            starting_space: None,
            cur_space: Vec::new(),
            cur_screen_id: Vec::new(),
            disabled_spaces: HashSet::default(),
            enabled_spaces: HashSet::default(),
            login_window_active: false,
            expose_active: false,
            is_globally_enabled,
            hotkeys_active: false,
        }
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
            Event::ScreenParametersChanged {
                screens: ids,
                frames,
                scale_factors,
                spaces,
                converter,
                on_screen,
            } => {
                self.cur_screen_id = ids;
                self.handle_space_changed(&spaces);
                self.reactor_tx.send(reactor::Event::ScreenParametersChanged {
                    frames: frames.clone(),
                    spaces: self.active_spaces(),
                    converter,
                    scale_factors,
                });
                self.reactor_tx
                    .send(reactor::Event::WindowsOnScreenUpdated { pid: None, on_screen });
                self.status_tx.send(status::Event::SpaceChanged(spaces));
                self.send_space_enabled_status();
                self.mouse_tx.send(mouse::Request::ScreenParametersChanged(frames, converter));
            }
            Event::SpaceChanged(spaces, on_screen) => {
                self.handle_space_changed(&spaces);
                if !self.expose_active {
                    self.reactor_tx
                        .send(reactor::Event::SpaceChanged(self.active_spaces(), on_screen));
                }
                self.status_tx.send(status::Event::SpaceChanged(spaces));
                self.send_space_enabled_status();
            }
            Event::ReactorEvent(e) => self.reactor_tx.send(e),
            Event::ToggleSpace(screen_id) => {
                let Some(space) = self.space_for_screen(screen_id) else {
                    return;
                };
                let toggle_set = if self.config.settings.default_disable {
                    &mut self.enabled_spaces
                } else {
                    &mut self.disabled_spaces
                };
                if !toggle_set.remove(&space) {
                    toggle_set.insert(space);
                }
                if !self.is_space_enabled(space) {
                    self.group_indicators_tx.send(group_bars::Event::SpaceDisabled(space));
                }
                self.status_tx
                    .send(status::Event::SpaceEnabledChanged(self.is_space_enabled(space)));
                self.request_space_refresh();
            }
            Event::ToggleGlobalEnabled => {
                self.is_globally_enabled = !self.is_globally_enabled;
                if !self.is_globally_enabled {
                    self.group_indicators_tx.send(group_bars::Event::GlobalDisabled);
                }
                self.status_tx
                    .send(status::Event::GlobalEnabledChanged(self.is_globally_enabled));
                self.send_space_enabled_status();
                self.request_space_refresh();
            }
            Event::LoginWindowActive(active) => {
                if active {
                    info!("Login window activated");
                } else {
                    info!("Login window deactivated");
                }
                self.login_window_active = active;
                self.request_space_refresh();
            }
            Event::ExposeActive(active) => {
                self.expose_active = active;
                if !active {
                    // Expose exited: request a space refresh so the reactor
                    // gets up-to-date visible windows.
                    self.request_space_refresh();
                }
            }
            Event::ReactorCommand(cmd) => {
                self.reactor_tx.send(reactor::Event::Command(cmd));
            }
            Event::ConfigUpdated(config) => {
                self.config = config.clone();
                self.reactor_tx.send(reactor::Event::ConfigChanged(config));
                // Force-send hotkey state since WmController unconditionally
                // unregisters hotkeys on config reload.
                self.send_hotkey_state();
                self.send_space_enabled_status();
                self.request_space_refresh();
            }
        }
    }

    fn handle_space_changed(&mut self, spaces: &[Option<SpaceId>]) {
        self.cur_space = spaces.to_vec();
        if self.starting_space.is_none() {
            self.starting_space = self.first_space();
        }
        self.update_hotkey_state();
    }

    fn first_space(&self) -> Option<SpaceId> {
        self.cur_space.first().copied().flatten()
    }

    fn space_for_screen(&self, screen_id: ScreenId) -> Option<SpaceId> {
        self.cur_screen_id
            .iter()
            .zip(&self.cur_space)
            .find(|(id, _)| **id == screen_id)
            .and_then(|(_, space)| *space)
    }

    fn is_space_enabled(&self, space: SpaceId) -> bool {
        match space {
            sp if self.config.settings.default_disable => self.enabled_spaces.contains(&sp),
            sp => !self.disabled_spaces.contains(&sp),
        }
    }

    fn active_spaces(&self) -> Vec<Option<SpaceId>> {
        if !self.is_globally_enabled {
            return vec![None; self.cur_space.len()];
        }
        let mut spaces = self.cur_space.clone();
        for space in &mut spaces {
            let enabled = match space {
                _ if self.login_window_active => false,
                Some(_) if self.one_space && *space != self.starting_space => false,
                Some(sp) if self.disabled_spaces.contains(sp) => false,
                Some(sp) if self.enabled_spaces.contains(sp) => true,
                _ if self.config.settings.default_disable => false,
                _ => true,
            };
            if !enabled {
                *space = None;
            }
        }
        spaces
    }

    /// Send a `SpaceEnabledChanged` status update using the first screen's
    /// space as a best-effort approximation of the focused screen.
    fn send_space_enabled_status(&self) {
        let enabled = self.first_space().map(|s| self.is_space_enabled(s)).unwrap_or(false);
        self.status_tx.send(status::Event::SpaceEnabledChanged(enabled));
    }

    fn request_space_refresh(&self) {
        self.ws_tx.send(window_server::Request::RequestSpaceRefresh);
    }

    fn update_hotkey_state(&mut self) {
        let all_spaces = !self.one_space;
        let active = self.starting_space.is_some()
            && (all_spaces || self.starting_space == self.first_space());
        if active != self.hotkeys_active {
            self.hotkeys_active = active;
            self.send_hotkey_state();
        }
    }

    fn send_hotkey_state(&self) {
        debug!(hotkeys_active = self.hotkeys_active, "Sending hotkey state");
        _ = self.wm_tx.send((
            tracing::Span::current(),
            WmEvent::HotkeysActive(self.hotkeys_active),
        ));
    }
}
