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
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Wake, Waker};
use std::time::Duration;

use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tracing::Span;

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

/// An app actor, run a step at a time.
struct AppActor {
    /// The actor's task, until it finishes.
    task: Option<Pin<Box<dyn Future<Output = ()>>>>,
    woken: Arc<Woken>,
}

/// Records that a task was woken.
struct Woken(AtomicBool);

impl Wake for Woken {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::Relaxed);
    }
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
    /// An app actor also runs when its task has been woken without a message
    /// from the simulator, for example by a request the reactor sent on its
    /// channel or by another app actor releasing the raise lock. `message` is
    /// `None` then, and the task is polled. The raise manager runs with no
    /// message when its timeout for the active sequence has passed.
    pub fn deliver(&mut self, actor: Actor, message: Option<Message>, mac: &Rc<RefCell<Mac>>) {
        let _system = fake::install(Rc::new(mac::System(Rc::clone(mac))));
        let message = match (actor, message) {
            (Actor::App(pid), None) => return self.poll_app(pid),
            (Actor::RaiseManager, None) => return self.time_out_raise(),
            (_, Some(message)) => message,
            (actor, None) => unreachable!("{actor:?} has no message"),
        };
        match (actor, message) {
            // The real actors.
            (Actor::Reactor, Message::Reactor(event)) => {
                self.reactor.handle_event_at_time(event, fake::now())
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
                fake::notify(observer, pid, element, name);
                self.poll_app(pid);
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
        let Some(task) = app::app_task(
            pid,
            info,
            self.ws_tx.clone(),
            startup,
            Arc::clone(&self.raise_lock),
        ) else {
            return;
        };
        self.apps.insert(
            pid,
            AppActor {
                task: Some(Box::pin(task)),
                woken: Arc::new(Woken(AtomicBool::new(true))),
            },
        );
        self.poll_app(pid);
    }

    /// Poll an app actor's task once if it was woken. A task that wakes
    /// itself while it is polled is left for the schedule to pick again, as
    /// the real executor goes back to its run loop between polls.
    fn poll_app(&mut self, pid: pid_t) {
        let Some(app) = self.apps.get_mut(&pid) else {
            return;
        };
        if !app.woken.0.swap(false, Ordering::Relaxed) {
            return;
        }
        let Some(task) = &mut app.task else { return };
        let waker = Waker::from(Arc::clone(&app.woken));
        if task.as_mut().poll(&mut Context::from_waker(&waker)).is_ready() {
            app.task = None;
        }
    }

    /// Whether the raise manager's timer for its active sequence has fired.
    pub fn raise_timeout_due(&self) -> bool {
        self.raise_manager.time_to_timeout() == Duration::ZERO
    }

    fn time_out_raise(&mut self) {
        if let Some(sequence_id) = self.raise_manager.take_timeout() {
            self.reactor_tx.send(reactor::Event::RaiseTimeout { sequence_id });
        }
    }

    /// The app actors that have something to do without being sent a message:
    /// a request waiting in their channel, or a lock they were waiting for.
    pub fn woken_apps(&self) -> impl Iterator<Item = Actor> + '_ {
        self.apps
            .iter()
            .filter(|(_, app)| app.task.is_some() && app.woken.0.load(Ordering::Relaxed))
            .map(|(&pid, _)| Actor::App(pid))
    }

    // What a test can observe.

    pub fn tiled_windows(&self, space: SpaceId) -> Vec<WindowId> {
        self.reactor.tiled_windows(space)
    }

    pub fn selected_window(&mut self, space: SpaceId) -> Option<WindowId> {
        self.reactor.selected_window(space)
    }
}
