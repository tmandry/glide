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
mod restore_snapshots;
#[cfg(test)]
mod testing;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{mem, thread};

use animation::{Animation, AnimationManager, Message as AnimationMessage};
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
use crate::actor::raise::{self, RaiseManager, RaiseRequest};
use crate::actor::saved_state::SavedState;
use crate::actor::space_manager::{SpaceManager, SpaceState};
use crate::actor::{group_bars, space_manager, status, window_server, wm_controller};
use crate::collections::{HashMap, HashSet};
use crate::config::Config;
use crate::log::{self, MetricsCommand};
use crate::sys::event::MouseState;
use crate::sys::executor::Executor;
use crate::sys::geometry::{CGRectDef, CGRectExt, SameAs, round_to_physical};
use crate::sys::screen::{CoordinateConverter, SpaceId};
use crate::sys::timer::Timer;
use crate::sys::window_server::{WindowServerId, WindowServerInfo, WindowsOnScreen};

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
        on_screen: WindowsOnScreen,
    },

    /// The current space changed.
    ///
    /// There is one SpaceId per screen in the last ScreenParametersChanged
    /// event. `None` in the SpaceId vec disables managing windows on that
    /// screen until the next space change.
    ///
    /// WindowsOnScreen is included to avoid doing two updates in rapid
    /// succession. If there are windows we know were destroyed in the new
    /// space we'll start rearranging things, only to do so again if we
    /// discover newly added windows.
    ///
    /// TODO: In the future WindowsOnScreenUpdated should include a mapping
    /// of SpaceId to window list, and Reactor would maintain a list per
    /// space. Then we can update the windows on screen for the space before
    /// sending SpaceChanged.
    SpaceChanged(Vec<Option<SpaceId>>, WindowsOnScreen),

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
    WindowCreated(WindowId, WindowInfo, MouseState),

    /// Updated list of windows visible on screen from the window server.
    ///
    /// Sent after space changes, app launches, and window creation. When
    /// `pid` is set, only that app's windows changed.
    WindowsOnScreenUpdated {
        pid: Option<pid_t>,
        on_screen: WindowsOnScreen,
    },

    // TODO: Consider replacing with WindowsOnScreenUpdated.
    WindowBecameVisible(WindowId),
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
    ///
    /// The second field is the process the window server was routing keyboard
    /// events to when the mouse moved, if it could be read. It is read in the
    /// mouse actor so it describes the same moment as the mouse position.
    MouseMovedOverWindow(WindowServerId, #[serde(default)] Option<pid_t>),

    /// A raise request completed. Used by the raise manager to track when
    /// all raise requests in a sequence have finished.
    RaiseCompleted {
        window_id: WindowId,
        sequence_id: u64,
    },

    /// A raise request failed. None of its windows will be raised.
    ///
    /// `quiet` is the one the request was made with. A non-quiet request was
    /// meant to move the focus, so the layout has to be reconciled with the
    /// main window that is actually focused.
    RaiseRequestFailed {
        windows: Vec<WindowId>,
        sequence_id: u64,
        quiet: Quiet,
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

    /// Save the state along with the space state, then exit.
    SaveAndExit(SpaceState),
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
}

pub struct Reactor {
    config: Arc<Config>,
    apps: HashMap<pid_t, AppState>,
    layout: LayoutManager,
    /// One-shot frame targets requested by layout transitions. They are merged
    /// into the next animation alongside the continuously calculated layout.
    pending_frame_overrides: HashMap<WindowId, CGRect>,
    windows: HashMap<WindowId, WindowState>,
    window_server_info: HashMap<WindowServerId, WindowServerInfo>,
    window_ids: HashMap<WindowServerId, WindowId>,
    visible_windows: HashSet<WindowServerId>,
    screens: Vec<Screen>,
    active_screen_idx: Option<u16>,
    main_window_tracker: MainWindowTracker,
    in_drag: bool,
    /// The window the user is currently resizing with the mouse, if any.
    ///
    /// We don't write frames to this window until the resize ends, since a
    /// write mid-drag fights the user's mouse.
    resizing_window: Option<WindowId>,
    /// Recent attempts to write a frame to a window, used to stop fighting apps
    /// that move their windows back.
    frame_attempts: HashMap<WindowId, FrameAttempt>,
    record: Record,
    raise_manager_tx: raise::Sender,
    animation_tx: Option<animation::Sender>,
    mouse_tx: Option<mouse::Sender>,
    status_tx: Option<status::Sender>,
    group_indicators_tx: group_bars::Sender,
}

/// How many times in a row we write the same frame to a window before giving
/// up, and how long a pause resets the count.
const MAX_FRAME_ATTEMPTS: u32 = 5;
const FRAME_ATTEMPT_RESET: Duration = Duration::from_secs(2);

#[derive(Debug)]
struct FrameAttempt {
    target: CGRect,
    count: u32,
    last: Instant,
}

#[derive(Debug)]
struct AppState {
    #[allow(unused)]
    pub info: AppInfo,
    pub handle: AppThreadHandle,
}

/// Extra information about the event a layout response came from.
#[derive(Default)]
struct ResponseContext {
    /// The windows visible on screen, front to back, from a snapshot taken with
    /// the event. `None` if the event didn't come with one.
    ///
    /// Only valid for the event it arrived with: raising windows changes the
    /// order and the window server doesn't report the result.
    visible_window_order: Option<Vec<WindowServerId>>,
    /// Whether the event came from the mouse moving, in which case we don't
    /// warp the mouse to the newly focused window.
    from_mouse: bool,
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
    ax_role: String,
    ax_subrole: Option<String>,
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
            ax_role: info.ax_role,
            ax_subrole: info.ax_subrole,
            last_sent_txid: TransactionId::default(),
            window_server_id: info.sys_id,
        }
    }
}

