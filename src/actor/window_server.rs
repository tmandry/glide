// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::cell::RefCell;
use std::mem;
use std::rc::{Rc, Weak};
use std::time::Duration;

use objc2::MainThreadMarker;
use objc2_app_kit::NSRunningApplication;
use objc2_foundation::ns_string;
use tracing::{debug, instrument, warn};

pub use crate::actor::app::pid_t;
use crate::actor::app::{self, AppInfo, AppThreadHandle, Quiet, WindowId, WindowInfo};
use crate::actor::{self, reactor, space_manager, wm_controller};
use crate::collections::HashMap;
use crate::sys::app::NSRunningApplicationExt;
use crate::sys::event::MouseState;
use crate::sys::screen::{self, NSScreenInfo, ScreenCache, ScreenInfo, SpaceId};
use crate::sys::window_server::{
    self as sys_ws, SkylightConnection, SkylightNotifier, WindowServerId, WindowsOnScreen,
    kCGSWindowIsInvisible, kCGSWindowIsTerminated, kCGSWindowIsVisible,
};

/// CGWindowLevel values for windows we care about. Everything else (e.g.
/// status bar items, screensavers, system overlays) is filtered out early.
const LAYER_NORMAL: i32 = 0; // kCGNormalWindowLevel
const LAYER_FLOATING: i32 = 3; // kCGFloatingWindowLevel
const LAYER_STATUS: i32 = 8; // kCGStatusWindowLevel (used by some panels)
const LAYER_DOCK: i32 = 20; // kCGDockWindowLevel

// ---------------------------------------------------------------------------
// WindowServer – off main thread
// ---------------------------------------------------------------------------

/// Actor that takes events from app actors and adds information from the window
/// server before sending them on to the Reactor via the SpaceManager.
pub struct WindowServer {
    screen_cache: ScreenCache,
    /// Window server IDs currently visible on screen.
    visible_window_ids: Vec<WindowServerId>,
    sm_tx: space_manager::Sender,
    wm_tx: wm_controller::Sender,
    skylight_tx: SkylightSender,
    screen_config_retry_attempt: u8,
    screen_config_retry_pending: bool,
    /// The screen configuration last sent downstream.
    last_screen_config: Option<ScreenConfig>,
}

/// A screen configuration, along with the window order it was sent with.
#[derive(Clone, PartialEq)]
struct ScreenConfig {
    screens: Vec<ScreenInfo>,
    spaces: Vec<Option<SpaceId>>,
    visible: Vec<WindowServerId>,
}

#[derive(Debug)]
pub enum Event {
    // Sent by the NotificationCenter actor.
    /// Screen configuration changed. Carries NSScreenInfo gathered on the main thread.
    ScreenParametersChanged(Vec<NSScreenInfo>),
    /// A display snapshot requested after a transiently inconsistent update.
    RetryScreenParameters {
        attempt: u8,
        screens: Vec<NSScreenInfo>,
    },
    /// The active space changed.
    SpaceChanged,

    // Sent by the App actor.
    /// This is to work around a bug introduced in macOS Sequoia where
    /// kAXUIElementDestroyedNotification is not always sent correctly.
    ///
    /// See https://github.com/glide-wm/glide/issues/10.
    RegisterWindow(WindowServerId, WindowId, AppThreadHandle),
    /// A new window was created.
    WindowCreated(WindowId, WindowInfo, MouseState),
    /// The main window of an application changed.
    ApplicationMainWindowChanged(pid_t, Option<WindowId>, Quiet),
    /// A window was minimized or unminimized.
    WindowVisibilityChanged(WindowId),
    ApplicationLaunched {
        pid: pid_t,
        handle: AppThreadHandle,
        info: AppInfo,
        is_frontmost: bool,
        main_window: Option<WindowId>,
        visible_windows: Vec<(WindowId, WindowInfo)>,
    },
    /// Reactor event passthrough.
    ///
    /// All reactor events go through us so they reach the reactor in the
    /// correct order with respect to the other events above.
    ReactorEvent(reactor::Event),
    /// Sent by SpaceManager when it needs a fresh window list (e.g. after
    /// toggling a space or exiting expose).
    RequestSpaceRefresh,
    /// Sent by SpaceManager when the Dock confirms Exposé.
    ExposeConfirmed,
}

