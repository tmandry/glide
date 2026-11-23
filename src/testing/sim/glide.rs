// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Glide's side of the simulation.
//!
//! The reactor, raise manager, space manager, window server actor and app
//! actors are the real ones, wired to each other as they are in the running
//! program. The app actors reach macOS through the fake accessibility
//! interface in [`crate::sys::ax::fake`]. The WmController and the dock actor
//! are stand-ins: each turns what it observes in macOS into the events the
//! real one sends.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use accessibility::AXError;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tracing::Span;

use super::FocusReport;
use super::app_thread::{AppThread, Call, Event as AppEvent, Reply, Woken};
use super::mac::{self, Mac, Notification, WorkspaceNotification};
use crate::actor::layout::LayoutManager;
use crate::actor::raise::{self, RaiseManager};
use crate::actor::reactor::{self, Reactor};
use crate::actor::space_manager::{self, SpaceManager};
use crate::actor::window_server::{self, WindowServer, mock_window_server};
use crate::actor::wm_controller::{self, StartupToken};
use crate::actor::{self, app, group_bars, mouse, status};
use crate::config::Config;
use crate::sys::app::{AppInfo, pid_t};
use crate::sys::ax::fake;
use crate::sys::screen::{CoordinateConverter, ScreenId, SpaceId};
use crate::sys::window_server::{WindowId, WindowsOnScreen};

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
    /// To the actor observing whatever sent the notification.
    Notification(Notification),
    /// To an app actor: start up. Apps that were running when Glide started
    /// get a startup token.
    Start {
        startup: Option<StartupToken>,
    },
    /// To the WmController: every app that was running when Glide started has
    /// reported or given up.
    StartupAppsReported,
}

/// An app actor, on its own thread.
struct AppActor {
    thread: AppThread,
    woken: Arc<Woken>,
    state: AppState,
}

enum AppState {
    /// Waiting for its task to be woken.
    Idle,
    /// Waiting for the reply to an accessibility request, until it times out.
    Requesting {
        request: mac::AxRequestId,
        timeout: Instant,
    },
    /// Sleeping until `until`.
    Sleeping { until: Instant },
    /// Its task has finished.
    Done,
}

pub struct Glide {
    reactor: Reactor,
    raise_manager: RaiseManager,
    space_manager: SpaceManager,
    window_server: WindowServer,

