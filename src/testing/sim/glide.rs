// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Glide's side of the simulation.
//!
//! The reactor, raise manager, space manager and window server actor are the
//! real ones, wired to each other as they are in the running program. The app
//! actors, the WmController and the dock actor are stand-ins: each turns what
//! it observes in macOS into the events the real one sends.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio_util::sync::CancellationToken;
use tracing::Span;

use super::mac::{self, AxNotification, Mac, Notification, WorkspaceNotification};
use crate::actor::app::{AppThreadHandle, Quiet, Request};
use crate::actor::layout::LayoutManager;
use crate::actor::raise::{self, RaiseManager};
use crate::actor::reactor::{self, Reactor, Requested};
use crate::actor::space_manager::{self, SpaceManager};
use crate::actor::window_server::{self, WindowServer, mock_window_server};
use crate::actor::{self, group_bars, mouse, status, wm_controller};
use crate::config::Config;
use crate::sys::app::{AppInfo, WindowInfo, pid_t};
use crate::sys::event::MouseState;
use crate::sys::geometry::SameAs;
use crate::sys::screen::{CoordinateConverter, ScreenId, SpaceId};
use crate::sys::window_server::{WindowId, WindowServerId, WindowsOnScreen};

/// One of Glide's actors. Each handles its messages on its own event loop.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Actor {
    Reactor,
    RaiseManager,
    SpaceManager,
    WindowServer,
    App(pid_t),
    WmController,
    Dock,
}

pub enum Message {
    Reactor(reactor::Event),
    Raise(raise::Event),
    SpaceManager(space_manager::Event),
    WindowServer(window_server::Event),
    /// To an app actor, from the reactor or the raise manager.
    Request(Request),
    /// To the actor observing whatever sent the notification.
    Notification(Notification),
    /// To an app actor: start observing the app and report it. `startup` is
    /// set for apps that were running when Glide started.
    Start {
        startup: bool,
    },
    /// To the WmController: every app that was running when Glide started has
    /// reported.
    StartupAppsReported,
}

/// Something Glide did to a window.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Wrote(String),
    Raised(String),
}

/// The stand-in app actor for one app.
struct AppActor {
    rx: UnboundedReceiver<(Span, Request)>,
    /// Windows reported to Glide.
    known: BTreeSet<WindowServerId>,
    /// The main window last reported to Glide.
    main_window: Option<WindowServerId>,
    /// Whether the app was frontmost when last read.
    is_frontmost: bool,
    /// Raise requests not finished, oldest first.
    raises: VecDeque<Raise>,
    /// Whether the oldest raise is waiting for the app to activate.
    raise_waiting: bool,
    /// How to report the activation a raise asked for, and the window whose
    /// becoming main belongs to that raise.
    last_activated: Option<(Quiet, Option<WindowId>)>,
}

struct Raise {
    wids: Vec<WindowId>,
    token: CancellationToken,
    sequence_id: u64,
    quiet: Quiet,
}

pub struct Glide {
    reactor: Reactor,
    raise_manager: RaiseManager,
    space_manager: SpaceManager,
    window_server: WindowServer,

    reactor_rx: reactor::Receiver,
    raise_rx: UnboundedReceiver<(Span, raise::Event)>,
    sm_tx: space_manager::Sender,
    sm_rx: space_manager::Receiver,
    ws_tx: window_server::Sender,
    ws_rx: window_server::Receiver,
    /// Receivers nothing reads, kept so that sends to them succeed.
    _unread: (
        UnboundedReceiver<(Span, wm_controller::WmEvent)>,
        status::Receiver,
        group_bars::Receiver,
        mouse::Receiver,
        window_server::SkylightReceiver,
    ),

    apps: BTreeMap<pid_t, AppActor>,
    /// Apps running at startup whose actors haven't reported yet.
    starting_apps: usize,
    /// Messages from the stand-ins that don't go through a channel.
    sent: Vec<(Actor, Message)>,
    /// What Glide did, with whether Mission Control was open at the time.
    actions: Vec<(Action, bool)>,
}