pub type Sender = actor::Sender<Event>;
pub type Receiver = actor::Receiver<Event>;

impl WindowServer {
    pub fn new(
        sm_tx: space_manager::Sender,
        wm_tx: wm_controller::Sender,
        skylight_tx: SkylightSender,
    ) -> Self {
        Self {
            screen_cache: ScreenCache::new(),
            visible_window_ids: vec![],
            sm_tx,
            wm_tx,
            skylight_tx,
            screen_config_retry_attempt: 0,
            screen_config_retry_pending: false,
            last_screen_config: None,
        }
    }

    pub async fn run(mut self, mut events_rx: Receiver) {
        while let Some((span, event)) = events_rx.recv().await {
            let _span = span.entered();
            self.on_event(event);
        }
    }

    #[instrument(skip(self))]
    fn on_event(&mut self, event: Event) {
        match event {
            Event::RegisterWindow(wsid, wid, tx) => {
                self.skylight_tx.send(SkylightRequest::TrackWindow(wsid, wid, tx));
            }
            Event::ExposeConfirmed => self.skylight_tx.send(SkylightRequest::FindExposeWindow),
            Event::ScreenParametersChanged(ns_screens) => self.handle_screen_parameters(ns_screens),
            Event::RetryScreenParameters { attempt, screens } => {
                if !self.screen_config_retry_pending || attempt != self.screen_config_retry_attempt
                {
                    return;
                }
                self.screen_config_retry_pending = false;
                self.screen_config_retry_attempt += 1;
                self.handle_screen_parameters(screens);
            }
            Event::SpaceChanged | Event::RequestSpaceRefresh => {
                let spaces = self.screen_cache.get_screen_spaces();
                let on_screen = self.get_windows_on_screen();
                self.sm_tx.send(space_manager::Event::SpaceChanged(spaces, on_screen));
            }
            Event::WindowCreated(wid, info, mouse_state) => {
                let pid = wid.pid;
                self.send_reactor_event(reactor::Event::WindowCreated(wid, info, mouse_state));
                self.send_windows_on_screen_if_changed(Some(pid));
                self.send_reactor_event(reactor::Event::WindowBecameVisible(wid));
            }
            Event::ApplicationMainWindowChanged(pid, wid, quiet) => {
                self.send_reactor_event(reactor::Event::ApplicationMainWindowChanged(
                    pid, wid, quiet,
                ));
            }
            Event::WindowVisibilityChanged(window_id) => {
                self.send_windows_on_screen_if_changed(Some(window_id.pid));
            }
            Event::ApplicationLaunched {
                pid,
                handle,
                info,
                is_frontmost,
                main_window,
                visible_windows,
            } => {
                let on_screen = self.get_windows_on_screen();
                self.send_reactor_event(reactor::Event::WindowsOnScreenUpdated {
                    pid: Some(pid),
                    on_screen,
                });
                self.send_reactor_event(reactor::Event::ApplicationLaunched {
                    pid,
                    handle,
                    info,
                    is_frontmost,
                    main_window,
                    visible_windows,
                });
            }
            Event::ReactorEvent(event) => self.send_reactor_event(event),
        }
    }