impl Reactor {
    /// Spawn the reactor on a dedicated thread, co-running a `WindowServer` in
    /// the same executor. Use [`channel()`] to create reactor_tx.
    #[expect(clippy::too_many_arguments)]
    pub fn spawn(
        config: Arc<Config>,
        one_space: bool,
        layout: LayoutManager,
        record: Record,
        mouse_tx: mouse::Sender,
        status_tx: status::Sender,
        group_indicators_tx: group_bars::Sender,
        reactor_tx: Sender,
        events: Receiver,
        wm_tx: wm_controller::Sender,
        ws_tx: window_server::Sender,
        ws_rx: window_server::Receiver,
        sm_tx: space_manager::Sender,
        sm_rx: space_manager::Receiver,
        skylight_tx: window_server::SkylightSender,
        restored_spaces: Option<SpaceState>,
    ) {
        thread::Builder::new()
            .name("reactor".to_string())
            .spawn(move || {
                let mut reactor =
                    Reactor::new(config.clone(), layout, record, group_indicators_tx.clone());
                reactor.mouse_tx.replace(mouse_tx.clone());
                reactor.status_tx.replace(status_tx.clone());
                let space_manager = SpaceManager::new(
                    one_space,
                    config,
                    reactor_tx.clone(),
                    ws_tx,
                    wm_tx.clone(),
                    status_tx,
                    group_indicators_tx,
                    mouse_tx,
                    restored_spaces,
                );
                let window_server = window_server::WindowServer::new(sm_tx, wm_tx, skylight_tx);
                Executor::run(async move {
                    tokio::join!(
                        reactor.run(events, reactor_tx),
                        space_manager.run(sm_rx),
                        window_server.run(ws_rx),
                    );
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
            pending_frame_overrides: HashMap::default(),
            windows: HashMap::default(),
            window_ids: HashMap::default(),
            window_server_info: HashMap::default(),
            visible_windows: HashSet::default(),
            screens: vec![],
            active_screen_idx: None,
            main_window_tracker: MainWindowTracker::default(),
            in_drag: false,
            resizing_window: None,
            frame_attempts: HashMap::default(),
            record,
            raise_manager_tx,
            animation_tx: None,
            mouse_tx: None,
            status_tx: None,
            group_indicators_tx: group_indicators_tx,
        }
    }

    pub async fn run(mut self, events: Receiver, events_tx: Sender) {
        let (raise_manager_tx, raise_manager_rx) = mpsc::unbounded_channel();
        self.raise_manager_tx = raise_manager_tx.clone();
        let (animation_tx, animation_rx) = mpsc::unbounded_channel();
        self.animation_tx = Some(animation_tx);

        let mouse_tx = self.mouse_tx.clone();
        let reactor_task = self.run_reactor_loop(events);
        let raise_manager_task = RaiseManager::run(raise_manager_rx, events_tx, mouse_tx);
        let animation_task = AnimationManager::run(animation_rx);

        let _ = tokio::join!(reactor_task, raise_manager_task, animation_task);
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
                    self.update_layout(&[], true);
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
        let mut animation_focus_wids: Vec<WindowId> = Vec::new();
        let mut is_resize = false;
        let raised_window = self.main_window_tracker.handle_event(&event);
        match event {
            Event::ApplicationLaunched {
                pid,
                info,
                handle,
                visible_windows,
                is_frontmost: _,
                main_window: _,
            } => {
                self.apps.insert(pid, AppState { info, handle });
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
            Event::WindowCreated(wid, window, mouse_state) => {
                // TODO: It's possible for a window to be on multiple spaces
                // or move spaces. (Add a test)
                // FIXME: We assume all windows are on the main screen.
                if let Some(wsid) = window.sys_id {
                    self.window_ids.insert(wsid, wid);
                }
                self.windows.insert(wid, window.clone().into());
                if mouse_state == MouseState::Down {
                    self.in_drag = true;
                    // Suppress updates while left button is pressed in case
                    // a drag is in progress.
                }
            }
            Event::WindowsOnScreenUpdated { pid, on_screen } => match pid {
                Some(_) => self.update_partial_window_server_info(on_screen),
                None => self.update_complete_window_server_info(on_screen),
            },
            Event::WindowBecameVisible(wid) => {
                if self.window_is_tracked(wid)
                    && let Some(window) = self.windows.get(&wid)
                    && let Some(space) = self.best_space_for_window(&window.frame_monotonic)
                    && let Some(info) = self.layout_window_info(wid)
                {
                    animation_focus_wids.push(wid);
                    self.send_layout_event(LayoutEvent::WindowAdded(space, wid, info));
                }
            }
            Event::WindowDestroyed(wid) => {
                self.layout.cancel_interactive_state();
                self.in_drag = false;
                self.resizing_window = None;
                if self.windows.remove(&wid).is_none() {
                    warn!("Got destroyed event for unknown window {wid:?}");
                }
                self.frame_attempts.remove(&wid);
                //animation_focus_wid = self.window_order.last().cloned();
                self.send_layout_event(LayoutEvent::WindowRemoved(wid));
            }
            Event::WindowFrameChanged(wid, new_frame, last_seen, requested, mouse_state) => {
                if mouse_state == Some(MouseState::Up) {
                    // The button is up, so any resize we were holding off on is
                    // over, even if we never saw the MouseUp event.
                    self.resizing_window = None;
                }
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
                self.send_layout_event(LayoutEvent::WindowFrameChanged { wid, frame: new_frame });
                let old_screen = self.best_screen_idx_for_window(&old_frame);
                let new_screen = self.best_screen_idx_for_window(&new_frame);
                if let Some(old) = old_screen
                    && let Some(new) = new_screen
                    && old != new
                    && let Some(info) = self.layout_window_info(wid)
                {
                    self.send_layout_event(LayoutEvent::WindowSpaceChanged {
                        wid,
                        added: self.screens[new].space,
                        removed: self.screens[old].space,
                        info,
                    });
                }
                if old_frame.size != new_frame.size {
                    let screens = self
                        .screens
                        .iter()
                        .flat_map(|screen| Some((screen.space?, screen.frame)))
                        .collect::<Vec<_>>();
                    // This event is ignored if the window is not in the layout.
                    self.send_layout_event(LayoutEvent::WindowResized {
                        wid,
                        old_frame,
                        new_frame,
                        screens,
                    });
                    if mouse_state == Some(MouseState::Down) {
                        self.resizing_window = Some(wid);
                    }
                    is_resize = true;
                } else if mouse_state == Some(MouseState::Down) {
                    self.in_drag = true;
                }
            }
            Event::ScreenParametersChanged {
                frames,
                spaces,
                converter,
                scale_factors,
                on_screen,
            } => {
                info!("screen parameters changed");
                let visible_window_order = on_screen.visible.clone();
                self.update_complete_window_server_info(on_screen);
                self.screens = frames
                    .into_iter()
                    .zip(spaces.clone())
                    .zip(scale_factors)
                    .map(|((frame, space), scale_factor)| Screen { frame, space, scale_factor })
                    .collect();
                let response = self
                    .screens
                    .iter()
                    .filter_map(|screen| screen.space.map(|space| (space, screen.frame.size)))
                    .collect::<Vec<_>>()
                    .into_iter()
                    .map(|(space, size)| {
                        self.layout.handle_event(LayoutEvent::SpaceExposed(space, size))
                    })
                    .reduce(layout::EventResponse::coalesce);
                if let Some(response) = response {
                    self.handle_layout_response_with_context(
                        response,
                        ResponseContext {
                            visible_window_order: Some(visible_window_order),
                            ..Default::default()
                        },
                    );
                    for space in self.screens.iter().flat_map(|screen| screen.space) {
                        self.layout.debug_tree_desc(space, "after event", false);
                    }
                }
                self.update_active_screen();
                // FIXME: Update visible windows if space changed.
                // Forward the event to group_indicators. We serialize these
                // through the reactor instead of delivering directly from
                // wm_controller in order to eliminate possible races with other
                // events sent by the reactor.
                self.group_indicators_tx
                    .send(group_bars::Event::ScreenParametersChanged(spaces, converter));
            }
            Event::SpaceChanged(spaces, on_screen) => {
                let visible_window_order = on_screen.visible.clone();
                self.update_complete_window_server_info(on_screen);
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
                self.resizing_window = None;
                info!("space changed");
                for (space, screen) in spaces.iter().zip(&mut self.screens) {
                    screen.space = *space;
                }
                let response = self
                    .screens
                    .iter()
                    .filter_map(|screen| screen.space.map(|space| (space, screen.frame.size)))
                    .map(|(space, size)| {
                        self.layout.handle_event(LayoutEvent::SpaceExposed(space, size))
                    })
                    .reduce(layout::EventResponse::coalesce);
                if let Some(response) = response {
                    self.handle_layout_response_with_context(
                        response,
                        ResponseContext {
                            visible_window_order: Some(visible_window_order),
                            ..Default::default()
                        },
                    );
                    for space in self.screens.iter().flat_map(|screen| screen.space) {
                        self.layout.debug_tree_desc(space, "after event", false);
                    }
                }
                if let Some(main_window) = self.main_window() {
                    let spaces = spaces.iter().copied().flatten().collect();
                    self.send_layout_event(LayoutEvent::WindowFocused(spaces, main_window));
                }
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
                            self.update_layout(&[], true);
                        } else if self.layout.update_interactive_move(
                            point,
                            screen.frame,
                            &self.config,
                        ) {
                            self.update_layout(&[], false);
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
                self.resizing_window = None;
                // Now re-check the layout.
            }
            Event::MouseMovedOverWindow(wsid, key_focus_pid) => {
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
                // Spotlight and similar apps can take keyboard focus without
                // activating, which leaves the main window unchanged. We don't
                // manage these windows and should avoid stealing focus from them.
                if let Some(key_focus_pid) = key_focus_pid
                    && current_main.is_some_and(|(_, main)| main.pid != key_focus_pid)
                {
                    debug!(
                        ?key_focus_pid,
                        ?current_main,
                        "Ignoring mouse move; keyboard focus is elsewhere"
                    );
                    return;
                }
                self.send_layout_event_with_context(
                    LayoutEvent::MouseMovedOverWindow {
                        over: (to_space, wid),
                        // TODO: Track focused window and use it here rather
                        // than main, to avoid stealing focus from a panel (of
                        // the main app; otherwise we would have returned above).
                        current_main,
                    },
                    ResponseContext {
                        from_mouse: true,
                        ..Default::default()
                    },
                );
            }
            Event::RaiseCompleted { window_id, sequence_id } => {
                let msg = raise::Event::RaiseCompleted { window_id, sequence_id };
                _ = self.raise_manager_tx.send((Span::current(), msg));
            }
            Event::RaiseRequestFailed { windows, sequence_id, quiet } => {
                let msg = raise::Event::RaiseRequestFailed { windows, sequence_id };
                _ = self.raise_manager_tx.send((Span::current(), msg));
                if quiet == Quiet::No
                    && let Some(main_window) = self.main_window()
                {
                    // The raise didn't move the focus, so bring the layout
                    // selection back to the window that has it.
                    let spaces = self.screens.iter().flat_map(|screen| screen.space).collect();
                    self.send_layout_event(LayoutEvent::WindowFocused(spaces, main_window));
                }
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
                // macOS can temporarily have no main window (for example after
                // clicking the desktop). Keep keyboard layout commands usable
                // by targeting the last active screen in that case.
                let command_space = self
                    .main_window_space()
                    .or_else(|| self.active_screen().and_then(|screen| screen.space));
                let screen = self
                    .screens
                    .iter()
                    .find(|screen| screen.space.is_some() && screen.space == command_space)
                    .map(|screen| screen.frame);
                let response =
                    self.layout.handle_command(command_space, screen, &visible_spaces, cmd);
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
            Event::SaveAndExit(spaces) => self.save_and_exit(spaces),
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
            self.update_layout(&animation_focus_wids, is_resize);
        }
    }

    fn save_and_exit(&self, spaces: SpaceState) -> ! {
        info!("SaveAndExit command received");
        match SavedState::save(&self.layout, spaces, &crate::config::saved_state_file()) {
            Ok(()) => std::process::exit(0),
            Err(e) => {
                error!("Could not save state: {e}");
                std::process::exit(3);
            }
        }
    }

    fn update_complete_window_server_info(&mut self, on_screen: WindowsOnScreen) {
        for info in on_screen.info.iter().filter(|i| i.layer == 0) {
            let Some(wid) = self.window_ids.get(&info.id) else {
                continue;
            };
            let Some(window) = self.windows.get_mut(wid) else {
                continue;
            };
            // Assume this update comes from after the last write. Typically the
            // window is on a different space than the one we're coming from
            // (unless it's on all spaces).
            //
            // TODO: It is still possible to have a race if we issued resizes on
            // this window that haven't completed yet (e.g. from an earlier
            // animation and a slow app). Consider having the app actor give us
            // updated locations on GetVisibleWindows instead.
            window.frame_monotonic = info.frame;
        }
        self.update_partial_window_server_info(on_screen);
    }

    fn update_partial_window_server_info(&mut self, on_screen: WindowsOnScreen) {
        // The on_screen snapshot always contains the complete list of visible
        // windows, even for partial (per-app) updates. Replace rather than
        // extend to avoid accumulating stale entries.
        self.visible_windows.clear();
        self.visible_windows.extend(on_screen.visible);
        self.window_server_info
            .extend(on_screen.info.into_iter().map(|info| (info.id, info)));
    }

    fn should_compare_visible_window(&self, wsid: WindowServerId) -> bool {
        let Some(info) = self.window_server_info.get(&wsid) else {
            return false;
        };
        if info.layer != 0 {
            // TODO: Revisit this if LayoutManager starts managing floating windows.
            return false;
        }
        self.screens
            .iter()
            .filter(|screen| screen.space.is_some())
            .map(|screen| screen.frame.intersection(&info.frame).size)
            .any(|size| size.width > 0.0 && size.height > 0.0)
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
            let Some(layout_info) = self.layout_window_info(wid) else {
                continue;
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

    /// The screen the window overlaps the most, or None if it is not on any
    /// screen. Apps park windows far off screen to hide them, and those belong
    /// to no screen at all.
    fn best_screen_idx_for_window(&self, frame: &CGRect) -> Option<usize> {
        self.screens
            .iter()
            .enumerate()
            .map(|(idx, screen)| (idx, screen.frame.intersection(frame).area()))
            .filter(|&(_, area)| area > 0.0)
            .max_by_key(|&(_, area)| area as i64)
            .map(|(idx, _)| idx)
            // A window with no area intersects nothing, so place it by its midpoint.
            .or_else(|| self.screens.iter().position(|screen| screen.frame.contains(frame.mid())))
    }

    fn best_space_for_window(&self, frame: &CGRect) -> Option<SpaceId> {
        self.screens[self.best_screen_idx_for_window(frame)?].space
    }

    /// Gathers the window properties the layout uses to classify a window.
    fn layout_window_info(&self, wid: WindowId) -> Option<LayoutWindowInfo> {
        let window = self.windows.get(&wid)?;
        let app = self.apps.get(&wid.pid);
        Some(LayoutWindowInfo {
            frame: window.frame_monotonic,
            bundle_id: app.and_then(|a| a.info.bundle_id.clone()),
            app_name: app.and_then(|a| a.info.localized_name.clone()),
            title: window.title.clone().into(),
            layer: window
                .window_server_id
                .and_then(|wsid| self.window_server_info.get(&wsid))
                .map(|info| info.layer),
            is_standard: window.is_ax_standard,
            is_resizable: window.is_resizable,
            ax_role: window.ax_role.clone(),
            ax_subrole: window.ax_subrole.clone(),
        })
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
        self.send_layout_event_with_context(event, ResponseContext::default());
    }

    fn send_layout_event_with_context(&mut self, event: LayoutEvent, context: ResponseContext) {
        let response = self.layout.handle_event(event);
        self.handle_layout_response_with_context(response, context);
        for space in self.screens.iter().flat_map(|screen| screen.space) {
            self.layout.debug_tree_desc(space, "after event", false);
        }
    }

    fn handle_layout_response(&mut self, response: layout::EventResponse) {
        self.handle_layout_response_with_context(response, ResponseContext::default());
    }

    fn handle_layout_response_with_context(
        &mut self,
        mut response: layout::EventResponse,
        ResponseContext {
            visible_window_order,
            from_mouse,
        }: ResponseContext,
    ) {
        if let Some(visible_window_order) = visible_window_order {
            response = self.filter_response(response, &visible_window_order);
        }

        let layout::EventResponse {
            frame_overrides,
            raise_windows,
            focus_window,
        } = response;
        self.pending_frame_overrides.extend(frame_overrides);
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

    fn filter_response(
        &self,
        mut response: layout::EventResponse,
        mut visible_window_order: &[WindowServerId],
    ) -> layout::EventResponse {
        if let Some(focus) = response.focus_window {
            // Attempt to match out the focus window with the top visible window.
            if let Some(&first) = visible_window_order.first()
                && focus.wsid() == Some(first)
            {
                // Note that we keep the focus window in the request, unless
                // there are no raise windows.
                visible_window_order = &visible_window_order[1..];
            } else {
                return response;
            }
        };
        let desired_visible_wsids = response
            .raise_windows
            .iter()
            .flat_map(|wid| self.windows.get(wid).and_then(|window| window.window_server_id))
            .collect::<HashSet<_>>();
        let current_top_wsids = visible_window_order
            .iter()
            .copied()
            // Filter out off-screen windows and windows on non-zero layers.
            .filter(|wsid| self.should_compare_visible_window(*wsid))
            .take(desired_visible_wsids.len())
            .collect::<HashSet<_>>();

        // Optimize the case where the response is a no-op.
        if current_top_wsids == desired_visible_wsids {
            response.focus_window.take();
            response.raise_windows.clear();
        }
        response
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
    pub fn update_layout(&mut self, new_wids: &[WindowId], skip_anim: bool) {
        let main_window = self.main_window();
        trace!(?main_window);
        let mut anim = Animation::new();
        let mut targets = BTreeMap::new();
        for &screen in &self.screens {
            let Some(space) = screen.space else { continue };
            if !skip_anim {
                self.layout.update_viewport_for_focus(space, screen.frame, &self.config);
            }
            let (result, groups) =
                self.layout.calculate_layout_and_groups(space, screen.frame, &self.config);

            self.group_indicators_tx
                .send(group_bars::Event::GroupsUpdated { space_id: space, groups });

            targets
                .extend(result.into_iter().map(|(wid, frame)| (wid, (frame, screen.scale_factor))));
        }
        for (wid, frame) in mem::take(&mut self.pending_frame_overrides) {
            let scale_factor = self
                .best_screen_idx_for_window(&frame)
                .and_then(|idx| self.screens.get(idx))
                .map_or(1.0, |screen| screen.scale_factor);
            targets.insert(wid, (frame, scale_factor));
        }
        for (wid, (target_frame, scale_factor)) in targets {
            if self.resizing_window == Some(wid) {
                // The user is dragging this window's edge; correct it on mouse
                // up instead.
                // TODO: A pending frame override for this window is dropped
                // here rather than deferred to mouse up.
                continue;
            }
            let Some(window) = self.windows.get_mut(&wid) else {
                // If we restored a saved state the window may not be available yet.
                continue;
            };
            let target_frame = round_to_physical(target_frame, scale_factor);
            let current_frame = window.frame_monotonic;
            if target_frame.same_as(current_frame) {
                continue;
            }
            // Some apps move a window back after we place it, which turns into
            // an event that makes us place it again. Stop writing the frame
            // once it's clear the app won't keep it.
            let now = Instant::now();
            let attempt = self.frame_attempts.entry(wid).or_insert(FrameAttempt {
                target: target_frame,
                count: 0,
                last: now,
            });
            if !attempt.target.same_as(target_frame)
                || now.duration_since(attempt.last) > FRAME_ATTEMPT_RESET
            {
                *attempt = FrameAttempt {
                    target: target_frame,
                    count: 0,
                    last: now,
                };
            }
            attempt.last = now;
            attempt.count = attempt.count.saturating_add(1);
            if attempt.count > MAX_FRAME_ATTEMPTS {
                if attempt.count == MAX_FRAME_ATTEMPTS + 1 {
                    warn!(?wid, ?current_frame, ?target_frame, "Giving up on window frame");
                }
                continue;
            }
            let Some(app) = self.apps.get(&wid.pid) else {
                continue;
            };
            let txid = window.next_txid();
            trace!(?wid, ?current_frame, ?target_frame);
            let is_new = new_wids.contains(&wid);
            anim.add_window(&app.handle, wid, current_frame, target_frame, is_new, txid);
            window.frame_monotonic = target_frame;
        }
        // If the user is doing something with the mouse we don't want to
        // animate on top of that.
        let skip_anim =
            skip_anim || !self.config.settings.animate || self.layout.has_active_scroll_animation();
        if let Some(tx) = &self.animation_tx
            && !anim.is_empty()
        {
            let message = if skip_anim {
                AnimationMessage::SkipToEnd(anim)
            } else {
                AnimationMessage::Replace(anim)
            };
            if let Err(err) = tx.send(message) {
                error!("Animation manager exited unexpectedly");
                match err.0 {
                    AnimationMessage::Replace(animation) => animation.skip_to_end(),
                    AnimationMessage::SkipToEnd(animation) => animation.skip_to_end(),
                }
            }
        } else {
            anim.skip_to_end();
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
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))],
            spaces: vec![Some(SpaceId::new(1))],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
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
    fn it_sends_layout_animation_to_manager() {
        let mut apps = Apps::new();
        let (mut reactor, mut animation_rx) =
            Reactor::new_for_test_with_animation(LayoutManager::new_for_test(), true);
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))],
            spaces: vec![Some(SpaceId::new(1))],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });

        reactor.handle_events(apps.make_app(1, make_windows(2)));
        reactor.handle_event(Event::StartupComplete);

        assert!(
            apps.requests().is_empty(),
            "layout should be handed to the animation manager, not sent directly to app actors"
        );
        assert!(matches!(
            animation_rx.try_recv(),
            Ok(animation::Message::Replace(_))
        ));
    }

    #[test]
    fn floating_window_restores_its_last_user_frame() {
        use LayoutCommand::ToggleWindowFloating;

        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let space = SpaceId::new(1);
        let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        let wid = WindowId::new(1, 1);
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![screen],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_events(apps.make_app_with_opts(1, make_windows(1), Some(wid), true));
        reactor.handle_event(Event::ApplicationGloballyActivated(1));
        reactor.handle_event(Event::StartupComplete);
        apps.simulate_until_quiet(&mut reactor);
        reactor.send_layout_event(LayoutEvent::WindowFocused(vec![space], wid));

        // First float restores the frame the window had before it was tiled.
        reactor.handle_event(Event::Command(Command::Layout(ToggleWindowFloating(
            Default::default(),
        ))));
        let initial_frame = CGRect::new(CGPoint::new(100., 100.), CGSize::new(50., 50.));
        let requests = apps.requests();
        assert!(requests.iter().any(|request| {
            matches!(request, Request::SetWindowFrame(request_wid, frame, _) if *request_wid == wid && *frame == initial_frame)
        }), "{requests:?}");
        for event in apps.simulate_events_for_requests(requests) {
            reactor.handle_event(event);
        }

        // A user move/resize while floating becomes the next restore target.
        let updated_frame = CGRect::new(CGPoint::new(300., 400.), CGSize::new(250., 125.));
        reactor.handle_event(Event::WindowFrameChanged(
            wid,
            updated_frame,
            apps.windows[&wid].last_seen_txid,
            Requested(false),
            None,
        ));
        assert_eq!(reactor.layout.floating_restore_frame(wid), Some(updated_frame));
        assert!(apps.requests().is_empty());

        reactor.handle_event(Event::Command(Command::Layout(ToggleWindowFloating(
            Default::default(),
        ))));
        apps.simulate_until_quiet(&mut reactor);
        assert_eq!(reactor.windows[&wid].frame_monotonic, screen);
        reactor.handle_event(Event::Command(Command::Layout(ToggleWindowFloating(
            Default::default(),
        ))));
        let requests = apps.requests();
        assert!(requests.iter().any(|request| {
            matches!(request, Request::SetWindowFrame(request_wid, frame, _) if *request_wid == wid && *frame == updated_frame)
        }), "{requests:?}");
    }

    #[test]
    fn floating_frame_restoration_uses_animation() {
        use LayoutCommand::ToggleWindowFloating;

        let mut apps = Apps::new();
        let (mut reactor, mut animation_rx) =
            Reactor::new_for_test_with_animation(LayoutManager::new_for_test(), true);
        let space = SpaceId::new(1);
        let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        let wid = WindowId::new(1, 1);
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![screen],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_events(apps.make_app_with_opts(1, make_windows(1), Some(wid), true));
        reactor.handle_event(Event::ApplicationGloballyActivated(1));
        reactor.handle_event(Event::StartupComplete);
        let animation::Message::Replace(animation) = animation_rx.try_recv().unwrap() else {
            panic!("expected initial layout animation");
        };
        animation.skip_to_end();
        apps.simulate_until_quiet(&mut reactor);
        reactor.send_layout_event(LayoutEvent::WindowFocused(vec![space], wid));

        reactor.handle_event(Event::Command(Command::Layout(ToggleWindowFloating(
            Default::default(),
        ))));
        assert!(
            apps.requests().is_empty(),
            "restore should be animated, not written directly"
        );
        assert!(matches!(
            animation_rx.try_recv(),
            Ok(animation::Message::Replace(_))
        ));
    }

    #[test]
    fn it_sends_writes_when_stale_read_state_looks_same_as_written_state() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))],
            spaces: vec![Some(SpaceId::new(1))],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
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
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))],
            spaces: vec![Some(SpaceId::new(1))],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
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
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))],
            spaces: vec![Some(SpaceId::new(1))],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
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
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let full_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![full_screen],
            spaces: vec![Some(SpaceId::new(1))],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
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
    fn it_stops_writing_a_frame_the_app_keeps_undoing() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let full_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![full_screen],
            spaces: vec![Some(SpaceId::new(1))],
            scale_factors: vec![1.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_events(apps.make_app(1, make_windows(1)));
        reactor.handle_event(Event::StartupComplete);
        apps.simulate_until_quiet(&mut reactor);

        // The app moves the window off its tiled position after every write.
        let wid = WindowId::new(1, 1);
        let moved_back = CGRect::new(CGPoint::new(100., 100.), full_screen.size);
        let mut writes = 0;
        let mut writes_in_last_round = 0;
        for _ in 0..MAX_FRAME_ATTEMPTS + 5 {
            reactor.handle_event(Event::WindowFrameChanged(
                wid,
                moved_back,
                apps.windows[&wid].last_seen_txid,
                Requested(false),
                None,
            ));
            let requests = apps.requests();
            writes_in_last_round = requests
                .iter()
                .filter(|request| {
                    matches!(
                        request,
                        Request::SetWindowFrame(..) | Request::AnimationFrame { .. }
                    )
                })
                .count();
            writes += writes_in_last_round;
            _ = apps.simulate_events_for_requests(requests);
        }
        // The initial placement counts toward the limit.
        assert!(writes <= MAX_FRAME_ATTEMPTS as usize, "{writes} writes");
        assert_eq!(0, writes_in_last_round);
    }

    #[test]
    fn windows_parked_off_screen_belong_to_no_screen() {
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let full_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![full_screen],
            spaces: vec![Some(SpaceId::new(1))],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });

        let off_screen = CGRect::new(CGPoint::new(0., -2000.), CGSize::new(1000., 500.));
        assert_eq!(None, reactor.best_screen_idx_for_window(&off_screen));

        let partly_on_screen = CGRect::new(CGPoint::new(0., -100.), CGSize::new(1000., 500.));
        assert_eq!(Some(0), reactor.best_screen_idx_for_window(&partly_on_screen));

        let no_area = CGRect::new(CGPoint::new(500., 500.), CGSize::new(0., 0.));
        assert_eq!(Some(0), reactor.best_screen_idx_for_window(&no_area));
    }

    #[test]
    fn it_selects_the_main_window_on_space_enable() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
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
            on_screen: Default::default(),
        });
        reactor.handle_event(Event::WindowsOnScreenUpdated {
            pid: None,
            on_screen: WindowsOnScreen::new(ws_info.clone()),
        });

        reactor.handle_events(apps.make_app_with_opts(
            1,
            make_windows(2),
            Some(WindowId::new(1, 1)),
            true,
        ));
        reactor.handle_event(Event::StartupComplete);
        reactor.handle_event(Event::ApplicationGloballyActivated(1));
        reactor.handle_events(apps.simulate_events());

        reactor.handle_event(Event::SpaceChanged(
            vec![Some(SpaceId::new(1))],
            WindowsOnScreen::new(ws_info),
        ));
        reactor.handle_events(apps.simulate_events());
        assert_eq!(
            reactor.layout.selected_window(SpaceId::new(1)),
            Some(WindowId::new(1, 1))
        );
    }

    #[test]
    fn it_surfaces_on_screen_change_when_the_snapshot_is_empty() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let (raise_manager_tx, mut raise_manager_rx) = mpsc::unbounded_channel();
        reactor.raise_manager_tx = raise_manager_tx;
        let space = SpaceId::new(1);
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_events(apps.make_app_with_opts(1, make_windows(2), None, false));
        let _events = apps.simulate_events();
        while raise_manager_rx.try_recv().is_ok() {}

        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 900.))],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        assert!(raise_manager_rx.try_recv().is_ok());
    }

    #[test]
    fn it_surfaces_on_screen_change_when_the_snapshot_omits_the_windows() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let (raise_manager_tx, mut raise_manager_rx) = mpsc::unbounded_channel();
        reactor.raise_manager_tx = raise_manager_tx;
        let space = SpaceId::new(1);
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_events(apps.make_app_with_opts(1, make_windows(2), None, false));
        let _events = apps.simulate_events();
        while raise_manager_rx.try_recv().is_ok() {}

        // The snapshot lists only a window we don't manage, so it says nothing
        // about where the windows we want to raise are.
        let on_screen = vec![WindowServerInfo {
            id: WindowServerId::new(999),
            pid: 2,
            layer: 0,
            frame: CGRect::new(CGPoint::new(0., 0.), CGSize::new(100., 100.)),
        }];
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 900.))],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: WindowsOnScreen::new(on_screen),
        });
        assert!(raise_manager_rx.try_recv().is_ok());
    }

    #[test]
    fn it_skips_surface_on_screen_change_when_top_layer_order_matches() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let (raise_manager_tx, mut raise_manager_rx) = mpsc::unbounded_channel();
        reactor.raise_manager_tx = raise_manager_tx;
        let space = SpaceId::new(1);
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_events(apps.make_app_with_opts(1, make_windows(2), None, false));
        let _events = apps.simulate_events();
        while raise_manager_rx.try_recv().is_ok() {}

        // A screen change that doesn't disturb the stacking shouldn't restack.
        let desired = reactor
            .layout
            .handle_event(LayoutEvent::SpaceExposed(space, CGSize::new(1000., 900.)))
            .raise_windows;
        let on_screen = desired
            .iter()
            .map(|wid| WindowServerInfo {
                id: reactor.windows[wid].window_server_id.unwrap(),
                pid: wid.pid,
                layer: 0,
                frame: reactor.windows[wid].frame_monotonic,
            })
            .collect();
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 900.))],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: WindowsOnScreen::new(on_screen),
        });
        assert!(raise_manager_rx.try_recv().is_err());
    }

    #[test]
    fn it_surfaces_on_screen_change_when_another_window_is_in_front() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let (raise_manager_tx, mut raise_manager_rx) = mpsc::unbounded_channel();
        reactor.raise_manager_tx = raise_manager_tx;
        let space = SpaceId::new(1);
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_events(apps.make_app_with_opts(1, make_windows(2), None, false));
        let _events = apps.simulate_events();
        while raise_manager_rx.try_recv().is_ok() {}

        // An unmanaged window is in front of the ones the layout wants on top,
        // so the restack is still needed.
        let mut on_screen = vec![WindowServerInfo {
            id: WindowServerId::new(999),
            pid: 2,
            layer: 0,
            frame: CGRect::new(CGPoint::new(0., 0.), CGSize::new(100., 100.)),
        }];
        on_screen.extend(reactor.windows.iter().map(|(wid, window)| WindowServerInfo {
            id: window.window_server_id.unwrap(),
            pid: wid.pid,
            layer: 0,
            frame: window.frame_monotonic,
        }));
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 900.))],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: WindowsOnScreen::new(on_screen),
        });
        assert!(raise_manager_rx.try_recv().is_ok());
    }

    #[test]
    fn it_skips_surface_when_top_layer_order_matches() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let (raise_manager_tx, mut raise_manager_rx) = mpsc::unbounded_channel();
        reactor.raise_manager_tx = raise_manager_tx;
        let space = SpaceId::new(1);
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_events(apps.make_app_with_opts(1, make_windows(2), None, false));
        let _events = apps.simulate_events();
        while raise_manager_rx.try_recv().is_ok() {}

        let desired = reactor
            .layout
            .handle_event(LayoutEvent::SpaceExposed(space, CGSize::new(1000., 1000.)))
            .raise_windows;
        let on_screen = desired
            .iter()
            .map(|wid| WindowServerInfo {
                id: reactor.windows[wid].window_server_id.unwrap(),
                pid: wid.pid,
                layer: 0,
                frame: reactor.windows[wid].frame_monotonic,
            })
            .collect();
        reactor.handle_event(Event::SpaceChanged(
            vec![Some(space)],
            WindowsOnScreen::new(on_screen),
        ));

        assert!(raise_manager_rx.try_recv().is_err());
    }

    #[test]
    fn it_skips_surface_when_top_layer_windows_are_reordered() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let (raise_manager_tx, mut raise_manager_rx) = mpsc::unbounded_channel();
        reactor.raise_manager_tx = raise_manager_tx;
        let space = SpaceId::new(1);
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_events(apps.make_app_with_opts(1, make_windows(2), None, false));
        let _events = apps.simulate_events();
        while raise_manager_rx.try_recv().is_ok() {}

        let desired = reactor
            .layout
            .handle_event(LayoutEvent::SpaceExposed(space, CGSize::new(1000., 1000.)))
            .raise_windows;
        let on_screen = desired
            .iter()
            .rev()
            .map(|wid| WindowServerInfo {
                id: reactor.windows[wid].window_server_id.unwrap(),
                pid: wid.pid,
                layer: 0,
                frame: reactor.windows[wid].frame_monotonic,
            })
            .collect();
        reactor.handle_event(Event::SpaceChanged(
            vec![Some(space)],
            WindowsOnScreen::new(on_screen),
        ));

        assert!(raise_manager_rx.try_recv().is_err());
    }

    #[test]
    fn it_surfaces_top_layer_windows_when_top_managed_set_differs() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let (raise_manager_tx, mut raise_manager_rx) = mpsc::unbounded_channel();
        reactor.raise_manager_tx = raise_manager_tx;
        let space = SpaceId::new(1);
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_events(apps.make_app_with_opts(1, make_windows(2), None, false));
        let _events = apps.simulate_events();
        while raise_manager_rx.try_recv().is_ok() {}

        let desired = reactor
            .layout
            .handle_event(LayoutEvent::SpaceExposed(space, CGSize::new(1000., 1000.)))
            .raise_windows;
        let mut on_screen = vec![WindowServerInfo {
            id: WindowServerId::new(90),
            pid: 9,
            layer: 0,
            frame: CGRect::new(CGPoint::new(10., 10.), CGSize::new(100., 100.)),
        }];
        on_screen.extend(desired.iter().map(|wid| WindowServerInfo {
            id: reactor.windows[wid].window_server_id.unwrap(),
            pid: wid.pid,
            layer: 0,
            frame: reactor.windows[wid].frame_monotonic,
        }));
        reactor.handle_event(Event::SpaceChanged(
            vec![Some(space)],
            WindowsOnScreen::new(on_screen),
        ));

        let msg = raise_manager_rx.try_recv().expect("Should have sent an event").1;
        match msg {
            raise::Event::RaiseRequest(RaiseRequest {
                raise_windows,
                focus_window,
                app_handles: _,
            }) => {
                assert_eq!(raise_windows, vec![desired]);
                assert!(focus_window.is_none());
            }
            _ => panic!("Unexpected event: {msg:?}"),
        }
    }

    #[test]
    fn filter_response_clears_matching_focus_and_raise_windows() {
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        reactor.screens = vec![Screen {
            frame: CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.)),
            space: Some(SpaceId::new(1)),
            scale_factor: 2.0,
        }];
        let w1 = WindowId::with_wsid(1, WindowServerId::new(1)).unwrap();
        let w2 = WindowId::with_wsid(1, WindowServerId::new(2)).unwrap();
        reactor.windows.insert(
            w1,
            super::WindowState {
                title: Secret::new(String::new()),
                window_server_id: Some(WindowServerId::new(1)),
                frame_monotonic: CGRect::new(CGPoint::new(0., 0.), CGSize::new(500., 1000.)),
                is_ax_standard: true,
                is_resizable: true,
                ax_role: String::new(),
                ax_subrole: None,
                last_sent_txid: TransactionId::default(),
            },
        );
        reactor.windows.insert(
            w2,
            super::WindowState {
                title: Secret::new(String::new()),
                window_server_id: Some(WindowServerId::new(2)),
                frame_monotonic: CGRect::new(CGPoint::new(500., 0.), CGSize::new(500., 1000.)),
                is_ax_standard: true,
                is_resizable: true,
                ax_role: String::new(),
                ax_subrole: None,
                last_sent_txid: TransactionId::default(),
            },
        );
        reactor.visible_windows =
            [WindowServerId::new(1), WindowServerId::new(2)].into_iter().collect();
        reactor.window_server_info.insert(
            WindowServerId::new(1),
            WindowServerInfo {
                id: WindowServerId::new(1),
                pid: 1,
                layer: 0,
                frame: reactor.windows[&w1].frame_monotonic,
            },
        );
        reactor.window_server_info.insert(
            WindowServerId::new(2),
            WindowServerInfo {
                id: WindowServerId::new(2),
                pid: 1,
                layer: 0,
                frame: reactor.windows[&w2].frame_monotonic,
            },
        );

        let response = reactor.filter_response(
            layout::EventResponse {
                frame_overrides: vec![],
                raise_windows: vec![w2],
                focus_window: Some(w1),
            },
            &[WindowServerId::new(1), WindowServerId::new(2)],
        );

        assert!(response.raise_windows.is_empty());
        assert!(response.focus_window.is_none());
    }

    #[test]
    fn filter_response_keeps_response_when_focus_is_not_frontmost() {
        let reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let w1 = WindowId::with_wsid(1, WindowServerId::new(1)).unwrap();
        let w2 = WindowId::with_wsid(1, WindowServerId::new(2)).unwrap();

        let response = reactor.filter_response(
            layout::EventResponse {
                frame_overrides: vec![],
                raise_windows: vec![w2],
                focus_window: Some(w1),
            },
            &[WindowServerId::new(2), WindowServerId::new(1)],
        );

        assert_eq!(response.raise_windows, vec![w2]);
        assert_eq!(response.focus_window, Some(w1));
    }

    #[test]
    fn it_ignores_unmanaged_and_nonzero_layer_windows_when_comparing_space_order() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let (raise_manager_tx, mut raise_manager_rx) = mpsc::unbounded_channel();
        reactor.raise_manager_tx = raise_manager_tx;
        let space = SpaceId::new(1);
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_events(apps.make_app_with_opts(1, make_windows(2), None, false));
        let _events = apps.simulate_events();
        while raise_manager_rx.try_recv().is_ok() {}

        let desired = reactor
            .layout
            .handle_event(LayoutEvent::SpaceExposed(space, CGSize::new(1000., 1000.)))
            .raise_windows;
        let mut on_screen = vec![
            WindowServerInfo {
                id: WindowServerId::new(90),
                pid: 9,
                layer: 3,
                frame: CGRect::new(CGPoint::new(10., 10.), CGSize::new(100., 100.)),
            },
            WindowServerInfo {
                id: WindowServerId::new(91),
                pid: 9,
                layer: 0,
                frame: CGRect::new(CGPoint::new(2000., 10.), CGSize::new(100., 100.)),
            },
        ];
        on_screen.extend(desired.iter().map(|wid| WindowServerInfo {
            id: reactor.windows[wid].window_server_id.unwrap(),
            pid: wid.pid,
            layer: 0,
            frame: reactor.windows[wid].frame_monotonic,
        }));
        reactor.handle_event(Event::SpaceChanged(
            vec![Some(space)],
            WindowsOnScreen::new(on_screen),
        ));

        assert!(raise_manager_rx.try_recv().is_err());
    }

    #[test]
    fn it_ignores_windows_on_disabled_spaces() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let full_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![full_screen],
            spaces: vec![None],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
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
            MouseState::Up,
        ));
        reactor.handle_event(Event::WindowDestroyed(WindowId::new(1, 2)));
    }

    #[test]
    fn it_keeps_discovered_windows_on_their_initial_screen() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let screen1 = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        let screen2 = CGRect::new(CGPoint::new(1000., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![screen1, screen2],
            spaces: vec![Some(SpaceId::new(1)), Some(SpaceId::new(2))],
            scale_factors: vec![2.0, 2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
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
    fn it_moves_windows_dragged_between_spaces() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let screen1 = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        let screen2 = CGRect::new(CGPoint::new(1000., 0.), CGSize::new(1000., 1000.));
        let space1 = SpaceId::new(1);
        let space2 = SpaceId::new(2);
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![screen1, screen2],
            spaces: vec![Some(space1), Some(space2)],
            scale_factors: vec![2.0, 2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });

        // Both windows start on screen1 / space1.
        reactor.handle_events(apps.make_app(1, make_windows(2)));
        reactor.handle_event(Event::StartupComplete);
        reactor.handle_events(apps.simulate_events());

        let dragged = WindowId::new(1, 1);
        let space1_windows: Vec<_> = reactor
            .layout
            .calculate_layout(space1, screen1, &reactor.config)
            .into_iter()
            .map(|(wid, _)| wid)
            .collect();
        assert!(space1_windows.contains(&dragged));
        assert!(reactor.layout.calculate_layout(space2, screen2, &reactor.config).is_empty());

        // Drag window 1 onto screen2, keeping its size the same so this is a
        // pure move (not a resize). The mouse button is still down.
        let frame = apps.windows[&dragged].frame;
        let new_frame = CGRect::new(CGPoint::new(1100., frame.origin.y), frame.size);
        reactor.handle_event(Event::WindowFrameChanged(
            dragged,
            new_frame,
            apps.windows[&dragged].last_seen_txid,
            Requested(false),
            Some(MouseState::Down),
        ));

        // The window now belongs to space2's layout and has left space1.
        let space1_windows: Vec<_> = reactor
            .layout
            .calculate_layout(space1, screen1, &reactor.config)
            .into_iter()
            .map(|(wid, _)| wid)
            .collect();
        let space2_windows: Vec<_> = reactor
            .layout
            .calculate_layout(space2, screen2, &reactor.config)
            .into_iter()
            .map(|(wid, _)| wid)
            .collect();
        assert!(!space1_windows.contains(&dragged), "{space1_windows:?}");
        assert!(space2_windows.contains(&dragged), "{space2_windows:?}");
        assert!(space1_windows.contains(&WindowId::new(1, 2)));

        // The reactor must not write any frames while the drag is in progress.
        assert!(
            apps.requests().is_empty(),
            "reactor shouldn't move windows mid-drag"
        );

        // Releasing the mouse re-tiles the window onto screen2.
        reactor.handle_event(Event::MouseUp);
        let requests = apps.requests();
        assert!(!requests.is_empty(), "release should re-tile the window");
        let events = apps.simulate_events_for_requests(requests);
        for event in events {
            reactor.handle_event(event);
        }
        assert_eq!(screen2, apps.windows[&dragged].frame);
    }

    #[test]
    fn it_keeps_windows_in_space_on_intra_space_drag() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let screen1 = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        let screen2 = CGRect::new(CGPoint::new(1000., 0.), CGSize::new(1000., 1000.));
        let space1 = SpaceId::new(1);
        let space2 = SpaceId::new(2);
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![screen1, screen2],
            spaces: vec![Some(space1), Some(space2)],
            scale_factors: vec![2.0, 2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });

        reactor.handle_events(apps.make_app(1, make_windows(2)));
        reactor.handle_event(Event::StartupComplete);
        reactor.handle_events(apps.simulate_events());

        // Drag a window to a different position but still on screen1 / space1.
        let dragged = WindowId::new(1, 1);
        let frame = apps.windows[&dragged].frame;
        let new_frame = CGRect::new(
            CGPoint::new(frame.origin.x + 20., frame.origin.y + 20.),
            frame.size,
        );
        reactor.handle_event(Event::WindowFrameChanged(
            dragged,
            new_frame,
            apps.windows[&dragged].last_seen_txid,
            Requested(false),
            Some(MouseState::Down),
        ));

        // The window stays on space1 and space2 remains empty.
        let space1_windows: Vec<_> = reactor
            .layout
            .calculate_layout(space1, screen1, &reactor.config)
            .into_iter()
            .map(|(wid, _)| wid)
            .collect();
        assert!(space1_windows.contains(&dragged));
        assert!(reactor.layout.calculate_layout(space2, screen2, &reactor.config).is_empty());

        // No frames are written while the mouse button is still down.
        assert!(
            apps.requests().is_empty(),
            "reactor shouldn't move windows mid-drag"
        );
    }

    /// Neighbors still follow along, and the window is corrected on mouse up.
    #[test]
    fn it_doesnt_write_to_a_window_the_user_is_resizing() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let space = SpaceId::new(1);
        let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![screen],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_events(apps.make_app(1, make_windows(2)));
        reactor.handle_event(Event::StartupComplete);
        apps.simulate_until_quiet(&mut reactor);

        let resized = WindowId::new(1, 1);
        let neighbor = WindowId::new(1, 2);
        let frame = apps.windows[&resized].frame;

        // The user drags the shared edge left, shrinking window 1.
        let new_frame = CGRect::new(
            frame.origin,
            CGSize::new(frame.size.width - 100., frame.size.height),
        );
        apps.windows.get_mut(&resized).unwrap().frame = new_frame;
        reactor.handle_event(Event::WindowFrameChanged(
            resized,
            new_frame,
            apps.windows[&resized].last_seen_txid,
            Requested(false),
            Some(MouseState::Down),
        ));

        // The neighbor grows to fill the space, but we leave the dragged window
        // alone.
        let wids = written_wids(apps.requests());
        assert!(wids.contains(&neighbor), "neighbor should follow: {wids:?}");
        assert!(
            !wids.contains(&resized),
            "shouldn't write to the window being resized: {wids:?}"
        );

        // The app settles on a change in three directions at once, which the
        // layout tree refuses to apply. The model and the window now disagree.
        let odd_frame = CGRect::new(
            CGPoint::new(new_frame.origin.x + 10., new_frame.origin.y + 10.),
            CGSize::new(new_frame.size.width - 30., new_frame.size.height - 5.),
        );
        apps.windows.get_mut(&resized).unwrap().frame = odd_frame;
        reactor.handle_event(Event::WindowFrameChanged(
            resized,
            odd_frame,
            apps.windows[&resized].last_seen_txid,
            Requested(false),
            Some(MouseState::Down),
        ));
        let wids = written_wids(apps.requests());
        assert!(
            !wids.contains(&resized),
            "shouldn't write to the window being resized: {wids:?}"
        );

        // Releasing the mouse snaps it back to the layout's frame.
        reactor.handle_event(Event::MouseUp);
        let wids = written_wids(apps.requests());
        assert!(
            wids.contains(&resized),
            "release should correct the window: {wids:?}"
        );
    }

    /// The MouseUp event can be lost, e.g. if the event tap is disabled while
    /// the button is down. The mouse state on the next frame change releases
    /// the window instead.
    #[test]
    fn it_stops_suppressing_a_resize_when_the_mouse_up_is_missed() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let space = SpaceId::new(1);
        let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![screen],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_events(apps.make_app(1, make_windows(2)));
        reactor.handle_event(Event::StartupComplete);
        apps.simulate_until_quiet(&mut reactor);

        let resized = WindowId::new(1, 1);
        let frame = apps.windows[&resized].frame;
        let new_frame = CGRect::new(
            frame.origin,
            CGSize::new(frame.size.width - 100., frame.size.height),
        );
        apps.windows.get_mut(&resized).unwrap().frame = new_frame;
        reactor.handle_event(Event::WindowFrameChanged(
            resized,
            new_frame,
            apps.windows[&resized].last_seen_txid,
            Requested(false),
            Some(MouseState::Down),
        ));
        let wids = written_wids(apps.requests());
        assert!(!wids.contains(&resized), "should be suppressed: {wids:?}");

        // No MouseUp arrives, but the next frame change reports the button up
        // and asks for a change the tree can't apply, so the model and the
        // window disagree.
        let odd_frame = CGRect::new(
            CGPoint::new(new_frame.origin.x + 10., new_frame.origin.y + 10.),
            CGSize::new(new_frame.size.width - 30., new_frame.size.height - 5.),
        );
        apps.windows.get_mut(&resized).unwrap().frame = odd_frame;
        reactor.handle_event(Event::WindowFrameChanged(
            resized,
            odd_frame,
            apps.windows[&resized].last_seen_txid,
            Requested(false),
            Some(MouseState::Up),
        ));
        let wids = written_wids(apps.requests());
        assert!(
            wids.contains(&resized),
            "button up should release the window: {wids:?}"
        );
    }

    fn written_wids(requests: Vec<Request>) -> Vec<WindowId> {
        requests
            .into_iter()
            .flat_map(|request| match request {
                Request::SetWindowFrame(wid, _, _) => vec![wid],
                Request::AnimationFrame { wid, .. } => vec![wid],
                _ => vec![],
            })
            .collect()
    }

    #[test]
    fn it_ignores_windows_on_nonzero_layers() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let full_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![full_screen],
            spaces: vec![Some(SpaceId::new(1))],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_event(Event::WindowsOnScreenUpdated {
            pid: None,
            on_screen: WindowsOnScreen::new(vec![WindowServerInfo {
                id: WindowServerId::new(1),
                pid: 1,
                layer: 10,
                frame: CGRect::ZERO,
            }]),
        });

        reactor.handle_events(apps.make_app_without_ws_info(1, make_windows(1), None, true));

        let state_before = apps.windows.clone();
        let _events = apps.simulate_events();
        assert_eq!(state_before, apps.windows, "Window should not have been moved",);

        // Make sure it doesn't choke on destroyed events for ignored windows.
        reactor.handle_event(Event::WindowDestroyed(WindowId::new(1, 1)));
        reactor.handle_event(Event::WindowCreated(
            WindowId::new(1, 2),
            make_window(2),
            MouseState::Up,
        ));
        reactor.handle_event(Event::WindowDestroyed(WindowId::new(1, 2)));
    }

    #[test]
    fn handle_layout_response_groups_windows_by_app_and_screen() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let (raise_manager_tx, mut raise_manager_rx) = mpsc::unbounded_channel();
        reactor.raise_manager_tx = raise_manager_tx;

        let screen1 = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        let screen2 = CGRect::new(CGPoint::new(1000., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![screen1, screen2],
            spaces: vec![Some(SpaceId::new(1)), Some(SpaceId::new(2))],
            scale_factors: vec![2.0, 2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });

        reactor.handle_events(apps.make_app(1, make_windows(2)));

        let mut windows = make_windows(2);
        windows[1].frame.origin = CGPoint::new(1100., 100.);
        reactor.handle_events(apps.make_app(2, windows));

        let _events = apps.simulate_events();
        while raise_manager_rx.try_recv().is_ok() {}

        reactor.handle_layout_response(layout::EventResponse {
            frame_overrides: vec![],
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
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let (raise_manager_tx, mut raise_manager_rx) = mpsc::unbounded_channel();
        reactor.raise_manager_tx = raise_manager_tx;

        reactor.handle_events(apps.make_app(1, make_windows(1)));
        reactor.handle_events(apps.make_app(2, make_windows(1)));

        let _events = apps.simulate_events();
        while raise_manager_rx.try_recv().is_ok() {}
        reactor.handle_layout_response(layout::EventResponse {
            frame_overrides: vec![],
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
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let space = SpaceId::new(1);
        let full_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![full_screen],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });

        reactor.handle_events(apps.make_app_with_opts(
            1,
            make_windows(3),
            Some(WindowId::new(1, 1)),
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
            on_screen: Default::default(),
        });
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![full_screen],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_event(Event::WindowsOnScreenUpdated {
            pid: None,
            on_screen: WindowsOnScreen::new(
                (1..=3)
                    .map(|n| WindowServerInfo {
                        pid: 1,
                        id: WindowServerId::new(n),
                        layer: 0,
                        frame: CGRect::ZERO,
                    })
                    .collect(),
            ),
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
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let full_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![full_screen],
            spaces: vec![Some(SpaceId::new(1))],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
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
            on_screen: Default::default(),
        });
        reactor.handle_event(Event::WindowsOnScreenUpdated {
            pid: None,
            on_screen: WindowsOnScreen::new(vec![WindowServerInfo {
                id: WindowServerId::new(1),
                pid: 1,
                layer: 0,
                frame: CGRect::new(CGPoint::new(500., 0.), CGSize::new(500., 500.)),
            }]),
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
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let space = SpaceId::new(1);
        reactor.handle_event(ScreenParametersChanged {
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        assert_eq!(None, reactor.main_window());

        reactor.handle_event(ApplicationGloballyActivated(1));
        reactor.handle_events(apps.make_app_with_opts(
            1,
            make_windows(2),
            Some(WindowId::new(1, 1)),
            true,
        ));

        reactor.handle_event(WindowDestroyed(WindowId::new(1, 1)));
        reactor.handle_event(Command(Layout(MoveFocus(Left))));
    }

    #[test]
    fn move_focus_uses_active_screen_when_no_window_is_focused() {
        use Direction::*;
        use Event::*;
        use LayoutCommand::*;

        use super::Command::*;
        use super::Reactor;

        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let space = SpaceId::new(1);
        reactor.handle_event(ScreenParametersChanged {
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_event(ApplicationGloballyActivated(1));
        reactor.handle_events(apps.make_app_with_opts(
            1,
            make_windows(2),
            Some(WindowId::new(1, 1)),
            true,
        ));
        assert_eq!(reactor.main_window(), Some(WindowId::new(1, 1)));

        let (raise_manager_tx, mut raise_manager_rx) = mpsc::unbounded_channel();
        reactor.raise_manager_tx = raise_manager_tx;
        reactor.handle_event(ApplicationGloballyDeactivated(1));
        assert_eq!(reactor.main_window(), None);

        reactor.handle_event(Command(Layout(MoveFocus(Right))));

        let event = raise_manager_rx
            .try_recv()
            .expect("focus command should produce a raise request")
            .1;
        let raise::Event::RaiseRequest(RaiseRequest { focus_window, .. }) = event else {
            panic!("unexpected raise event: {event:?}");
        };
        assert_eq!(focus_window.map(|(wid, _)| wid), Some(WindowId::new(1, 2)));
    }

    #[test]
    fn it_follows_the_mouse_only_when_the_main_window_has_keyboard_focus() {
        use Event::*;

        let mut apps = Apps::new();
        let space = SpaceId::new(1);
        let full_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        reactor.handle_event(ScreenParametersChanged {
            frames: vec![full_screen],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_event(ApplicationGloballyActivated(1));
        reactor.handle_events(apps.make_app_with_opts(
            1,
            make_windows(2),
            Some(WindowId::new(1, 1)),
            true,
        ));
        reactor.handle_events(apps.simulate_events());
        assert_eq!(reactor.main_window(), Some(WindowId::new(1, 1)));

        let (raise_manager_tx, mut raise_manager_rx) = mpsc::unbounded_channel();
        reactor.raise_manager_tx = raise_manager_tx;

        // Another process has keyboard focus, as when Spotlight is open.
        reactor.handle_event(MouseMovedOverWindow(WindowServerId::new(2), Some(2)));
        assert!(
            raise_manager_rx.try_recv().is_err(),
            "mouse move should be ignored while another process has keyboard focus"
        );

        // The main window's app has keyboard focus.
        reactor.handle_event(MouseMovedOverWindow(WindowServerId::new(2), Some(1)));
        let event = raise_manager_rx
            .try_recv()
            .expect("mouse move should produce a raise request")
            .1;
        let raise::Event::RaiseRequest(RaiseRequest { focus_window, .. }) = event else {
            panic!("unexpected raise event: {event:?}");
        };
        assert_eq!(focus_window.map(|(wid, _)| wid), Some(WindowId::new(1, 2)));

        // The key focus process could not be read.
        reactor.handle_event(MouseMovedOverWindow(WindowServerId::new(1), None));
        assert!(
            raise_manager_rx.try_recv().is_ok(),
            "mouse move should be followed when the key focus process is unknown"
        );
    }

    #[test]
    fn it_removes_terminated_app_windows_on_startup_complete() {
        use Event::*;

        let mut apps = Apps::new();
        let space = SpaceId::new(1);
        let full_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));

        // First reactor: simulate the state before shutdown with three apps running
        let mut reactor1 = Reactor::new_for_test(LayoutManager::new_for_test());
        reactor1.handle_event(ScreenParametersChanged {
            frames: vec![full_screen],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
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
            on_screen: Default::default(),
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
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let space = SpaceId::new(1);
        let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(Event::ScreenParametersChanged {
            frames: vec![screen],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
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
