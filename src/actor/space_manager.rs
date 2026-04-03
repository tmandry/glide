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
    FocusedScreenChanged(ScreenId),
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
    focused_screen: Option<ScreenId>,
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
            focused_screen: None,
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
            Event::FocusedScreenChanged(screen_id) => {
                self.focused_screen = Some(screen_id);
                self.send_space_enabled_status();
            }
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

    fn focused_space(&self) -> Option<SpaceId> {
        self.focused_screen
            .and_then(|s| self.space_for_screen(s))
            .or_else(|| self.first_space())
    }

    fn send_space_enabled_status(&self) {
        let enabled = self.focused_space().map(|s| self.is_space_enabled(s)).unwrap_or(false);
        self.status_tx.send(status::Event::SpaceEnabledChanged(enabled));
    }

    fn request_space_refresh(&self) {
        self.ws_tx.send(window_server::Event::RequestSpaceRefresh);
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use objc2_core_foundation::CGRect;
    use test_log::test;
    use tokio::sync::mpsc;
    use tracing::Span;

    use super::*;
    use crate::actor::{self, group_bars, mouse, reactor, status, window_server};
    use crate::config::Config;
    use crate::sys::screen::{CoordinateConverter, ScreenId, SpaceId};
    use crate::sys::window_server::WindowsOnScreen;

    struct TestHarness {
        sm: SpaceManager,
        reactor_rx: reactor::Receiver,
        ws_rx: window_server::Receiver,
        wm_rx: mpsc::UnboundedReceiver<(Span, WmEvent)>,
        status_rx: status::Receiver,
        #[expect(dead_code)]
        group_bars_rx: group_bars::Receiver,
        #[expect(dead_code)]
        mouse_rx: mouse::Receiver,
    }

    impl TestHarness {
        fn new() -> Self {
            Self::new_with(false, Config::default())
        }

        fn new_with(one_space: bool, config: Config) -> Self {
            let (reactor_tx, reactor_rx) = actor::channel();
            let (ws_tx, ws_rx) = actor::channel();
            let (wm_tx, wm_rx) = mpsc::unbounded_channel();
            let (status_tx, status_rx) = actor::channel();
            let (group_bars_tx, group_bars_rx) = actor::channel();
            let (mouse_tx, mouse_rx) = actor::channel();
            let sm = SpaceManager::new(
                one_space,
                Arc::new(config),
                reactor_tx,
                ws_tx,
                wm_tx,
                status_tx,
                group_bars_tx,
                mouse_tx,
            );
            Self {
                sm,
                reactor_rx,
                ws_rx,
                wm_rx,
                status_rx,
                group_bars_rx,
                mouse_rx,
            }
        }

        fn on_event(&mut self, event: Event) {
            self.sm.on_event(event);
        }

        /// Send a ScreenParametersChanged event with one screen/space.
        fn setup_space(&mut self, screen: ScreenId, space: SpaceId) {
            self.on_event(Event::ScreenParametersChanged {
                screens: vec![screen],
                frames: vec![CGRect::ZERO],
                spaces: vec![Some(space)],
                scale_factors: vec![1.0],
                converter: CoordinateConverter::default(),
                on_screen: WindowsOnScreen::new(vec![]),
            });
            self.drain_all();
        }

        fn send_space_changed(&mut self, spaces: Vec<Option<SpaceId>>) {
            self.on_event(Event::SpaceChanged(spaces, WindowsOnScreen::new(vec![])));
        }

        fn drain_all(&mut self) {
            drain(&mut self.reactor_rx);
            drain_ws(&mut self.ws_rx);
            drain_wm(&mut self.wm_rx);
            drain_status(&mut self.status_rx);
        }
    }

    fn drain<T>(rx: &mut actor::Receiver<T>) -> Vec<T> {
        let mut events = vec![];
        while let Ok((_, event)) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    fn drain_ws(rx: &mut window_server::Receiver) -> Vec<window_server::Event> {
        drain(rx)
    }

    fn drain_status(rx: &mut status::Receiver) -> Vec<status::Event> {
        drain(rx)
    }

    fn drain_wm(rx: &mut mpsc::UnboundedReceiver<(Span, WmEvent)>) -> Vec<WmEvent> {
        let mut events = vec![];
        while let Ok((_, event)) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    /// Extract the spaces from a reactor SpaceChanged event.
    fn space_changed_spaces(events: &[reactor::Event]) -> Option<&Vec<Option<SpaceId>>> {
        events.iter().find_map(|e| match e {
            reactor::Event::SpaceChanged(spaces, _) => Some(spaces),
            _ => None,
        })
    }

    fn screen(id: u32) -> ScreenId {
        ScreenId::new(id)
    }

    fn space(id: u64) -> SpaceId {
        SpaceId::new(id)
    }

    /// With default_disable=true (the default config), spaces start disabled.
    /// ToggleSpace enables the current space.
    #[test]
    fn toggle_enables_space_when_default_disable() {
        let mut h = TestHarness::new();
        h.setup_space(screen(1), space(10));

        // Space is disabled by default.
        h.send_space_changed(vec![Some(space(10))]);
        let events = drain(&mut h.reactor_rx);
        assert_eq!(*space_changed_spaces(&events).unwrap(), vec![None]);

        // Toggle to enable.
        h.on_event(Event::ToggleSpace(screen(1)));
        h.drain_all();

        h.send_space_changed(vec![Some(space(10))]);
        let events = drain(&mut h.reactor_rx);
        assert_eq!(*space_changed_spaces(&events).unwrap(), vec![Some(space(10))]);
    }

    /// Toggle twice returns to the original state.
    #[test]
    fn toggle_space_twice_restores_state() {
        let mut h = TestHarness::new();
        h.setup_space(screen(1), space(10));

        // Enable then disable again.
        h.on_event(Event::ToggleSpace(screen(1)));
        h.drain_all();
        h.on_event(Event::ToggleSpace(screen(1)));
        h.drain_all();

        h.send_space_changed(vec![Some(space(10))]);
        let events = drain(&mut h.reactor_rx);
        assert_eq!(*space_changed_spaces(&events).unwrap(), vec![None]);
    }

    /// With default_disable=false, spaces start enabled. ToggleSpace disables.
    #[test]
    fn toggle_disables_space_when_default_enable() {
        let mut config = Config::default();
        config.settings.default_disable = false;
        let mut h = TestHarness::new_with(false, config);
        h.setup_space(screen(1), space(10));

        // Space is enabled by default.
        h.send_space_changed(vec![Some(space(10))]);
        let events = drain(&mut h.reactor_rx);
        assert_eq!(*space_changed_spaces(&events).unwrap(), vec![Some(space(10))]);

        // Toggle to disable.
        h.on_event(Event::ToggleSpace(screen(1)));
        h.drain_all();

        h.send_space_changed(vec![Some(space(10))]);
        let events = drain(&mut h.reactor_rx);
        assert_eq!(*space_changed_spaces(&events).unwrap(), vec![None]);
    }

    #[test]
    fn global_disable_nones_all_spaces() {
        let mut h = TestHarness::new();
        h.setup_space(screen(1), space(10));
        // Enable the space first so we can verify global disable overrides it.
        h.on_event(Event::ToggleSpace(screen(1)));
        h.drain_all();

        h.on_event(Event::ToggleGlobalEnabled);
        h.drain_all();

        h.send_space_changed(vec![Some(space(10))]);
        let events = drain(&mut h.reactor_rx);
        assert_eq!(*space_changed_spaces(&events).unwrap(), vec![None]);
    }

    #[test]
    fn one_space_disables_non_starting_space() {
        let mut h = TestHarness::new_with(true, Config::default());
        // Enable the space and establish it as starting_space.
        h.on_event(Event::ScreenParametersChanged {
            screens: vec![screen(1)],
            frames: vec![CGRect::ZERO],
            spaces: vec![Some(space(10))],
            scale_factors: vec![1.0],
            converter: CoordinateConverter::default(),
            on_screen: WindowsOnScreen::new(vec![]),
        });
        h.on_event(Event::ToggleSpace(screen(1)));
        h.drain_all();

        // Starting space is enabled.
        h.send_space_changed(vec![Some(space(10))]);
        let events = drain(&mut h.reactor_rx);
        assert_eq!(*space_changed_spaces(&events).unwrap(), vec![Some(space(10))]);

        // Switch to a different space – disabled because one_space mode.
        h.send_space_changed(vec![Some(space(20))]);
        let events = drain(&mut h.reactor_rx);
        assert_eq!(*space_changed_spaces(&events).unwrap(), vec![None]);

        // Switch back to starting space – still enabled.
        h.send_space_changed(vec![Some(space(10))]);
        let events = drain(&mut h.reactor_rx);
        assert_eq!(*space_changed_spaces(&events).unwrap(), vec![Some(space(10))]);
    }

    #[test]
    fn login_window_disables_all() {
        let mut h = TestHarness::new();
        h.setup_space(screen(1), space(10));
        h.on_event(Event::ToggleSpace(screen(1)));
        h.drain_all();

        h.on_event(Event::LoginWindowActive(true));
        h.drain_all();

        h.send_space_changed(vec![Some(space(10))]);
        let events = drain(&mut h.reactor_rx);
        assert_eq!(*space_changed_spaces(&events).unwrap(), vec![None]);
    }

    #[test]
    fn expose_suppresses_space_changed() {
        let mut h = TestHarness::new();
        h.setup_space(screen(1), space(10));
        h.on_event(Event::ToggleSpace(screen(1)));
        h.drain_all();

        h.on_event(Event::ExposeActive(true));

        // SpaceChanged should NOT reach reactor.
        h.send_space_changed(vec![Some(space(10))]);
        let events = drain(&mut h.reactor_rx);
        assert!(space_changed_spaces(&events).is_none());

        // Exiting expose sends RequestSpaceRefresh to ws_tx.
        h.on_event(Event::ExposeActive(false));
        let ws_events = drain_ws(&mut h.ws_rx);
        assert!(ws_events.iter().any(|e| matches!(e, window_server::Event::RequestSpaceRefresh)));
    }

    #[test]
    fn hotkey_state_activates_on_first_space() {
        let mut h = TestHarness::new();
        h.on_event(Event::ScreenParametersChanged {
            screens: vec![screen(1)],
            frames: vec![CGRect::ZERO],
            spaces: vec![Some(space(10))],
            scale_factors: vec![1.0],
            converter: CoordinateConverter::default(),
            on_screen: WindowsOnScreen::new(vec![]),
        });
        let wm_events = drain_wm(&mut h.wm_rx);
        assert!(
            wm_events.iter().any(|e| matches!(e, WmEvent::HotkeysActive(true))),
            "Expected HotkeysActive(true), got {wm_events:?}"
        );
    }

    #[test]
    fn one_space_hotkeys_deactivate_on_different_space() {
        let mut h = TestHarness::new_with(true, Config::default());

        // First space activates hotkeys.
        h.on_event(Event::ScreenParametersChanged {
            screens: vec![screen(1)],
            frames: vec![CGRect::ZERO],
            spaces: vec![Some(space(10))],
            scale_factors: vec![1.0],
            converter: CoordinateConverter::default(),
            on_screen: WindowsOnScreen::new(vec![]),
        });
        let wm_events = drain_wm(&mut h.wm_rx);
        assert!(wm_events.iter().any(|e| matches!(e, WmEvent::HotkeysActive(true))));

        // Switch to different space deactivates hotkeys.
        drain(&mut h.reactor_rx);
        h.send_space_changed(vec![Some(space(20))]);
        let wm_events = drain_wm(&mut h.wm_rx);
        assert!(
            wm_events.iter().any(|e| matches!(e, WmEvent::HotkeysActive(false))),
            "Expected HotkeysActive(false), got {wm_events:?}"
        );
    }
}