    fn handle_screen_parameters(&mut self, ns_screens: Vec<NSScreenInfo>) {
        let Some((screens, converter)) = self.screen_cache.update_screen_config(ns_screens) else {
            self.schedule_screen_config_retry();
            return;
        };
        self.screen_config_retry_attempt = 0;
        self.screen_config_retry_pending = false;

        // The system has been observed to send long runs of this notification
        // with no actual change; drop them here so the rest of the system never
        // sees them. The windows on screen have to be unchanged too: an
        // identical configuration is also reported after the system restacks
        // windows, and that is our cue to put them back. Compare the order
        // alone, so windows that are merely moving don't count as a change.
        let on_screen = self.get_windows_on_screen();
        let config = ScreenConfig {
            screens,
            spaces: self.screen_cache.get_screen_spaces(),
            visible: on_screen.visible.clone(),
        };
        if self.last_screen_config.as_ref() == Some(&config) {
            debug!("Screen configuration is unchanged");
            return;
        }
        self.last_screen_config = Some(config.clone());
        let ScreenConfig { screens, spaces, .. } = config;

        self.sm_tx.send(space_manager::Event::ScreenParametersChanged {
            screens: screens.iter().map(|s| s.id).collect(),
            frames: screens.iter().map(|s| s.visible_frame).collect(),
            converter,
            spaces,
            scale_factors: screens.iter().map(|s| s.scale_factor).collect(),
            on_screen,
        });
    }

    fn schedule_screen_config_retry(&mut self) {
        const RETRY_DELAYS: [Duration; 3] = [
            Duration::from_millis(100),
            Duration::from_millis(250),
            Duration::from_millis(500),
        ];
        if self.screen_config_retry_pending {
            return;
        }
        let attempt = self.screen_config_retry_attempt;
        let Some(&delay) = RETRY_DELAYS.get(attempt as usize) else {
            warn!(attempt, "Giving up on inconsistent screen configuration");
            // Start over, so that the next inconsistent update gets its own
            // retries. Otherwise the counter stays exhausted until some later
            // update happens to succeed.
            self.screen_config_retry_attempt = 0;
            return;
        };
        self.screen_config_retry_pending = true;
        let wm_tx = self.wm_tx.clone();
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            _ = wm_tx.send((
                tracing::Span::none(),
                wm_controller::WmEvent::RefreshScreenParameters(attempt),
            ));
        });
    }

    /// Queries the window server for visible windows and sends a
    /// `WindowsOnScreenUpdated` event if the list changed.
    fn send_windows_on_screen_if_changed(&mut self, pid: Option<pid_t>) {
        let prev = mem::take(&mut self.visible_window_ids);
        let on_screen = self.get_windows_on_screen();
        if self.visible_window_ids != prev {
            self.send_reactor_event(reactor::Event::WindowsOnScreenUpdated { pid, on_screen });
        }
    }

    fn get_windows_on_screen(&mut self) -> WindowsOnScreen {
        let windows: Vec<_> = self
            .get_all_visible_windows()
            .into_iter()
            .filter(|w| matches!(w.layer, LAYER_NORMAL | LAYER_FLOATING | LAYER_STATUS))
            .collect();
        self.visible_window_ids = windows.iter().map(|w| w.id).collect();
        WindowsOnScreen::new(windows)
    }

    #[cfg(not(test))]
    fn get_all_visible_windows(&self) -> Vec<sys_ws::WindowServerInfo> {
        sys_ws::get_visible_windows_with_layer(None)
    }

    #[cfg(test)]
    fn get_all_visible_windows(&self) -> Vec<sys_ws::WindowServerInfo> {
        MOCK_VISIBLE_WINDOWS.with(|w| w.borrow().clone())
    }

    fn send_reactor_event(&self, event: reactor::Event) {
        self.sm_tx.send(space_manager::Event::ReactorEvent(event));
    }
}

// ---------------------------------------------------------------------------
// SkylightWatcher – main thread only
// ---------------------------------------------------------------------------

/// Watches for Skylight window-server events. Requires the main thread because
/// of `SkylightConnection`.
pub struct SkylightWatcher(Rc<RefCell<SkylightWatcherState>>);

