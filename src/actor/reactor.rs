// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The Reactor's job is to maintain coherence between the system and model state.
//!
//! It takes events from the rest of the system and builds a coherent picture of
//! what is going on. It shares this with the layout actor, and reacts to layout
//! changes by sending requests out to the other actors in the system.

mod animation;
mod main_window;
mod replay;

#[cfg(test)]
mod testing;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use std::{mem, thread};

use animation::Animation;
use main_window::MainWindowTracker;
use objc2_core_foundation::CGRect;
use redact::Secret;
pub use replay::{Record, replay};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use tokio::sync::mpsc;
use tracing::{Span, debug, error, info, instrument, trace, warn};

use super::mouse;
use crate::actor::app::{AppInfo, AppThreadHandle, Quiet, Request, WindowId, WindowInfo, pid_t};
use crate::actor::layout::{self, LayoutCommand, LayoutEvent, LayoutManager, LayoutWindowInfo};
use crate::actor::raise::{self, RaiseRequest};
use crate::actor::{group_bars, status, window_server, wm_controller};
use crate::collections::{HashMap, HashSet};
use crate::config::Config;
use crate::log::{self, MetricsCommand};
use crate::sys::event::MouseState;
use crate::sys::executor::Executor;
use crate::sys::geometry::{CGRectDef, CGRectExt, SameAs, round_to_physical};
use crate::sys::screen::{CoordinateConverter, SpaceId};
use crate::sys::timer::Timer;
use crate::sys::window_server::{WindowServerId, WindowServerInfo};

pub type Sender = crate::actor::Sender<Event>;
pub type Receiver = crate::actor::Receiver<Event>;

pub fn channel() -> (Sender, Receiver) {
    crate::actor::channel()
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug)]
pub enum Event {
    /// The screen layout, including resolution, changed. This is always the
    /// first event sent on startup.
    ///
    /// The first vec is the frame for each screen. The main screen is always
    /// first in the list.
    ///
    /// See the `SpaceChanged` event for an explanation of the other parameters.
    ScreenParametersChanged {
        #[serde_as(as = "Vec<CGRectDef>")]
        frames: Vec<CGRect>,
        spaces: Vec<Option<SpaceId>>,
        scale_factors: Vec<f64>,
        converter: CoordinateConverter,
        windows: Vec<WindowServerInfo>,
    },

    /// The current space changed.
    ///
    /// There is one SpaceId per screen in the last ScreenParametersChanged
    /// event. `None` in the SpaceId vec disables managing windows on that
    /// screen until the next space change.
    ///
    /// A snapshot of visible windows from the window server is also taken and
    /// sent with this message. This allows us to determine more precisely which
    /// windows are visible on a given space, since app actor events like
    /// WindowsDiscovered are not ordered with respect to space events.
    SpaceChanged(Vec<Option<SpaceId>>, Vec<WindowServerInfo>),

    /// All running apps at launch have been registered.
    StartupComplete,

    /// An application was launched. This event is also sent for every running
    /// application on startup.
    ///
    /// Both WindowInfo (accessibility) and WindowServerInfo are collected for
    /// any already-open windows when the launch event is sent. Since this
    /// event isn't ordered with respect to the Space events, it is possible to
    /// receive this event for a space we just switched off of.. FIXME. The same
    /// is true of WindowCreated events.
    ApplicationLaunched {
        pid: pid_t,
        info: AppInfo,
        #[serde(skip, default = "replay::deserialize_app_thread_handle")]
        handle: AppThreadHandle,
        is_frontmost: bool,
        main_window: Option<WindowId>,
        visible_windows: Vec<(WindowId, WindowInfo)>,
        window_server_info: Vec<WindowServerInfo>,
    },
    ApplicationTerminated(pid_t),
    ApplicationThreadTerminated(pid_t),
    ApplicationActivated(pid_t, Quiet),
    ApplicationDeactivated(pid_t),
    ApplicationGloballyActivated(pid_t),
    ApplicationGloballyDeactivated(pid_t),
    ApplicationMainWindowChanged(pid_t, Option<WindowId>, Quiet),

    WindowsDiscovered {
        pid: pid_t,
        new: Vec<(WindowId, WindowInfo)>,
        known_visible: Vec<WindowId>,
    },
    WindowCreated(WindowId, WindowInfo, Option<WindowServerInfo>, MouseState),
    WindowDestroyed(WindowId),
    WindowFrameChanged(
        WindowId,
        #[serde(with = "CGRectDef")] CGRect,
        TransactionId,
        Requested,
        Option<MouseState>,
    ),

    /// Left mouse button was released.
    ///
    /// Layout changes are suppressed while the button is down so that they
    /// don't interfere with drags. This event is used to update the layout in
    /// case updates were supressed while the button was down.
    ///
    /// FIXME: This can be interleaved incorrectly with the MouseState in app
    /// actor events.
    MouseUp,
    /// The mouse cursor moved over a new window. Only sent if focus-follows-
    /// mouse is enabled.
    MouseMovedOverWindow(WindowServerId),

    /// A raise request completed. Used by the raise manager to track when
    /// all raise requests in a sequence have finished.
    RaiseCompleted {
        window_id: WindowId,
        sequence_id: u64,
    },

    /// A raise sequence timed out. Used by the raise manager to clean up
    /// pending raises that took too long.
    RaiseTimeout {
        sequence_id: u64,
    },

    LeftMouseDown(
        #[serde(with = "crate::sys::geometry::CGPointDef")] objc2_core_foundation::CGPoint,
    ),
    LeftMouseDragged(
        #[serde(with = "crate::sys::geometry::CGPointDef")] objc2_core_foundation::CGPoint,
    ),

    ScrollWheel {
        delta_x: f64,
        delta_y: f64,
        alt_held: bool,
    },