fn window_id(pid: pid_t, wsid: WindowServerId) -> WindowId {
    WindowId::with_wsid(pid, wsid).unwrap()
}

fn window_info(window: &mac::AppWindow) -> WindowInfo {
    WindowInfo {
        is_standard: true,
        is_resizable: true,
        title: window.title.clone().into(),
        frame: window.frame,
        sys_id: Some(window.id),
        ax_role: "AXWindow".into(),
        ax_subrole: Some("AXStandardWindow".into()),
    }
}

impl Glide {
    pub fn new() -> Glide {
        let mut config = Config::default();
        config.settings.default_disable = false;
        let config = Arc::new(config);

        let (reactor_tx, reactor_rx) = actor::channel();
        let (sm_tx, sm_rx) = actor::channel();
        let (ws_tx, ws_rx) = actor::channel();
        let (raise_tx, raise_rx) = unbounded_channel();
        let (wm_tx, wm_rx) = unbounded_channel();
        let (status_tx, status_rx) = actor::channel();
        let (group_bars_tx, group_bars_rx) = actor::channel();
        let (mouse_tx, mouse_rx) = actor::channel();
        let (skylight_tx, skylight_rx) = actor::channel();

        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        reactor.set_raise_manager(raise_tx);
        let space_manager = SpaceManager::new(
            false,
            config,
            reactor_tx,
            ws_tx.clone(),
            wm_tx.clone(),
            status_tx,
            group_bars_tx,
            mouse_tx,
            None,
        );
        let window_server = WindowServer::new(sm_tx.clone(), wm_tx, skylight_tx);

        Glide {
            reactor,
            raise_manager: RaiseManager::new(),
            space_manager,
            window_server,
            reactor_rx,
            raise_rx,
            sm_tx,
            sm_rx,
            ws_tx,
            ws_rx,
            _unread: (wm_rx, status_rx, group_bars_rx, mouse_rx, skylight_rx),
            apps: BTreeMap::new(),
            starting_apps: 0,
            sent: Vec::new(),
            actions: Vec::new(),
        }
    }

    /// Start up: report the screen, then start an app actor for each running
    /// app.
    pub fn start(&mut self, mac: &Mac) {
        let window_server = mac.window_server();
        self.sm_tx.send(space_manager::Event::ScreenParametersChanged {
            screens: vec![ScreenId::new(1)],
            frames: vec![window_server.screen()],
            spaces: vec![Some(window_server.current_desktop())],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: WindowsOnScreen::new(window_server.windows_on_screen()),
        });
        let apps: Vec<_> = mac.apps().collect();
        self.starting_apps = apps.len();
        for pid in apps {
            self.sent.push((Actor::App(pid), Message::Start { startup: true }));
        }
        if self.starting_apps == 0 {
            self.sent.push((Actor::WmController, Message::StartupAppsReported));
        }
    }

    /// Whether an app actor is observing `pid`.
    pub fn observes(&self, pid: pid_t) -> bool {
        self.apps.contains_key(&pid)
    }

    /// Take the messages the actors have sent each other.
    pub fn take_sent(&mut self) -> Vec<(Actor, Message)> {
        let mut sent = std::mem::take(&mut self.sent);
        while let Ok((_, event)) = self.reactor_rx.try_recv() {
            sent.push((Actor::Reactor, Message::Reactor(event)));
        }
        while let Ok((_, event)) = self.raise_rx.try_recv() {
            sent.push((Actor::RaiseManager, Message::Raise(event)));
        }
        while let Ok((_, event)) = self.sm_rx.try_recv() {
            sent.push((Actor::SpaceManager, Message::SpaceManager(event)));
        }
        while let Ok((_, event)) = self.ws_rx.try_recv() {
            sent.push((Actor::WindowServer, Message::WindowServer(event)));
        }
        for (&pid, app) in &mut self.apps {
            while let Ok((_, request)) = app.rx.try_recv() {
                sent.push((Actor::App(pid), Message::Request(request)));
            }
        }
        sent
    }