struct SkylightWatcherState {
    connection: SkylightConnection,
    notifiers: Vec<SkylightNotifier>,
    weak_self: Weak<RefCell<Self>>,
    /// Registered windows (for SkyLight destruction tracking).
    registered_windows: HashMap<WindowServerId, (WindowId, AppThreadHandle)>,
    sm_tx: space_manager::Sender,
    /// The Dock's Exposé window.
    expose_window: Option<WindowServerId>,
}

/// Commands sent from the reactor-thread `WindowServer` to the main-thread
/// `SkylightWatcher`.
#[derive(Debug)]
pub enum SkylightRequest {
    TrackWindow(WindowServerId, WindowId, AppThreadHandle),
    FindExposeWindow,
}

pub type SkylightSender = actor::Sender<SkylightRequest>;
pub type SkylightReceiver = actor::Receiver<SkylightRequest>;

impl SkylightWatcher {
    pub fn new(mtm: MainThreadMarker, sm_tx: space_manager::Sender) -> Self {
        Self(Rc::new_cyclic(
            |weak_self: &Weak<RefCell<SkylightWatcherState>>| {
                let mut state = SkylightWatcherState {
                    connection: SkylightConnection::new(mtm),
                    notifiers: vec![],
                    weak_self: weak_self.clone(),
                    registered_windows: HashMap::default(),
                    sm_tx,
                    expose_window: None,
                };
                state.register_callbacks();
                RefCell::new(state)
            },
        ))
    }

    pub async fn run(self, mut commands_rx: SkylightReceiver) {
        while let Some((span, command)) = commands_rx.recv().await {
            let _span = span.entered();
            let mut state = self.0.borrow_mut();
            state.on_command(command);
        }
    }
}

impl SkylightWatcherState {
    fn register_callbacks(&mut self) {
        self.register_callback(kCGSWindowIsTerminated, |this, wsid| {
            this.on_window_destroyed(wsid)
        });
        self.register_callback(kCGSWindowIsVisible, |this, wsid| {
            this.on_window_visibility_changed(wsid, true)
        });
        self.register_callback(kCGSWindowIsInvisible, |this, wsid| {
            this.on_window_visibility_changed(wsid, false)
        });
    }

    fn on_window_visibility_changed(&mut self, wsid: WindowServerId, visible: bool) {
        if self.expose_window == Some(wsid) {
            self.sm_tx.send(space_manager::Event::ExposeActive(visible));
        }
    }

    fn register_callback(&mut self, event: u32, callback: fn(&mut Self, WindowServerId)) {
        let weak_self = self.weak_self.clone();
        let expected_event = event;
        let notifier = self
            .connection
            .on_event(event, move |callback_event, data| {
                if callback_event != expected_event {
                    return;
                }
                let wsid = WindowServerId(u32::from_ne_bytes(
                    data.try_into().expect("data should be a CGWindowID"),
                ));
                let Some(state) = weak_self.upgrade() else {
                    warn!("could not upgrade state in callback");
                    return;
                };
                callback(&mut state.borrow_mut(), wsid);
            })
            .expect("Initializing SkylightNotifier");
        self.notifiers.push(notifier);
    }

    fn on_command(&mut self, command: SkylightRequest) {
        match command {
            SkylightRequest::TrackWindow(wsid, wid, tx) => {
                debug!("Window registered: {wsid:?}");
                self.registered_windows.insert(wsid, (wid, tx));
                if let Err(e) = self.connection.add_window(wsid) {
                    warn!("Failed to update SkylightConnection window list: {e}");
                }
            }
            SkylightRequest::FindExposeWindow => {
                let found = find_dock_expose_window();
                if found == self.expose_window {
                    return;
                }
                if let Some(old) = self.expose_window.take() {
                    _ = self.connection.remove_window(old);
                }
                self.expose_window = found.filter(|&wsid| self.connection.add_window(wsid).is_ok());
            }
        }
    }