    reactor_tx: reactor::Sender,
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
    /// The apps the WmController has started an actor for.
    known_apps: BTreeSet<pid_t>,
    raise_lock: app::RaiseLock,
    reported_focus: Vec<FocusReport>,
    /// The title of each window the reactor's selection moved to on the
    /// desktop being shown.
    selection_history: Vec<String>,
    /// Closes once every app that was running at startup has dropped its
    /// token.
    startup_rx: Option<UnboundedReceiver<()>>,
    /// Messages from the stand-ins that don't go through a channel.
    sent: Vec<(Actor, Message)>,
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
            reactor_tx.clone(),
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
            reactor_tx,
            reactor_rx,
            raise_rx,
            sm_tx,
            sm_rx,
            ws_tx,
            raise_lock: app::RaiseLock::default(),
            reported_focus: Vec::new(),
            selection_history: Vec::new(),
            ws_rx,
            _unread: (wm_rx, status_rx, group_bars_rx, mouse_rx, skylight_rx),
            apps: BTreeMap::new(),
            known_apps: BTreeSet::new(),
            startup_rx: None,
            sent: Vec::new(),
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
        let (startup_tx, startup_rx) = unbounded_channel();
        self.startup_rx = Some(startup_rx);
        for pid in mac.apps() {
            self.known_apps.insert(pid);
            let startup = Some(startup_tx.clone());
            self.sent.push((Actor::App(pid), Message::Start { startup }));
        }
        if let Some(pid) = window_server.frontmost_app() {
            self.reactor_event(reactor::Event::ApplicationGloballyActivated(pid));
        }
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
        if let Some(rx) = &mut self.startup_rx
            && rx.try_recv() == Err(TryRecvError::Disconnected)
        {
            self.startup_rx = None;
            sent.push((Actor::WmController, Message::StartupAppsReported));
        }
        sent
    }

    /// Have `actor` handle `message`.
    ///
    /// An app actor also runs without a message from the simulator when the
    /// reply to its call is due, or when its task has been woken, for example
    /// by a request the reactor sent on its channel or by another app actor
    /// releasing the raise lock. `message` is `None` then. The raise manager
    /// runs with no message when its timeout for the active sequence has
    /// passed.
    pub fn deliver(&mut self, actor: Actor, message: Option<Message>, mac: &Rc<RefCell<Mac>>) {
        let message = match (actor, message) {
            (Actor::App(pid), None) => return self.poll_app(pid, mac),
            (Actor::RaiseManager, None) => return self.time_out_raise(),
            (_, Some(message)) => message,
            (actor, None) => unreachable!("{actor:?} has no message"),
        };
        match (actor, message) {
            // The real actors.
            (Actor::Reactor, Message::Reactor(event)) => {
                let report = {
                    let mac = mac.borrow();
                    match event {
                        reactor::Event::ApplicationActivated(pid, quiet) => {
                            mac.app_name(pid).map(|name| FocusReport::App(name.to_owned(), quiet))
                        }
                        reactor::Event::ApplicationMainWindowChanged(_, Some(wid), quiet) => wid
                            .wsid()
                            .and_then(|wsid| mac.title(wsid))
                            .map(|title| FocusReport::Window(title.to_owned(), quiet)),
                        _ => None,
                    }
                };
                self.reported_focus.extend(report);
                self.reactor.handle_event_at_time(event, fake::now());
                self.record_selection(&mac.borrow());
            }
            (Actor::RaiseManager, Message::Raise(event)) => {
                self.raise_manager.handle_message(event)
            }
            (Actor::SpaceManager, Message::SpaceManager(event)) => {
                self.space_manager.on_event(event)
            }
            (Actor::WindowServer, Message::WindowServer(event)) => {
                // The actor queries the window server while it handles the
                // event.
                {
                    let mac = mac.borrow();
                    let window_server = mac.window_server();
                    mock_window_server(
                        window_server.windows_on_screen(),
                        window_server.all_windows(),
                        vec![Some(window_server.current_desktop())],
                    );
                }
                self.window_server.on_event(event);
            }
            (Actor::App(pid), Message::Start { startup }) => self.start_app(pid, startup, mac),
            (
                Actor::App(_),
                Message::Notification(Notification::Ax { pid, observer, element, name }),
            ) => {
                let Some(app) = self.apps.get(&pid) else {
                    return;
                };
                if let AppState::Done = app.state {
                    return;
                }
                let event = app.thread.run(Some((observer, pid, element, name)));
                self.on_app_event(pid, event, mac);
            }

            // The stand-ins.
            (Actor::WmController, Message::StartupAppsReported) => {
                self.sm_tx.send(space_manager::Event::ReactorEvent(
                    reactor::Event::StartupComplete,
                ));
            }
            (Actor::WmController, Message::Notification(Notification::Workspace(n))) => match n {
                WorkspaceNotification::AppLaunched(pid) => {
                    if self.known_apps.insert(pid) {
                        self.sent.push((Actor::App(pid), Message::Start { startup: None }));
                    }
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

    // The app actors.

    fn start_app(&mut self, pid: pid_t, startup: Option<StartupToken>, mac: &Rc<RefCell<Mac>>) {
        let info = {
            let mac = mac.borrow();
            let Some(name) = mac.app_name(pid) else {
                return;
            };
            AppInfo {
                bundle_id: mac.bundle_id(pid),
                localized_name: Some(name.to_owned()),
            }
        };
        let woken = Woken::new();
        let thread = AppThread::spawn(
            pid,
            info,
            self.ws_tx.clone(),
            startup,
            Arc::clone(&self.raise_lock),
            Arc::clone(&woken),
        );
        let event = thread.run(None);
        self.apps.insert(
            pid,
            AppActor {
                thread,
                woken,
                state: AppState::Idle,
            },
        );
        self.on_app_event(pid, event, mac);
    }

    /// Run an app actor that is ready: give it the reply to its call once
    /// that is due, or poll its task if it was woken.
    fn poll_app(&mut self, pid: pid_t, mac: &Rc<RefCell<Mac>>) {
        let Some(app) = self.apps.get_mut(&pid) else {
            return;
        };
        let now = fake::now();
        let event = match std::mem::replace(&mut app.state, AppState::Idle) {
            AppState::Requesting { request, timeout } => {
                let mut mac = mac.borrow_mut();
                let reply = match mac.take_ax_reply(request) {
                    Some(reply) => reply,
                    None if timeout <= now => {
                        mac.cancel_ax_request(request);
                        Err(AXError::CannotComplete)
                    }
                    None => {
                        app.state = AppState::Requesting { request, timeout };
                        return;
                    }
                };
                drop(mac);
                app.thread.reply(Reply::Ax(reply))
            }
            AppState::Sleeping { until } if until <= now => app.thread.reply(Reply::Slept),
            AppState::Idle if app.woken.is_set() => app.thread.run(None),
            state => {
                app.state = state;
                return;
            }
        };
        self.on_app_event(pid, event, mac);
    }

    /// Handle what an app actor did when it last ran, until it yields or
    /// waits on a call.
    fn on_app_event(&mut self, pid: pid_t, mut event: AppEvent, mac: &Rc<RefCell<Mac>>) {
        let app = self.apps.get_mut(&pid).unwrap();
        loop {
            let reply = match event {
                AppEvent::Yielded { done } => {
                    app.state = if done { AppState::Done } else { AppState::Idle };
                    return;
                }
                AppEvent::Call(Call::Ax(target, request)) => {
                    let request = mac.borrow_mut().ax_request(target, request);
                    let timeout = fake::now() + mac::REQUEST_TIMEOUT;
                    app.state = AppState::Requesting { request, timeout };
                    return;
                }
                AppEvent::Call(Call::Sleep(duration)) => {
                    app.state = AppState::Sleeping { until: fake::now() + duration };
                    return;
                }
                AppEvent::Call(Call::MakeKeyWindow(wsid)) => {
                    mac.borrow_mut().send_window_server(mac::Message::MakeKey(wsid));
                    Reply::MakeKeyWindow(Ok(()))
                }
                AppEvent::Call(Call::BundleId(target)) => {
                    Reply::BundleId(mac.borrow().bundle_id(target))
                }
            };
            event = app.thread.reply(reply);
        }
    }

    /// Whether an app actor is waiting on a call, and so can't take a
    /// message.
    pub fn app_is_calling(&self, pid: pid_t) -> bool {
        self.apps.get(&pid).is_some_and(|app| {
            matches!(
                app.state,
                AppState::Requesting { .. } | AppState::Sleeping { .. }
            )
        })
    }

    /// When the next request an app actor is waiting on times out, or the
    /// next sleeping actor wakes.
    pub fn next_call_due(&self) -> Option<Instant> {
        self.apps
            .values()
            .filter_map(|app| match app.state {
                AppState::Requesting { timeout, .. } => Some(timeout),
                AppState::Sleeping { until } => Some(until),
                _ => None,
            })
            .min()
    }

    /// How long until the raise manager times out the raise it is waiting
    /// on, if it is waiting on one.
    pub fn time_to_raise_timeout(&self) -> Option<Duration> {
        Some(self.raise_manager.time_to_timeout()).filter(|&t| t != Duration::MAX)
    }

    fn time_out_raise(&mut self) {
        if let Some(sequence_id) = self.raise_manager.take_timeout() {
            self.reactor_tx.send(reactor::Event::RaiseTimeout { sequence_id });
        }
    }

    /// The app actors that have something to do without being sent a message:
    /// the reply to their call has come, or their task was woken by a request
    /// waiting in their channel or a lock they were waiting for.
    pub fn woken_apps<'a>(&'a self, mac: &'a Mac) -> impl Iterator<Item = Actor> + 'a {
        let now = fake::now();
        self.apps
            .iter()
            .filter(move |(_, app)| match app.state {
                AppState::Idle => app.woken.is_set(),
                AppState::Requesting { request, timeout } => {
                    timeout <= now || mac.has_ax_reply(request)
                }
                AppState::Sleeping { until } => until <= now,
                AppState::Done => false,
            })
            .map(|(&pid, _)| Actor::App(pid))
    }

    // What a test can observe.

    pub fn tiled_windows(&self, space: SpaceId) -> Vec<WindowId> {
        self.reactor.tiled_windows(space)
    }

    fn record_selection(&mut self, mac: &Mac) {
        let space = mac.window_server().current_desktop();
        // The reactor has no layout to select in until it has tiled a window.
        if self.reactor.tiled_windows(space).is_empty() {
            return;
        }
        let Some(selected) = self.reactor.selected_window(space) else {
            return;
        };
        let Some(title) = selected.wsid().and_then(|wsid| mac.title(wsid)) else {
            return;
        };
        if self.selection_history.last().map(String::as_str) != Some(title) {
            self.selection_history.push(title.to_owned());
        }
    }

    pub fn selection_history(&self) -> &[String] {
        &self.selection_history
    }

    pub fn reported_focus(&self) -> &[FocusReport] {
        &self.reported_focus
    }

    pub fn selected_window(&mut self, space: SpaceId) -> Option<WindowId> {
        self.reactor.selected_window(space)
    }
}