    /// Have `actor` handle `message`.
    pub fn deliver(&mut self, actor: Actor, message: Message, mac: &mut Mac) {
        match (actor, message) {
            // The real actors.
            (Actor::Reactor, Message::Reactor(event)) => self.reactor.handle_events(vec![event]),
            (Actor::RaiseManager, Message::Raise(event)) => {
                self.raise_manager.handle_message(event)
            }
            (Actor::SpaceManager, Message::SpaceManager(event)) => {
                self.space_manager.on_event(event)
            }
            (Actor::WindowServer, Message::WindowServer(event)) => {
                // The actor queries the window server while it handles the
                // event.
                let window_server = mac.window_server();
                mock_window_server(
                    window_server.windows_on_screen(),
                    window_server.all_windows(),
                    vec![Some(window_server.current_desktop())],
                );
                self.window_server.on_event(event);
            }

            // The stand-ins.
            (Actor::App(pid), Message::Start { startup }) => self.start_app(pid, startup, mac),
            (Actor::App(pid), Message::Request(request)) => self.app_request(pid, request, mac),
            (Actor::App(pid), Message::Notification(Notification::Ax(_, n))) => {
                self.app_notification(pid, n, mac)
            }
            (Actor::WmController, Message::StartupAppsReported) => {
                self.sm_tx.send(space_manager::Event::ReactorEvent(
                    reactor::Event::StartupComplete,
                ));
            }
            (Actor::WmController, Message::Notification(Notification::Workspace(n))) => match n {
                WorkspaceNotification::AppLaunched(pid) => {
                    self.sent.push((Actor::App(pid), Message::Start { startup: false }));
                }
                WorkspaceNotification::AppActivated(pid) => {
                    self.reactor_event(reactor::Event::ApplicationGloballyActivated(pid))
                }
                WorkspaceNotification::AppDeactivated(pid) => {
                    self.reactor_event(reactor::Event::ApplicationGloballyDeactivated(pid))
                }
                WorkspaceNotification::ActiveSpaceChanged => {
                    self.ws_tx.send(window_server::Event::SpaceChanged);
                }
            },
            (Actor::Dock, Message::Notification(Notification::MissionControl(open))) => {
                self.sm_tx.send(space_manager::Event::ExposeActive(open));
            }
            (actor, _) => unreachable!("message delivered to the wrong actor: {actor:?}"),
        }
    }

    /// Send an event to the reactor the way actors do, through the window
    /// server actor.
    fn reactor_event(&self, event: reactor::Event) {
        self.ws_tx.send(window_server::Event::ReactorEvent(event));
    }

    fn record(&mut self, action: fn(String) -> Action, wsid: WindowServerId, mac: &Mac) {
        if let Some(title) = mac.title(wsid) {
            self.actions.push((action(title.to_owned()), mac.mission_control()));
        }
    }

    // The app actor stand-in. It follows `actor::app`.

    fn start_app(&mut self, pid: pid_t, startup: bool, mac: &mut Mac) {
        let (tx, rx) = unbounded_channel();
        let ax = mac.ax(pid);
        let (is_frontmost, main_window) = (ax.is_frontmost(), ax.main_window());
        self.ws_tx.send(window_server::Event::ApplicationLaunched {
            pid,
            handle: AppThreadHandle::new_for_test(tx),
            info: AppInfo {
                bundle_id: Some(format!("com.example.{}", ax.name())),
                localized_name: Some(ax.name().to_owned()),
            },
            is_frontmost,
            main_window: main_window.map(|wsid| window_id(pid, wsid)),
            visible_windows: ax
                .windows()
                .iter()
                .map(|w| (window_id(pid, w.id), window_info(w)))
                .collect(),
        });
        self.apps.insert(
            pid,
            AppActor {
                rx,
                known: ax.windows().iter().map(|w| w.id).collect(),
                main_window,
                is_frontmost,
                raises: VecDeque::new(),
                raise_waiting: false,
                last_activated: None,
            },
        );
        if startup {
            self.starting_apps -= 1;
            if self.starting_apps == 0 {
                self.sent.push((Actor::WmController, Message::StartupAppsReported));
            }
        }
    }