    Command(Command),
    ConfigChanged(Arc<Config>),
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Requested(pub bool);

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum Command {
    Layout(LayoutCommand),
    Metrics(MetricsCommand),
    Reactor(ReactorCommand),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub enum ReactorCommand {
    Debug,
    Serialize,
    SaveAndExit,
}

use crate::actor::raise::RaiseManager;

pub struct Reactor {
    config: Arc<Config>,
    apps: HashMap<pid_t, AppState>,
    layout: LayoutManager,
    windows: HashMap<WindowId, WindowState>,
    window_server_info: HashMap<WindowServerId, WindowServerInfo>,
    window_ids: HashMap<WindowServerId, WindowId>,
    visible_windows: HashSet<WindowServerId>,
    screens: Vec<Screen>,
    active_screen_idx: Option<u16>,
    main_window_tracker: MainWindowTracker,
    in_drag: bool,
    record: Record,
    raise_manager_tx: raise::Sender,
    mouse_tx: Option<mouse::Sender>,
    status_tx: Option<status::Sender>,
    group_indicators_tx: group_bars::Sender,
}

#[derive(Debug)]
struct AppState {
    #[allow(unused)]
    pub info: AppInfo,
    pub handle: AppThreadHandle,
}

#[derive(Copy, Clone, Debug)]
struct Screen {
    frame: CGRect,
    space: Option<SpaceId>,
    scale_factor: f64,
}

/// A per-window counter that tracks the last time the reactor sent a request to
/// change the window frame.
#[derive(Default, Debug, Copy, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransactionId(u32);

#[derive(Debug)]
struct WindowState {
    #[allow(unused)]
    title: Secret<String>,
    /// The last known frame of the window. Always includes the last write.
    ///
    /// This value only updates monotonically with respect to writes; in other
    /// words, we only accept reads when we know they come after the last write.
    frame_monotonic: CGRect,
    is_ax_standard: bool,
    is_resizable: bool,
    last_sent_txid: TransactionId,
    window_server_id: Option<WindowServerId>,
}

impl WindowState {
    #[must_use]
    fn next_txid(&mut self) -> TransactionId {
        self.last_sent_txid.0 += 1;
        self.last_sent_txid
    }
}

impl From<WindowInfo> for WindowState {
    fn from(info: WindowInfo) -> Self {
        WindowState {
            title: info.title,
            frame_monotonic: info.frame,
            is_ax_standard: info.is_standard,
            is_resizable: info.is_resizable,
            last_sent_txid: TransactionId::default(),
            window_server_id: info.sys_id,
        }
    }
}

impl Reactor {
    /// Spawn the reactor on a dedicated thread, co-running a `WindowServer` in
    /// the same executor. Use [`channel()`] to create reactor_tx.
    pub fn spawn(
        config: Arc<Config>,
        layout: LayoutManager,
        record: Record,
        mouse_tx: mouse::Sender,
        status_tx: status::Sender,
        group_indicators_tx: group_bars::Sender,
        reactor_tx: Sender,
        events: Receiver,
        wm_tx: wm_controller::Sender,
        ws_rx: window_server::Receiver,
        skylight_tx: window_server::SkylightSender,
    ) {
        thread::Builder::new()
            .name("reactor".to_string())
            .spawn(move || {
                let mut reactor = Reactor::new(config, layout, record, group_indicators_tx);
                reactor.mouse_tx.replace(mouse_tx);
                reactor.status_tx.replace(status_tx);
                let window_server =
                    window_server::WindowServer::new(wm_tx, reactor_tx.clone(), skylight_tx);
                Executor::run(async move {
                    tokio::join!(reactor.run(events, reactor_tx), window_server.run(ws_rx),);
                });
            })
            .unwrap();
    }

    pub fn new(
        config: Arc<Config>,
        mut layout: LayoutManager,
        mut record: Record,
        group_indicators_tx: group_bars::Sender,
    ) -> Reactor {
        // FIXME: Remove apps that are no longer running from restored state.
        record.start(&config, &layout);
        layout.set_config(&config);
        let (raise_manager_tx, _rx) = mpsc::unbounded_channel();
        Reactor {
            config,
            apps: HashMap::default(),
            layout,
            windows: HashMap::default(),
            window_ids: HashMap::default(),
            window_server_info: HashMap::default(),
            visible_windows: HashSet::default(),
            screens: vec![],
            active_screen_idx: None,
            main_window_tracker: MainWindowTracker::default(),
            in_drag: false,
            record,
            raise_manager_tx,
            mouse_tx: None,
            status_tx: None,
            group_indicators_tx: group_indicators_tx,
        }
    }

    pub async fn run(mut self, events: Receiver, events_tx: Sender) {
        let (raise_manager_tx, raise_manager_rx) = mpsc::unbounded_channel();
        self.raise_manager_tx = raise_manager_tx.clone();

        let mouse_tx = self.mouse_tx.clone();
        let reactor_task = self.run_reactor_loop(events);
        let raise_manager_task = RaiseManager::run(raise_manager_rx, events_tx, mouse_tx);

        let _ = tokio::join!(reactor_task, raise_manager_task);
    }

    async fn run_reactor_loop(mut self, mut events: Receiver) {
        // TODO: Accessibility APIs may be too slow for 120Hz; consider screen-capture animation approach.
        let tick_interval = Duration::from_secs_f64(1.0 / 120.0);
        let mut tick_timer = Timer::manual();

        loop {
            let animating = self.layout.has_active_scroll_animation();
            tokio::select! {
                event = events.recv() => {
                    let Some((span, event)) = event else { break };
                    let _guard = span.enter();
                    let was_animating = self.layout.has_active_scroll_animation();
                    self.handle_event(event);
                    if !was_animating && self.layout.has_active_scroll_animation() {
                        tick_timer.set_next_fire(Duration::ZERO);
                    }
                }
                _ = tick_timer.next(), if animating => {
                    self.layout.tick_viewports();
                    self.update_layout(None, true);
                    if self.layout.has_active_scroll_animation() {
                        tick_timer.set_next_fire(tick_interval);
                    }
                }
            }
        }
    }

    fn log_event(&self, event: &Event) {
        match event {
            // Record more noisy events as trace logs instead of debug.
            Event::WindowFrameChanged(..)
            | Event::MouseUp
            | Event::LeftMouseDown(_)
            | Event::LeftMouseDragged(_) => trace!(?event, "Event"),
            _ => debug!(?event, "Event"),
        }
    }

    fn handle_event(&mut self, event: Event) {
        self.record.on_event(&event);
        self.log_event(&event);
        let mut animation_focus_wid = None;
        let mut is_resize = false;
        let raised_window = self.main_window_tracker.handle_event(&event);
        match event {
            Event::ApplicationLaunched {
                pid,
                info,
                handle,
                visible_windows,
                window_server_info,
                is_frontmost: _,
                main_window: _,
            } => {
                self.apps.insert(pid, AppState { info, handle });
                self.update_partial_window_server_info(window_server_info);
                self.on_windows_discovered(pid, visible_windows, vec![]);
            }
            Event::StartupComplete => {
                self.send_layout_event(LayoutEvent::AppsRunningUpdated(
                    self.apps.keys().copied().collect(),
                ));
            }
            Event::ApplicationTerminated(pid) => {
                if let Some(app) = self.apps.get_mut(&pid) {
                    _ = app.handle.send(Request::Terminate);
                }
            }
            Event::ApplicationThreadTerminated(pid) => {
                self.apps.remove(&pid);
                self.send_layout_event(LayoutEvent::AppClosed(pid));
            }
            Event::ApplicationActivated(..)
            | Event::ApplicationDeactivated(..)
            | Event::ApplicationGloballyActivated(..)
            | Event::ApplicationGloballyDeactivated(..)
            | Event::ApplicationMainWindowChanged(..) => {
                // Handled by MainWindowTracker.
            }
            Event::WindowsDiscovered { pid, new, known_visible } => {
                self.on_windows_discovered(pid, new, known_visible);
            }
            Event::WindowCreated(wid, window, ws_info, mouse_state) => {
                // TODO: It's possible for a window to be on multiple spaces
                // or move spaces. (Add a test)
                // FIXME: We assume all windows are on the main screen.
                if let Some(wsid) = window.sys_id {
                    self.window_ids.insert(wsid, wid);
                }
                self.windows.insert(wid, window.clone().into());
                if let Some(info) = ws_info.clone() {
                    self.window_server_info.insert(info.id, info);
                }
                if self.window_is_tracked(wid)
                    && let Some(space) = self.best_space_for_window(&window.frame)
                    && let Some(app) = self.apps.get(&wid.pid)
                {
                    animation_focus_wid = Some(wid);
                    let info = LayoutWindowInfo {
                        bundle_id: app.info.bundle_id.clone(),
                        title: window.title.clone().into(),
                        layer: ws_info.map(|i| i.layer),
                        is_standard: window.is_standard,
                        is_resizable: window.is_resizable,
                    };
                    self.send_layout_event(LayoutEvent::WindowAdded(space, wid, info));
                }
                if mouse_state == MouseState::Down {
                    self.in_drag = true;
                    // Suppress updates while left button is pressed in case
                    // a drag is in progress.
                }
            }
            Event::WindowDestroyed(wid) => {
                self.layout.cancel_interactive_state();
                self.in_drag = false;
                if self.windows.remove(&wid).is_none() {
                    warn!("Got destroyed event for unknown window {wid:?}");
                }
                //animation_focus_wid = self.window_order.last().cloned();
                self.send_layout_event(LayoutEvent::WindowRemoved(wid));
            }
            Event::WindowFrameChanged(wid, new_frame, last_seen, requested, mouse_state) => {
                let window = self.windows.get_mut(&wid).unwrap();
                if last_seen != window.last_sent_txid {
                    // Ignore events that happened before the last time we
                    // changed the size or position of this window. Otherwise
                    // we would update the layout model incorrectly.
                    debug!(?last_seen, ?window.last_sent_txid, "Ignoring resize");
                    return;
                }
                if requested.0 {
                    // TODO: If the size is different from requested, applying a
                    // correction to the model can result in weird feedback
                    // loops, so we ignore these for now.
                    return;
                }
                let old_frame = mem::replace(&mut window.frame_monotonic, new_frame);
                if old_frame == new_frame {
                    return;
                }
                let screens = self
                    .screens
                    .iter()
                    .flat_map(|screen| Some((screen.space?, screen.frame)))
                    .collect::<Vec<_>>();
                // This event is ignored if the window is not in the layout.
                if old_frame.size != new_frame.size {
                    self.send_layout_event(LayoutEvent::WindowResized {
                        wid,
                        old_frame,
                        new_frame,
                        screens,
                    });
                    is_resize = true;
                } else if mouse_state == Some(MouseState::Down) {
                    self.in_drag = true;
                }
            }
            Event::ScreenParametersChanged {
                frames,
                spaces,
                windows,
                converter,
                scale_factors,
            } => {
                info!("screen parameters changed");
                self.screens = frames
                    .into_iter()
                    .zip(spaces.clone())
                    .zip(scale_factors)
                    .map(|((frame, space), scale_factor)| Screen { frame, space, scale_factor })
                    .collect();
                let screens = self.screens.clone();
                for screen in screens {
                    let Some(space) = screen.space else { continue };
                    self.send_layout_event(LayoutEvent::SpaceExposed(space, screen.frame.size));
                }
                self.update_complete_window_server_info(windows);
                self.update_active_screen();
                // FIXME: Update visible windows if space changed.
                // Forward the event to group_indicators. We serialize these
                // through the reactor instead of delivering directly from
                // wm_controller in order to eliminate possible races with other
                // events sent by the reactor.
                self.group_indicators_tx
                    .send(group_bars::Event::ScreenParametersChanged(spaces, converter));
            }
            Event::SpaceChanged(spaces, ws_info) => {
                if spaces.len() != self.screens.len() {
                    warn!(
                        "Ignoring space change event: we have {} spaces, but {} screens",
                        spaces.len(),
                        self.screens.len()
                    );
                    return;
                }
                self.layout.cancel_interactive_state();
                self.in_drag = false;
                info!("space changed");
                for (space, screen) in spaces.iter().zip(&mut self.screens) {
                    screen.space = *space;
                }
                let screens = self.screens.clone();
                for screen in screens {
                    let Some(space) = screen.space else {
                        continue;
                    };
                    self.send_layout_event(LayoutEvent::SpaceExposed(space, screen.frame.size));
                }
                if let Some(main_window) = self.main_window() {
                    let spaces = spaces.iter().copied().flatten().collect();
                    self.send_layout_event(LayoutEvent::WindowFocused(spaces, main_window));
                }
                self.update_complete_window_server_info(ws_info);
                self.update_active_screen();
                self.update_visible_windows();
            }
            Event::LeftMouseDown(point) => {
                if let Some(screen) = self.active_screen()
                    && let Some(space) = screen.space
                {
                    if let Some((col, win, edges)) =
                        self.layout.hit_test_scroll_edges(space, point, screen.frame, &self.config)
                    {
                        self.layout.begin_interactive_resize(col, win, edges, point);
                        self.in_drag = true;
                    } else if let Some((wid, node)) =
                        self.layout.hit_test_scroll_window(space, point, screen.frame, &self.config)
                    {
                        self.layout.begin_interactive_move(space, wid, node, point);
                        self.in_drag = true;
                    }
                }
            }
            Event::LeftMouseDragged(point) => {
                if let Some(&screen) = self.active_screen() {
                    if screen.space.is_some() {
                        if self.layout.update_interactive_resize(point, screen.frame) {
                            self.update_layout(None, true);
                        } else if self.layout.update_interactive_move(
                            point,
                            screen.frame,
                            &self.config,
                        ) {
                            self.update_layout(None, false);
                        }
                    }
                }
            }
            Event::MouseUp => {
                if self.layout.has_interactive_state() {
                    if let Some(&screen) = self.active_screen() {
                        if let Some(space) = screen.space {
                            self.layout.end_interactive_resize(space, screen.frame, &self.config);
                            self.layout.end_interactive_move(space, screen.frame, &self.config);
                        }
                    }
                }
                self.in_drag = false;
                // Now re-check the layout.
            }
            Event::MouseMovedOverWindow(wsid) => {
                let Some(&wid) = self.window_ids.get(&wsid) else { return };
                let Some(window) = self.windows.get(&wid) else { return };
                let Some(to_space) = self.best_space_for_window(&window.frame_monotonic) else {
                    // The space is disabled.
                    return;
                };
                let current_main = match (self.main_window_space(), self.main_window()) {
                    (Some(space), Some(id)) => Some((space, id)),
                    _ => None,
                };
                self.send_layout_event_from_mouse(
                    LayoutEvent::MouseMovedOverWindow {
                        over: (to_space, wid),
                        current_main,
                    },
                    true,
                );
            }
            Event::RaiseCompleted { window_id, sequence_id } => {
                let msg = raise::Event::RaiseCompleted { window_id, sequence_id };
                _ = self.raise_manager_tx.send((Span::current(), msg));
            }
            Event::RaiseTimeout { sequence_id } => {
                let msg = raise::Event::RaiseTimeout { sequence_id };
                _ = self.raise_manager_tx.send((Span::current(), msg));
            }
            Event::ScrollWheel { delta_x, delta_y, alt_held } => {
                if !self.config.settings.experimental.scroll.enable {
                    return;
                }
                // TODO: Make the modifier key configurable.
                if !alt_held {
                    return;
                }
                if let Some(&screen) = self.active_screen() {
                    if let Some(space) = screen.space {
                        let scroll_config = &self.config.settings.experimental.scroll;
                        let delta = if delta_x != 0.0 { delta_x } else { delta_y };
                        let response = self.layout.handle_scroll_wheel(
                            space,
                            delta,
                            &screen.frame,
                            scroll_config,
                        );
                        self.handle_layout_response(response);
                    }
                }
            }
            Event::Command(Command::Layout(cmd)) => {
                info!(?cmd);
                let visible_spaces =
                    self.screens.iter().flat_map(|screen| screen.space).collect::<Vec<_>>();
                let response =
                    self.layout.handle_command(self.main_window_space(), &visible_spaces, cmd);
                self.handle_layout_response(response);
            }
            Event::Command(Command::Metrics(cmd)) => log::handle_command(cmd),
            Event::Command(Command::Reactor(ReactorCommand::Debug)) => {
                for screen in &self.screens {
                    if let Some(space) = screen.space {
                        self.layout.debug_tree_desc(space, "", true);
                    }
                }
            }
            Event::Command(Command::Reactor(ReactorCommand::Serialize)) => {
                println!("{}", self.layout.serialize_to_string());
            }
            Event::Command(Command::Reactor(ReactorCommand::SaveAndExit)) => {
                info!("SaveAndExit command received");
                match self.layout.save(crate::config::restore_file()) {
                    Ok(()) => std::process::exit(0),
                    Err(e) => {
                        error!("Could not save layout: {e}");
                        std::process::exit(3);
                    }
                }
            }
            Event::ConfigChanged(config) => {
                self.layout.set_config(&config);
                self.config = config;
            }
        }
        if let Some(raised_window) = raised_window {
            let spaces = self.screens.iter().flat_map(|screen| screen.space).collect();
            self.send_layout_event(LayoutEvent::WindowFocused(spaces, raised_window));
            self.update_active_screen();
        }
        if !self.in_drag {
            self.update_layout(animation_focus_wid, is_resize);
        }
    }

    fn update_complete_window_server_info(&mut self, ws_info: Vec<WindowServerInfo>) {
        self.visible_windows.clear();
        self.update_partial_window_server_info(ws_info);
    }

    fn update_partial_window_server_info(&mut self, ws_info: Vec<WindowServerInfo>) {
        self.visible_windows.extend(ws_info.iter().map(|info| info.id));
        for info in ws_info.iter().filter(|i| i.layer == 0) {
            let Some(wid) = self.window_ids.get(&info.id) else {
                continue;
            };
            let Some(window) = self.windows.get_mut(wid) else {
                continue;
            };
            // Assume this update comes from after the last write. The window
            // is on a different space (unless it's on all spaces) and
            // there's no way to order it with respect to our writes anyway.
            window.frame_monotonic = info.frame;
        }
        self.window_server_info.extend(ws_info.into_iter().map(|info| (info.id, info)));
    }

    fn update_visible_windows(&mut self) {
        // TODO: Do this correctly/more optimally using CGWindowListCopyWindowInfo
        // (see notes for on_windows_discovered below).
        for app in self.apps.values_mut() {
            // Errors mean the app terminated (and a termination event
            // is coming); ignore.
            _ = app.handle.send(Request::GetVisibleWindows);
        }
    }

    fn on_windows_discovered(
        &mut self,
        pid: pid_t,
        new: Vec<(WindowId, WindowInfo)>,
        _known_visible: Vec<WindowId>,
    ) {
        // Note that we rely on the window server info, not accessibility, to
        // tell us which windows are visible.
        //
        // The accessibility APIs report that there are no visible windows when
        // at a login screen, for instance, but there is not a corresponding
        // system notification to use as context. Even if there were, lining
        // them up with the responses we get from the app would be unreliable.
        //
        // We therefore do not let accessibility `.windows()` results remove
        // known windows from the visible list. Doing so incorrectly would cause
        // us to destroy the layout. We do wait for windows to become initially
        // known to accesibility before adding them to the layout, but that is
        // not generally problematic.
        //
        // TODO: Notice when returning from the login screen and ask again for
        // undiscovered windows.
        self.window_ids
            .extend(new.iter().flat_map(|(wid, info)| info.sys_id.map(|wsid| (wsid, *wid))));
        self.windows.extend(new.into_iter().map(|(wid, info)| (wid, info.into())));
        let mut app_windows: BTreeMap<SpaceId, Vec<(WindowId, LayoutWindowInfo)>> = BTreeMap::new();
        let app = self.apps.get(&pid);
        for wid in self
            .visible_windows
            .iter()
            .flat_map(|wsid| self.window_ids.get(wsid).copied())
            .filter(|wid| wid.pid == pid)
            .filter(|wid| self.window_is_tracked(*wid))
        {
            let Some(window) = self.windows.get(&wid) else { continue };
            let Some(space) = self.best_space_for_window(&window.frame_monotonic) else {
                continue;
            };
            let layout_info = LayoutWindowInfo {
                bundle_id: app.and_then(|a| a.info.bundle_id.clone()),
                title: window.title.clone().into(),
                layer: window
                    .window_server_id
                    .and_then(|wsid| self.window_server_info.get(&wsid))
                    .map(|info| info.layer),
                is_standard: window.is_ax_standard,
                is_resizable: window.is_resizable,
            };
            app_windows.entry(space).or_default().push((wid, layout_info));
        }
        let screens = self.screens.clone();
        for screen in screens {
            let Some(space) = screen.space else { continue };
            self.send_layout_event(LayoutEvent::WindowsOnScreenUpdated(
                space,
                pid,
                app_windows.remove(&space).unwrap_or_default(),
            ));
        }
        // If it's possible we just added the main window to the layout, make
        // sure the layout knows it's focused.
        if let Some(main_window) = self.main_window() {
            if main_window.pid == pid {
                let spaces = self.screens.iter().flat_map(|screen| screen.space).collect();
                self.send_layout_event(LayoutEvent::WindowFocused(spaces, main_window));
            }
        }
    }

    fn best_screen_idx_for_window(&self, frame: &CGRect) -> Option<usize> {
        self.screens
            .iter()
            .enumerate()
            .max_by_key(|(_, s)| s.frame.intersection(frame).area() as i64)
            .map(|(idx, _)| idx)
    }

    fn best_space_for_window(&self, frame: &CGRect) -> Option<SpaceId> {
        self.screens[self.best_screen_idx_for_window(frame)?].space
    }

    fn update_active_screen(&mut self) {
        let changed = (|| {
            let frame = self.windows.get(&self.main_window()?)?.frame_monotonic;
            let screen = self.best_screen_idx_for_window(&frame)?;
            Some(self.active_screen_idx.replace(screen as u16) != Some(screen as u16))
        })();
        if changed.unwrap_or(false)
            && let Some(status_tx) = &mut self.status_tx
        {
            status_tx.send(status::Event::FocusedScreenChanged);
        }
    }

    fn active_screen(&self) -> Option<&Screen> {
        self.screens.get(self.active_screen_idx.unwrap_or(0) as usize)
    }

    fn window_is_tracked(&self, _id: WindowId) -> bool {
        // For now we track all windows in the reactor and let the LayoutManager
        // decide what to keep.
        true
    }

    fn send_layout_event(&mut self, event: LayoutEvent) {
        self.send_layout_event_from_mouse(event, false);
    }

    fn send_layout_event_from_mouse(&mut self, event: LayoutEvent, from_mouse: bool) {
        let response = self.layout.handle_event(event);
        self.handle_layout_response_from_mouse(response, from_mouse);
        for space in self.screens.iter().flat_map(|screen| screen.space) {
            self.layout.debug_tree_desc(space, "after event", false);
        }
    }

    fn handle_layout_response(&mut self, response: layout::EventResponse) {
        self.handle_layout_response_from_mouse(response, false);
    }

    fn handle_layout_response_from_mouse(
        &mut self,
        response: layout::EventResponse,
        from_mouse: bool,
    ) {
        let layout::EventResponse { raise_windows, focus_window } = response;
        if raise_windows.is_empty() && focus_window.is_none() {
            return;
        }

        let mut app_handles = HashMap::default();
        for &wid in raise_windows.iter().chain(&focus_window) {
            if let Some(app) = self.apps.get(&wid.pid) {
                app_handles.insert(wid.pid, app.handle.clone());
            }
        }

        let mut windows_by_app_and_screen = HashMap::default();
        for &wid in &raise_windows {
            let Some(window) = self.windows.get(&wid) else { continue };
            windows_by_app_and_screen
                .entry((wid.pid, self.best_space_for_window(&window.frame_monotonic)))
                .or_insert(vec![])
                .push(wid);
        }

        let focus_window_with_warp = focus_window.map(|wid| {
            let warp = if self.config.settings.mouse_follows_focus && !from_mouse {
                self.windows.get(&wid).map(|w| w.frame_monotonic.mid())
            } else {
                // We disable warp above if the event itself is caused by mouse
                // movement.
                None
            };
            (wid, warp)
        });

        let msg = raise::Event::RaiseRequest(RaiseRequest {
            raise_windows: windows_by_app_and_screen.into_values().collect(),
            focus_window: focus_window_with_warp,
            app_handles,
        });

        _ = self.raise_manager_tx.send((Span::current(), msg));
    }

    /// The main window of the active app, if any.
    fn main_window(&self) -> Option<WindowId> {
        self.main_window_tracker.main_window()
    }

    fn main_window_space(&self) -> Option<SpaceId> {
        // TODO: Optimize this with a cache or something.
        self.best_space_for_window(&self.windows.get(&self.main_window()?)?.frame_monotonic)
    }

    #[instrument(skip(self), fields())]
    pub fn update_layout(&mut self, new_wid: Option<WindowId>, skip_anim: bool) {
        let main_window = self.main_window();
        trace!(?main_window);
        let mut anim = Animation::new();
        for &screen in &self.screens {
            let Some(space) = screen.space else { continue };
            if !skip_anim {
                self.layout.update_viewport_for_focus(space, screen.frame, &self.config);
            }
            let (result, groups) =
                self.layout.calculate_layout_and_groups(space, screen.frame, &self.config);

            self.group_indicators_tx
                .send(group_bars::Event::GroupsUpdated { space_id: space, groups });

            for &(wid, target_frame) in &result {
                let Some(window) = self.windows.get_mut(&wid) else {
                    // If we restored a saved state the window may not be available yet.
                    continue;
                };
                let target_frame = round_to_physical(target_frame, screen.scale_factor);
                let current_frame = window.frame_monotonic;
                if target_frame.same_as(current_frame) {
                    continue;
                }
                let Some(app) = self.apps.get(&wid.pid) else {
                    continue;
                };
                let txid = window.next_txid();
                trace!(?wid, ?current_frame, ?target_frame);
                let is_new = Some(wid) == new_wid;
                anim.add_window(&app.handle, wid, current_frame, target_frame, is_new, txid);
                window.frame_monotonic = target_frame;
            }
        }
        // If the user is doing something with the mouse we don't want to
        // animate on top of that.
        if skip_anim || !self.config.settings.animate || self.layout.has_active_scroll_animation() {
            anim.skip_to_end();
        } else {
            anim.run();
        }
    }
}

#[cfg(test)]
pub mod tests {
    use itertools::Itertools;
    use objc2_core_foundation::{CGPoint, CGSize};
    use test_log::test;

    use super::testing::*;
    use super::*;
    use crate::actor::app::Request;
    use crate::actor::layout::LayoutManager;
    use crate::model::Direction;
    use crate::sys::window_server::WindowServerId;

    #[test]
    fn it_ignores_stale_resize_events() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new());
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))],
            spaces: vec![Some(SpaceId::new(1))],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            windows: vec![],
        });

        reactor.handle_events(apps.make_app(1, make_windows(2)));
        reactor.handle_event(Event::StartupComplete);
        let requests = apps.requests();
        assert!(!requests.is_empty());
        let events_1 = apps.simulate_events_for_requests(requests);

        reactor.handle_events(apps.make_app(2, make_windows(2)));
        assert!(!apps.requests().is_empty());

        for event in dbg!(events_1) {
            reactor.handle_event(event);
        }
        let requests = apps.requests();
        assert!(
            requests.is_empty(),
            "got requests when there should have been none: {requests:?}"
        );
    }

    #[test]
    fn it_sends_writes_when_stale_read_state_looks_same_as_written_state() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new());
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))],
            spaces: vec![Some(SpaceId::new(1))],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            windows: vec![],
        });

        reactor.handle_events(apps.make_app(1, make_windows(2)));
        reactor.handle_event(Event::StartupComplete);
        let events_1 = apps.simulate_events();
        let state_1 = apps.windows.clone();
        assert!(!state_1.is_empty());

        for event in events_1 {
            reactor.handle_event(event);
        }
        assert!(apps.requests().is_empty());

        reactor.handle_events(apps.make_app(2, make_windows(1)));
        let _events_2 = apps.simulate_events();

        reactor.handle_event(Event::WindowDestroyed(WindowId::new(2, 1)));
        let _events_3 = apps.simulate_events();
        let state_3 = apps.windows;

        // These should be the same, because we should have resized the first
        // two windows both at the beginning, and at the end when the third
        // window was destroyed.
        for (wid, state) in dbg!(state_1) {
            assert!(state_3.contains_key(&wid), "{wid:?} not in {state_3:#?}");
            assert_eq!(state.frame, state_3[&wid].frame);
        }
    }

    #[test]
    fn sends_writes_same_as_last_written_state_if_changed_externally() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new());
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))],
            spaces: vec![Some(SpaceId::new(1))],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            windows: vec![],
        });

        reactor.handle_events(apps.make_app(1, make_windows(2)));
        reactor.handle_event(Event::StartupComplete);
        let events_1 = apps.simulate_events();
        let state_1 = apps.windows.clone();
        assert!(!state_1.is_empty());

        for event in events_1 {
            reactor.handle_event(event);
        }
        assert!(apps.requests().is_empty());

        // Move a window in an invalid way.
        let wid = WindowId::new(1, 1);
        let old_frame = state_1[&wid].frame;
        reactor.handle_event(Event::WindowFrameChanged(
            wid,
            CGRect::new(
                CGPoint::new(old_frame.origin.x, old_frame.origin.y + 10.),
                old_frame.size,
            ),
            state_1[&wid].last_seen_txid,
            Requested(false),
            None,
        ));

        let requests = apps.requests();
        assert!(!requests.is_empty());
        let _events_2 = apps.simulate_events_for_requests(requests);
        assert_eq!(apps.windows[&wid].frame, old_frame);
    }

    #[test]
    fn it_responds_to_resizes() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new());
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))],
            spaces: vec![Some(SpaceId::new(1))],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            windows: vec![],
        });

        reactor.handle_events(apps.make_app(1, make_windows(3)));
        reactor.handle_event(Event::StartupComplete);

        let events = apps.simulate_events();
        let windows = apps.windows.clone();
        for event in events {
            reactor.handle_event(event);
        }
        assert!(
            apps.requests().is_empty(),
            "reactor shouldn't react to unsurprising events"
        );

        // Resize the right edge of the middle window.
        let resizing = WindowId::new(1, 2);
        let window = &apps.windows[&resizing];
        let frame = CGRect::new(
            window.frame.origin,
            CGSize::new(window.frame.size.width + 10., window.frame.size.height),
        );
        reactor.handle_event(Event::WindowFrameChanged(
            resizing,
            frame,
            window.last_seen_txid,
            Requested(false),
            None,
        ));

        // Expect the next window to be resized.
        let next = WindowId::new(1, 3);
        let old_frame = windows[&next].frame;
        let requests = apps.requests();
        assert!(!requests.is_empty());
        let _events = apps.simulate_events_for_requests(requests);
        assert_ne!(old_frame, apps.windows[&next].frame);
    }

    #[test]
    fn it_manages_windows_on_enabled_spaces() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new());
        let full_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![full_screen],
            spaces: vec![Some(SpaceId::new(1))],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            windows: vec![],
        });

        reactor.handle_events(apps.make_app(1, make_windows(1)));
        reactor.handle_event(Event::StartupComplete);

        let _events = apps.simulate_events();
        assert_eq!(
            full_screen,
            apps.windows.get(&WindowId::new(1, 1)).expect("Window was not resized").frame,
        );
    }

    #[test]
    fn it_selects_the_main_window_on_space_enable() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new());
        let full_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        let ws_info = (1..=2)
            .map(|id| WindowServerInfo {
                id: WindowServerId::new(id),
                pid: 1,
                layer: 0,
                frame: CGRect::ZERO,
            })
            .collect::<Vec<_>>();
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![full_screen],
            spaces: vec![None],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            windows: ws_info.clone(),
        });

        reactor.handle_events(apps.make_app_with_opts(
            1,
            make_windows(2),
            Some(WindowId::new(1, 1)),
            true,
            true,
        ));
        reactor.handle_event(Event::StartupComplete);
        reactor.handle_event(Event::ApplicationGloballyActivated(1));
        reactor.handle_events(apps.simulate_events());

        reactor.handle_event(Event::SpaceChanged(vec![Some(SpaceId::new(1))], ws_info));
        reactor.handle_events(apps.simulate_events());
        assert_eq!(
            reactor.layout.selected_window(SpaceId::new(1)),
            Some(WindowId::new(1, 1))
        );
    }

    #[test]
    fn it_ignores_windows_on_disabled_spaces() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new());
        let full_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![full_screen],
            spaces: vec![None],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            windows: vec![],
        });

        reactor.handle_events(apps.make_app(1, make_windows(1)));

        let state_before = apps.windows.clone();
        let _events = apps.simulate_events();
        assert_eq!(state_before, apps.windows, "Window should not have been moved",);

        // Make sure it doesn't choke on destroyed events for ignored windows.
        reactor.handle_event(Event::WindowDestroyed(WindowId::new(1, 1)));
        reactor.handle_event(Event::WindowCreated(
            WindowId::new(1, 2),
            make_window(2),
            None,
            MouseState::Up,
        ));
        reactor.handle_event(Event::WindowDestroyed(WindowId::new(1, 2)));
    }

    #[test]
    fn it_keeps_discovered_windows_on_their_initial_screen() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new());
        let screen1 = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        let screen2 = CGRect::new(CGPoint::new(1000., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![screen1, screen2],
            spaces: vec![Some(SpaceId::new(1)), Some(SpaceId::new(2))],
            scale_factors: vec![2.0, 2.0],
            converter: CoordinateConverter::default(),
            windows: vec![],
        });

        let mut windows = make_windows(2);
        windows[1].frame.origin = CGPoint::new(1100., 100.);
        reactor.handle_events(apps.make_app(1, windows));

        let _events = apps.simulate_events();
        assert_eq!(
            screen1,
            apps.windows.get(&WindowId::new(1, 1)).expect("Window was not resized").frame,
        );
        assert_eq!(
            screen2,
            apps.windows.get(&WindowId::new(1, 2)).expect("Window was not resized").frame,
        );
    }

    #[test]
    fn it_ignores_windows_on_nonzero_layers() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new());
        let full_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![full_screen],
            spaces: vec![Some(SpaceId::new(1))],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            windows: vec![WindowServerInfo {
                id: WindowServerId::new(1),
                pid: 1,
                layer: 10,
                frame: CGRect::ZERO,
            }],
        });

        reactor.handle_events(apps.make_app_with_opts(1, make_windows(1), None, true, false));

        let state_before = apps.windows.clone();
        let _events = apps.simulate_events();
        assert_eq!(state_before, apps.windows, "Window should not have been moved",);

        // Make sure it doesn't choke on destroyed events for ignored windows.
        reactor.handle_event(Event::WindowDestroyed(WindowId::new(1, 1)));
        reactor.handle_event(Event::WindowCreated(
            WindowId::new(1, 2),
            make_window(2),
            None,
            MouseState::Up,
        ));
        reactor.handle_event(Event::WindowDestroyed(WindowId::new(1, 2)));
    }

    #[test]
    fn handle_layout_response_groups_windows_by_app_and_screen() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new());
        let (raise_manager_tx, mut raise_manager_rx) = mpsc::unbounded_channel();
        reactor.raise_manager_tx = raise_manager_tx;

        let screen1 = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        let screen2 = CGRect::new(CGPoint::new(1000., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![screen1, screen2],
            spaces: vec![Some(SpaceId::new(1)), Some(SpaceId::new(2))],
            scale_factors: vec![2.0, 2.0],
            converter: CoordinateConverter::default(),
            windows: vec![],
        });

        reactor.handle_events(apps.make_app(1, make_windows(2)));

        let mut windows = make_windows(2);
        windows[1].frame.origin = CGPoint::new(1100., 100.);
        reactor.handle_events(apps.make_app(2, windows));

        let _events = apps.simulate_events();
        while raise_manager_rx.try_recv().is_ok() {}

        reactor.handle_layout_response(layout::EventResponse {
            raise_windows: vec![
                WindowId::new(1, 1),
                WindowId::new(1, 2),
                WindowId::new(2, 1),
                WindowId::new(2, 2),
            ],
            focus_window: None,
        });
        let msg = raise_manager_rx.try_recv().expect("Should have sent an event").1;
        match msg {
            raise::Event::RaiseRequest(RaiseRequest {
                raise_windows,
                focus_window,
                app_handles: _,
            }) => {
                let raise_windows: HashSet<Vec<WindowId>> = raise_windows.into_iter().collect();
                let expected = [
                    vec![WindowId::new(1, 1), WindowId::new(1, 2)],
                    vec![WindowId::new(2, 1)],
                    vec![WindowId::new(2, 2)],
                ]
                .into_iter()
                .collect();
                assert_eq!(raise_windows, expected);
                assert!(focus_window.is_none());
            }
            _ => panic!("Unexpected event: {msg:?}"),
        }
    }

    #[test]
    fn handle_layout_response_includes_handles_for_raise_and_focus_windows() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new());
        let (raise_manager_tx, mut raise_manager_rx) = mpsc::unbounded_channel();
        reactor.raise_manager_tx = raise_manager_tx;

        reactor.handle_events(apps.make_app(1, make_windows(1)));
        reactor.handle_events(apps.make_app(2, make_windows(1)));

        let _events = apps.simulate_events();
        while raise_manager_rx.try_recv().is_ok() {}
        reactor.handle_layout_response(layout::EventResponse {
            raise_windows: vec![WindowId::new(1, 1)],
            focus_window: Some(WindowId::new(2, 1)),
        });
        let msg = raise_manager_rx.try_recv().expect("Should have sent an event").1;
        match msg {
            raise::Event::RaiseRequest(RaiseRequest { app_handles, .. }) => {
                assert!(app_handles.contains_key(&1));
                assert!(app_handles.contains_key(&2));
            }
            _ => panic!("Unexpected event: {msg:?}"),
        }
    }

    #[test]
    fn it_preserves_layout_after_login_screen() {
        // TODO: This would be better tested with a more complete simulation.
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new());
        let space = SpaceId::new(1);
        let full_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![full_screen],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            windows: vec![],
        });

        reactor.handle_events(apps.make_app_with_opts(
            1,
            make_windows(3),
            Some(WindowId::new(1, 1)),
            true,
            true,
        ));
        reactor.handle_event(Event::StartupComplete);
        reactor.handle_event(Event::ApplicationGloballyActivated(1));
        apps.simulate_until_quiet(&mut reactor);
        let default = reactor.layout.calculate_layout(space, full_screen, &reactor.config);

        assert!(reactor.layout.selected_window(space).is_some());
        reactor.handle_event(Event::Command(Command::Layout(LayoutCommand::MoveNode(
            Direction::Up,
        ))));
        apps.simulate_until_quiet(&mut reactor);
        let modified = reactor.layout.calculate_layout(space, full_screen, &reactor.config);
        assert_ne!(default, modified);

        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![CGRect::ZERO],
            spaces: vec![None],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            windows: vec![],
        });
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![full_screen],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            windows: (1..=3)
                .map(|n| WindowServerInfo {
                    pid: 1,
                    id: WindowServerId::new(n),
                    layer: 0,
                    frame: CGRect::ZERO,
                })
                .collect(),
        });
        let requests = apps.requests();
        for request in requests {
            match request {
                Request::GetVisibleWindows => {
                    // Simulate the login screen condition: No windows are
                    // considered visible by the accessibility API, but they are
                    // from the window server API in the event above.
                    reactor.handle_event(Event::WindowsDiscovered {
                        pid: 1,
                        new: vec![],
                        known_visible: vec![],
                    });
                }
                req => {
                    let events = apps.simulate_events_for_requests(vec![req]);
                    for event in events {
                        reactor.handle_event(event);
                    }
                }
            }
        }
        apps.simulate_until_quiet(&mut reactor);

        assert_eq!(
            reactor.layout.calculate_layout(space, full_screen, &reactor.config),
            modified
        );
    }

    #[test]
    fn it_fixes_window_sizes_after_screen_config_changes() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new());
        let full_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![full_screen],
            spaces: vec![Some(SpaceId::new(1))],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            windows: vec![],
        });

        reactor.handle_events(apps.make_app(1, make_windows(1)));
        reactor.handle_event(Event::StartupComplete);

        let _events = apps.simulate_events();
        assert_eq!(
            full_screen,
            apps.windows.get(&WindowId::new(1, 1)).expect("Window was not resized").frame,
        );

        // Simulate the system resizing a window after it recognizes an old
        // configurations. Resize events are not sent in this case.
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![
                full_screen,
                CGRect::new(CGPoint::new(1000., 0.), CGSize::new(1000., 1000.)),
            ],
            spaces: vec![Some(SpaceId::new(1)), None],
            scale_factors: vec![2.0, 2.0],
            converter: CoordinateConverter::default(),
            windows: vec![WindowServerInfo {
                id: WindowServerId::new(1),
                pid: 1,
                layer: 0,
                frame: CGRect::new(CGPoint::new(500., 0.), CGSize::new(500., 500.)),
            }],
        });

        let _events = apps.simulate_events();
        assert_eq!(
            full_screen,
            apps.windows.get(&WindowId::new(1, 1)).expect("Window was not resized").frame,
        );
    }

    #[test]
    fn it_doesnt_crash_after_main_window_closes() {
        use Direction::*;
        use Event::*;
        use LayoutCommand::*;

        use super::Command::*;
        use super::Reactor;
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new());
        let space = SpaceId::new(1);
        reactor.handle_event(ScreenParametersChanged {
            frames: vec![CGRect::ZERO],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            windows: vec![],
        });
        assert_eq!(None, reactor.main_window());

        reactor.handle_event(ApplicationGloballyActivated(1));
        reactor.handle_events(apps.make_app_with_opts(
            1,
            make_windows(2),
            Some(WindowId::new(1, 1)),
            true,
            true,
        ));

        reactor.handle_event(WindowDestroyed(WindowId::new(1, 1)));
        reactor.handle_event(Command(Layout(MoveFocus(Left))));
    }

    #[test]
    fn it_removes_terminated_app_windows_on_startup_complete() {
        use Event::*;

        let mut apps = Apps::new();
        let space = SpaceId::new(1);
        let full_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));

        // First reactor: simulate the state before shutdown with three apps running
        let mut reactor1 = Reactor::new_for_test(LayoutManager::new());
        reactor1.handle_event(ScreenParametersChanged {
            frames: vec![full_screen],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            windows: vec![],
        });
        reactor1.handle_events(apps.make_app(1, make_windows(2)));
        reactor1.handle_events(apps.make_app(2, make_windows(2)));
        reactor1.handle_events(apps.make_app(3, make_windows(1)));
        apps.simulate_until_quiet(&mut reactor1);

        // Verify all 5 windows are in the layout
        let layout_before = reactor1.layout.calculate_layout(space, full_screen, &reactor1.config);
        assert_eq!(layout_before.len(), 5, "Expected 5 windows before shutdown");

        // Serialize the layout to simulate saving state before shutdown
        let serialized_layout = ron::ser::to_string(&reactor1.layout).unwrap();

        // Second reactor: simulate restore after reboot, where app 2 was terminated
        // and doesn't launch again
        let restored_layout: LayoutManager = ron::de::from_str(&serialized_layout).unwrap();
        let mut apps2 = Apps::new();
        let mut reactor2 = Reactor::new_for_test(restored_layout);
        reactor2.handle_event(ScreenParametersChanged {
            frames: vec![full_screen],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            windows: vec![],
        });
        // Only apps 1 and 3 launch during restore (app 2 was terminated between save and restore)
        reactor2.handle_events(apps2.make_app(1, make_windows(2)));
        reactor2.handle_events(apps2.make_app(3, make_windows(1)));
        apps2.simulate_until_quiet(&mut reactor2);

        // Before StartupComplete, the layout still contains ghost nodes for app 2's windows
        let layout_before_cleanup =
            reactor2.layout.calculate_layout(space, full_screen, &reactor2.config);
        assert_eq!(layout_before_cleanup.len(), 5);

        // Send StartupComplete to trigger cleanup of terminated app windows
        reactor2.handle_event(StartupComplete);

        // After StartupComplete, verify that windows from terminated app 2 are removed
        // but windows from running apps 1 and 3 remain
        let windows_after = reactor2
            .layout
            .calculate_layout(space, full_screen, &reactor2.config)
            .into_iter()
            .map(|(wid, _)| wid)
            .sorted()
            .collect_vec();

        assert_eq!(
            windows_after,
            &[
                WindowId::new(1, 1),
                WindowId::new(1, 2),
                WindowId::new(3, 1),
            ]
        );
    }

    #[test]
    fn no_scroll_animation_when_idle() {
        let mut reactor = Reactor::new_for_test(LayoutManager::new());
        let space = SpaceId::new(1);
        let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![screen],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            windows: vec![],
        });

        let mut apps = Apps::new();
        reactor.handle_events(apps.make_app(1, make_windows(2)));
        reactor.handle_event(Event::StartupComplete);
        apps.simulate_until_quiet(&mut reactor);

        assert!(
            !reactor.layout.has_active_scroll_animation(),
            "timer should be dormant when no scroll animation is active"
        );
    }
}