    fn on_window_destroyed(&mut self, wsid: WindowServerId) {
        debug!("Window destroyed: {wsid:?}");
        if self.expose_window == Some(wsid) {
            self.expose_window = None;
            self.connection.on_window_destroyed(wsid);
            return;
        }
        let Some((wid, tx)) = self.registered_windows.remove(&wsid) else {
            return;
        };
        self.connection.on_window_destroyed(wsid);
        _ = tx.send(app::Request::WindowDestroyed(wid));
    }
}

/// The Dock keeps one persistent window at the Dock level and orders it in
/// when Exposé starts; Exposé adds more on the same level, so take the oldest
/// of those covering a whole display.
fn find_dock_expose_window() -> Option<WindowServerId> {
    let dock =
        NSRunningApplication::runningApplicationsWithBundleIdentifier(ns_string!("com.apple.dock"));
    let dock_pid = dock.iter().next()?.pid();
    let displays = screen::display_bounds();
    sys_ws::get_visible_windows_with_layer(Some(LAYER_DOCK))
        .into_iter()
        .filter(|w| w.pid == dock_pid && displays.contains(&w.frame))
        .map(|w| w.id)
        .min()
}

#[cfg(test)]
thread_local! {
    static MOCK_VISIBLE_WINDOWS: RefCell<Vec<sys_ws::WindowServerInfo>> = RefCell::new(vec![]);
}