    fn app(&mut self, pid: pid_t) -> &mut AppActor {
        self.apps.get_mut(&pid).expect("app actor is not running")
    }

    /// Report a window to Glide if this actor hasn't yet.
    fn register_window(&mut self, pid: pid_t, wsid: WindowServerId, mac: &mut Mac) {
        let ax = mac.ax(pid);
        let Some(window) = ax.window(wsid) else {
            return;
        };
        if !self.app(pid).known.insert(wsid) {
            return;
        }
        self.ws_tx.send(window_server::Event::WindowCreated(
            window_id(pid, wsid),
            window_info(window),
            MouseState::Up,
        ));
    }

    fn app_notification(&mut self, pid: pid_t, notification: AxNotification, mac: &mut Mac) {
        match notification {
            AxNotification::WindowCreated(wsid) => self.register_window(pid, wsid, mac),
            AxNotification::WindowDestroyed(wsid) => {
                if self.app(pid).known.remove(&wsid) {
                    self.reactor_event(reactor::Event::WindowDestroyed(window_id(pid, wsid)));
                }
            }
            AxNotification::MainWindowChanged => {
                let quiet_if = self.app(pid).last_activated.as_mut().and_then(|a| a.1.take());
                self.on_main_window_changed(pid, quiet_if, mac);
            }
            AxNotification::Activated | AxNotification::Deactivated => {
                self.on_activation_changed(pid, mac)
            }
        }
        self.advance_raises(pid, mac);
    }

    /// Read the app's main window and report it if it changed. The change is
    /// quiet if the new main window is `quiet_if`.
    fn on_main_window_changed(&mut self, pid: pid_t, quiet_if: Option<WindowId>, mac: &mut Mac) {
        let Some(wsid) = mac.ax(pid).main_window() else {
            return;
        };
        // This often runs before the window's creation is observed.
        self.register_window(pid, wsid, mac);
        let app = self.app(pid);
        if app.main_window == Some(wsid) {
            return;
        }
        app.main_window = Some(wsid);
        let wid = window_id(pid, wsid);
        let quiet = if quiet_if == Some(wid) {
            Quiet::Yes
        } else {
            Quiet::No
        };
        self.ws_tx.send(window_server::Event::ApplicationMainWindowChanged(
            pid,
            Some(wid),
            quiet,
        ));
    }

    /// Read whether the app is frontmost and report it if that changed. An
    /// activation lets the raise waiting for it go on.
    fn on_activation_changed(&mut self, pid: pid_t, mac: &mut Mac) {
        let is_frontmost = mac.ax(pid).is_frontmost();
        let app = self.app(pid);
        let was_frontmost = std::mem::replace(&mut app.is_frontmost, is_frontmost);
        let event = if is_frontmost {
            let (quiet, quiet_window) = app.last_activated.take().unwrap_or((Quiet::No, None));
            self.on_main_window_changed(pid, quiet_window, mac);
            reactor::Event::ApplicationActivated(pid, quiet)
        } else {
            reactor::Event::ApplicationDeactivated(pid)
        };
        if was_frontmost != is_frontmost {
            self.reactor_event(event);
        }
        if is_frontmost && std::mem::take(&mut self.app(pid).raise_waiting) {
            self.finish_raise(pid, mac);
        }
    }

