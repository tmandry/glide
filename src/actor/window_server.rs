// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use objc2::MainThreadMarker;
use tracing::{Span, debug, instrument, warn};

pub use crate::actor::app::pid_t;
use crate::actor::app::{self, AppThreadHandle, Quiet, WindowId, WindowInfo};
use crate::actor::reactor::Event;
use crate::actor::wm_controller::{self, WmEvent};
use crate::actor::{self, reactor};
use crate::collections::HashMap;
use crate::sys::event::MouseState;
use crate::sys::screen::{NSScreenInfo, ScreenCache};
use crate::sys::window_server::{
    self as sys_ws, SkylightConnection, SkylightNotifier, WindowServerId, WindowsOnScreen,
    kCGSWindowIsTerminated,
};

/// CGWindowLevel values for windows we care about. Everything else (e.g.
/// status bar items, screensavers, system overlays) is filtered out early.
const LAYER_NORMAL: i32 = 0; // kCGNormalWindowLevel
const LAYER_FLOATING: i32 = 3; // kCGFloatingWindowLevel
const LAYER_STATUS: i32 = 8; // kCGStatusWindowLevel (used by some panels)

// ---------------------------------------------------------------------------
// WindowServer – off main thread
// ---------------------------------------------------------------------------

/// Actor that takes events from app actors and adds information from the window
/// server before sending them on to the Reactor.
pub struct WindowServer {
    screen_cache: ScreenCache,
    /// Window server IDs currently visible on screen.
    visible_window_ids: Vec<WindowServerId>,
    wm_tx: wm_controller::Sender,
    reactor_tx: reactor::Sender,
    skylight_tx: SkylightSender,
}

#[derive(Debug)]
pub enum Request {
    // Sent by the NotificationCenter actor.
    /// Screen configuration changed. Carries NSScreenInfo gathered on the main thread.
    ScreenParametersChanged(Vec<NSScreenInfo>),
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
    /// Reactor event passthrough.
    ///
    /// All reactor events go through us so they reach the reactor in the
    /// correct order with respect to the other events above.
    ReactorEvent(Event),
}

pub type Sender = actor::Sender<Request>;
pub type Receiver = actor::Receiver<Request>;

impl WindowServer {
    pub fn new(
        wm_tx: wm_controller::Sender,
        reactor_tx: reactor::Sender,
        skylight_tx: SkylightSender,
    ) -> Self {
        Self {
            screen_cache: ScreenCache::new(),
            visible_window_ids: vec![],
            wm_tx,
            reactor_tx,
            skylight_tx,
        }
    }

    pub async fn run(mut self, mut requests_rx: Receiver) {
        while let Some((span, request)) = requests_rx.recv().await {
            let _span = span.entered();
            self.on_request(request);
        }
    }

    #[instrument(skip(self))]
    fn on_request(&mut self, request: Request) {
        match request {
            Request::RegisterWindow(wsid, wid, tx) => {
                self.skylight_tx.send(SkylightRequest::TrackWindow(wsid, wid, tx));
            }
            Request::ScreenParametersChanged(ns_screens) => {
                let Some((screens, converter)) = self.screen_cache.update_screen_config(ns_screens)
                else {
                    return;
                };
                let on_screen = self.get_windows_on_screen();
                let event = WmEvent::ScreenParametersChanged {
                    screens: screens.iter().map(|s| s.id).collect(),
                    frames: screens.iter().map(|s| s.visible_frame).collect(),
                    converter,
                    spaces: self.screen_cache.get_screen_spaces(),
                    scale_factors: screens.iter().map(|s| s.scale_factor).collect(),
                };
                self.send_wm_event(event);
                self.reactor_tx.send(Event::WindowsOnScreenUpdated { pid: None, on_screen });
            }
            Request::SpaceChanged => {
                let spaces = self.screen_cache.get_screen_spaces();
                let on_screen = self.get_windows_on_screen();
                self.send_wm_event(WmEvent::SpaceChanged(spaces));
                self.reactor_tx.send(Event::WindowsOnScreenUpdated { pid: None, on_screen });
            }
            Request::WindowCreated(wid, info, mouse_state) => {
                let on_screen = self.get_windows_on_screen();
                let pid = wid.pid;
                self.reactor_tx.send(Event::WindowCreated(wid, info, mouse_state));
                self.reactor_tx
                    .send(Event::WindowsOnScreenUpdated { pid: Some(pid), on_screen });
                self.reactor_tx.send(Event::WindowBecameVisible(wid));
            }
            Request::ApplicationMainWindowChanged(pid, wid, quiet) => {
                self.update_visible_window_ids();
                self.reactor_tx.send(Event::ApplicationMainWindowChanged(pid, wid, quiet));
            }
            Request::WindowVisibilityChanged(_window_id) => {
                self.update_visible_window_ids();
            }
            Request::ReactorEvent(event) => self.reactor_tx.send(event),
        }
    }

    fn get_windows_on_screen(&mut self) -> WindowsOnScreen {
        let windows: Vec<_> = sys_ws::get_visible_windows_with_layer(None)
            .into_iter()
            .filter(|w| matches!(w.layer, LAYER_NORMAL | LAYER_FLOATING | LAYER_STATUS))
            .collect();
        self.visible_window_ids = windows.iter().map(|w| w.id).collect();
        WindowsOnScreen::new(windows)
    }

    fn update_visible_window_ids(&mut self) {
        self.visible_window_ids = sys_ws::get_visible_window_ids();
    }

    fn send_wm_event(&self, event: WmEvent) {
        _ = self.wm_tx.send((Span::current().clone(), event));
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
}

/// Commands sent from the reactor-thread `WindowServer` to the main-thread
/// `SkylightWatcher`.
#[derive(Debug)]
pub enum SkylightRequest {
    TrackWindow(WindowServerId, WindowId, AppThreadHandle),
}

pub type SkylightSender = actor::Sender<SkylightRequest>;
pub type SkylightReceiver = actor::Receiver<SkylightRequest>;

impl SkylightWatcher {
    pub fn new(mtm: MainThreadMarker) -> Self {
        Self(Rc::new_cyclic(
            |weak_self: &Weak<RefCell<SkylightWatcherState>>| {
                let mut state = SkylightWatcherState {
                    connection: SkylightConnection::new(mtm),
                    notifiers: vec![],
                    weak_self: weak_self.clone(),
                    registered_windows: HashMap::default(),
                };
                state.register_callbacks();
                RefCell::new(state)
            },
        ))
    }

    pub async fn run(self, mut commands_rx: SkylightReceiver) {
        while let Some((_span, command)) = commands_rx.recv().await {
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
        }
    }

    fn on_window_destroyed(&mut self, wsid: WindowServerId) {
        debug!("Window destroyed: {wsid:?}");
        let Some((wid, tx)) = self.registered_windows.remove(&wsid) else {
            return;
        };
        self.connection.on_window_destroyed(wsid);
        _ = tx.send(app::Request::WindowDestroyed(wid));
    }
}