#[cfg(test)]
mod tests {
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};
    use test_log::test;

    use super::*;
    use crate::actor::{self, space_manager};
    use crate::sys::window_server::{WindowServerId, WindowServerInfo};

    fn wsid(id: u32) -> WindowServerId {
        WindowServerId::new(id)
    }

    fn make_window(id: u32, layer: i32) -> WindowServerInfo {
        WindowServerInfo {
            id: wsid(id),
            pid: 1,
            layer,
            frame: CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(100.0, 100.0)),
        }
    }

    fn set_mock_windows(windows: Vec<WindowServerInfo>) {
        MOCK_VISIBLE_WINDOWS.with(|w| *w.borrow_mut() = windows);
    }

    struct TestHarness {
        ws: WindowServer,
        sm_rx: space_manager::Receiver,
        #[expect(dead_code)]
        skylight_rx: SkylightReceiver,
    }

    impl TestHarness {
        fn new() -> Self {
            let (sm_tx, sm_rx) = actor::channel();
            let (wm_tx, _wm_rx) = tokio::sync::mpsc::unbounded_channel();
            let (skylight_tx, skylight_rx) = actor::channel();
            let ws = WindowServer::new(sm_tx, wm_tx, skylight_tx);
            Self { ws, sm_rx, skylight_rx }
        }

        fn on_event(&mut self, event: Event) {
            self.ws.on_event(event);
        }

        fn drain_sm(&mut self) -> Vec<space_manager::Event> {
            let mut events = vec![];
            while let Ok((_, event)) = self.sm_rx.try_recv() {
                events.push(event);
            }
            events
        }
    }

    fn find_reactor_events(sm_events: &[space_manager::Event]) -> Vec<&reactor::Event> {
        sm_events
            .iter()
            .filter_map(|e| match e {
                space_manager::Event::ReactorEvent(re) => Some(re),
                _ => None,
            })
            .collect()
    }

    fn find_windows_on_screen_updated<'a>(
        reactor_events: &'a [&'a reactor::Event],
    ) -> Vec<&'a WindowsOnScreen> {
        reactor_events
            .iter()
            .filter_map(|e| match e {
                reactor::Event::WindowsOnScreenUpdated { on_screen, .. } => Some(on_screen),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn giving_up_on_screen_config_resets_the_retry_counter() {
        let mut h = TestHarness::new();
        // Pretend the retries for one inconsistent update are exhausted. This
        // takes the give-up branch, so no retry is scheduled.
        h.ws.screen_config_retry_attempt = 3;
        h.ws.schedule_screen_config_retry();
        assert!(!h.ws.screen_config_retry_pending);
        // The next inconsistent update should get its own retries rather than
        // giving up immediately.
        assert_eq!(h.ws.screen_config_retry_attempt, 0);
    }

    #[test]
    fn filters_irrelevant_layers() {
        set_mock_windows(vec![
            make_window(1, LAYER_NORMAL),   // 0 – keep
            make_window(2, LAYER_FLOATING), // 3 – keep
            make_window(3, LAYER_STATUS),   // 8 – keep
            make_window(4, 25),             // screensaver – filter
            make_window(5, -1),             // desktop – filter
        ]);

        let mut h = TestHarness::new();
        h.on_event(Event::WindowVisibilityChanged(WindowId::new(1, 1)));
        let sm_events = h.drain_sm();
        let reactor_events = find_reactor_events(&sm_events);
        let updates = find_windows_on_screen_updated(&reactor_events);

        assert_eq!(updates.len(), 1);
        let visible_ids: Vec<u32> = updates[0].visible.iter().map(|id| id.as_u32()).collect();
        assert_eq!(visible_ids, vec![1, 2, 3]);
    }

    #[test]
    fn no_event_when_visible_windows_unchanged() {
        set_mock_windows(vec![make_window(1, LAYER_NORMAL)]);

        let mut h = TestHarness::new();
        // First call: visible_window_ids goes from [] to [1] – changed.
        h.on_event(Event::WindowVisibilityChanged(WindowId::new(1, 1)));
        let sm_events = h.drain_sm();
        let reactor_events = find_reactor_events(&sm_events);
        assert_eq!(find_windows_on_screen_updated(&reactor_events).len(), 1);

        // Second call: visible_window_ids is still [1] – no change.
        h.on_event(Event::WindowVisibilityChanged(WindowId::new(1, 1)));
        let sm_events = h.drain_sm();
        let reactor_events = find_reactor_events(&sm_events);
        assert_eq!(find_windows_on_screen_updated(&reactor_events).len(), 0);
    }

    #[test]
    fn event_sent_when_visible_windows_change() {
        set_mock_windows(vec![make_window(1, LAYER_NORMAL)]);

        let mut h = TestHarness::new();
        h.on_event(Event::WindowVisibilityChanged(WindowId::new(1, 1)));
        h.drain_sm();

        // Change the mock.
        set_mock_windows(vec![make_window(1, LAYER_NORMAL), make_window(2, LAYER_NORMAL)]);
        h.on_event(Event::WindowVisibilityChanged(WindowId::new(1, 1)));
        let sm_events = h.drain_sm();
        let reactor_events = find_reactor_events(&sm_events);
        let updates = find_windows_on_screen_updated(&reactor_events);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].visible.len(), 2);
    }

    #[test]
    fn window_created_sends_windows_on_screen_if_changed() {
        set_mock_windows(vec![make_window(1, LAYER_NORMAL)]);

        let mut h = TestHarness::new();
        let wid = WindowId::new(1, 1);
        let info = WindowInfo {
            is_standard: true,
            title: String::new().into(),
            frame: CGRect::ZERO,
            sys_id: None,
            is_resizable: true,
            ax_role: "AXWindow".into(),
            ax_subrole: None,
        };
        h.on_event(Event::WindowCreated(wid, info, MouseState::Up));
        let sm_events = h.drain_sm();
        let reactor_events = find_reactor_events(&sm_events);

        // Should have WindowCreated, WindowsOnScreenUpdated, WindowBecameVisible.
        assert!(reactor_events.iter().any(|e| matches!(e, reactor::Event::WindowCreated(..))));
        assert_eq!(find_windows_on_screen_updated(&reactor_events).len(), 1);
        assert!(
            reactor_events
                .iter()
                .any(|e| matches!(e, reactor::Event::WindowBecameVisible(_)))
        );
    }
}