    fn app_request(&mut self, pid: pid_t, request: Request, mac: &mut Mac) {
        match request {
            Request::Terminate
            | Request::BeginWindowAnimation(_)
            | Request::EndWindowAnimation(_)
            | Request::WindowDestroyed(_) => {}
            Request::GetVisibleWindows => {
                let windows = mac.ax(pid).windows().to_vec();
                let app = self.app(pid);
                let (mut new, mut known_visible) = (Vec::new(), Vec::new());
                for window in &windows {
                    let wid = window_id(pid, window.id);
                    if app.known.insert(window.id) {
                        new.push((wid, window_info(window)));
                    } else {
                        known_visible.push(wid);
                    }
                }
                self.reactor_event(reactor::Event::WindowsDiscovered { pid, new, known_visible });
            }
            Request::SetWindowFrame(wid, frame, txid)
            | Request::AnimationFrame { wid, frame, txid, .. } => {
                let Some(wsid) = wid.wsid() else { return };
                let Some(old_frame) = mac.ax(pid).set_frame(wsid, frame) else {
                    return;
                };
                self.record(Action::Wrote, wsid, mac);
                if !old_frame.same_as(frame) {
                    self.reactor_event(reactor::Event::WindowFrameChanged(
                        wid,
                        frame,
                        txid,
                        Requested(true),
                        None,
                    ));
                }
            }
            Request::Raise(wids, token, sequence_id, quiet) => {
                self.app(pid).raises.push_back(Raise {
                    wids,
                    token,
                    sequence_id,
                    quiet,
                });
            }
        }
        self.advance_raises(pid, mac);
    }

    fn fail_raise(&mut self, raise: Raise) {
        self.reactor_event(reactor::Event::RaiseRequestFailed {
            windows: raise.wids,
            sequence_id: raise.sequence_id,
            quiet: raise.quiet,
        });
    }

    /// Carry the app's raise requests as far as they can go.
    ///
    /// A raise gives its first window the keyboard focus. If the app isn't
    /// frontmost, the raise then waits to observe the app's activation before
    /// it brings its windows to the front.
    fn advance_raises(&mut self, pid: pid_t, mac: &mut Mac) {
        loop {
            let app = self.app(pid);
            let Some(raise) = app.raises.front() else {
                return;
            };
            let first = raise.wids.first().copied();
            let first_wsid = first.and_then(|wid| wid.wsid());
            let (quiet, single) = (raise.quiet, raise.wids.len() == 1);
            if raise.token.is_cancelled()
                || first_wsid.is_none_or(|wsid| mac.ax(pid).window(wsid).is_none())
            {
                let app = self.app(pid);
                app.raise_waiting = false;
                let raise = app.raises.pop_front().unwrap();
                self.fail_raise(raise);
                continue;
            }
            if app.raise_waiting {
                return;
            }
            let is_frontmost = mac.ax(pid).is_frontmost();
            mac.send_window_server(mac::Message::MakeKey(first_wsid.unwrap()));
            if !is_frontmost {
                let app = self.app(pid);
                app.raise_waiting = true;
                // Windows under the last one are always raised quietly.
                app.last_activated = Some(if single {
                    (quiet, (quiet == Quiet::Yes).then_some(first).flatten())
                } else {
                    (Quiet::Yes, first)
                });
                return;
            }
            self.finish_raise(pid, mac);
        }
    }

    /// Bring the windows of the app's oldest raise to the front, in order.
    fn finish_raise(&mut self, pid: pid_t, mac: &mut Mac) {
        let raise = self.app(pid).raises.pop_front().unwrap();
        for (i, &wid) in raise.wids.iter().enumerate() {
            let raised = wid.wsid().filter(|&wsid| mac.ax(pid).raise(wsid));
            let Some(wsid) = raised else {
                self.fail_raise(raise);
                return;
            };
            self.record(Action::Raised, wsid, mac);
            self.reactor_event(reactor::Event::RaiseCompleted {
                window_id: wid,
                sequence_id: raise.sequence_id,
            });
            let is_last = i + 1 == raise.wids.len();
            let quiet_if = (!is_last || raise.quiet == Quiet::Yes).then_some(wid);
            self.on_main_window_changed(pid, quiet_if, mac);
        }
    }

    // What a test can observe.

    pub fn tiled_windows(&self, space: SpaceId) -> Vec<WindowId> {
        self.reactor.tiled_windows(space)
    }

    pub fn actions(&self) -> &[(Action, bool)] {
        &self.actions
    }

    pub fn clear_actions(&mut self) {
        self.actions.clear();
    }
}
