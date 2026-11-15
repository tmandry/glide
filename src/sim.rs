// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Run Glide against a simulated macOS.
//!
//! The real system is a set of processes and threads with their own event
//! loops, sending each other messages. The simulation keeps that shape: every
//! [`Process`] has an inbox, and a step delivers one message to one process.
//! The [`Schedule`] picks which process goes next, so the same scenario can be
//! run under different interleavings.
//!
//! The reactor, raise manager, space manager and window server actor are the
//! real ones. The app actors are stand-ins that answer requests from the state
//! of the [`World`].

mod world;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use objc2_core_foundation::CGRect;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tracing::Span;

use self::world::{Notification, World};
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
use crate::sys::screen::{CoordinateConverter, ScreenId};
use crate::sys::window_server::{WindowId, WindowServerId, WindowsOnScreen};

/// Something with an inbox and its own event loop.
///
/// The order here is the order [`Schedule::GlideFirst`] serves them in.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Process {
    Reactor,
    RaiseManager,
    SpaceManager,
    WindowServerActor,
    /// The app actor for one app.
    App(pid_t),
    /// The controller, which observes workspace notifications.
    Controller,
    /// The dock actor, which observes Mission Control.
    Dock,
    /// The macOS window server.
    WindowServer,
}

/// Which process handles its next message when several have one waiting.
#[derive(Debug, Copy, Clone)]
pub enum Schedule {
    /// Glide handles everything it has before macOS does anything more.
    GlideFirst,
    /// macOS finishes everything it has before Glide handles any of it.
    MacFirst,
    /// A pseudorandom order determined by the seed.
    Seeded(u64),
}

enum Message {
    Reactor(reactor::Event),
    Raise(raise::Event),
    SpaceManager(space_manager::Event),
    WindowServerActor(window_server::Event),
    Request(Request),
    Notification(Notification),
    /// Register the app with Glide. `startup` is set for apps that were
    /// running when Glide started.
    InitApp {
        startup: bool,
    },
    StartupComplete,
}

/// Something Glide did to a window.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Wrote(String),
    Raised(String),
}

/// The state of Glide's stand-in app actor for one app.
#[derive(Default)]
struct AppActor {
    /// Windows this actor has reported to Glide.
    known: BTreeSet<WindowServerId>,
}

pub struct Sim {
    pub mac: World,
    schedule: Schedule,
    inboxes: BTreeMap<Process, VecDeque<Message>>,
    steps: usize,

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
    app_rx: BTreeMap<pid_t, UnboundedReceiver<(Span, Request)>>,
    app_actors: BTreeMap<pid_t, AppActor>,
    /// Receivers nothing reads, kept so that sends to them succeed.
    _unread: (
        UnboundedReceiver<(Span, wm_controller::WmEvent)>,
        status::Receiver,
        group_bars::Receiver,
        mouse::Receiver,
        window_server::SkylightReceiver,
    ),

    glide_running: bool,
    /// Apps running at startup that haven't registered with Glide yet.
    starting_apps: usize,
    /// What Glide did, with whether Mission Control was open at the time.
    actions: Vec<(Action, bool)>,
}

/// How many steps a scenario may take before it counts as never settling.
const MAX_STEPS: usize = 10_000;

impl Sim {
    /// Make a system with one screen and the given number of desktops. Glide
    /// isn't running yet.
    pub fn new(schedule: Schedule, screen: CGRect, desktops: usize) -> Sim {
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

        Sim {
            mac: World::new(screen, desktops),
            schedule,
            inboxes: BTreeMap::new(),
            steps: 0,
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
            app_rx: BTreeMap::new(),
            app_actors: BTreeMap::new(),
            _unread: (wm_rx, status_rx, group_bars_rx, mouse_rx, skylight_rx),
            glide_running: false,
            starting_apps: 0,
            actions: Vec::new(),
        }
    }

    // What the user does. Each of these only changes the world and queues
    // messages; call `run` to let the system react.

    pub fn launch_app(&mut self, name: &str) -> pid_t {
        let pid = self.mac.launch_app(name);
        if self.glide_running {
            self.send(Process::App(pid), Message::InitApp { startup: false });
        }
        self.collect();
        pid
    }

    pub fn open_window(&mut self, pid: pid_t, title: &str) {
        self.mac.open_window(pid, title);
        self.collect();
    }

    pub fn close_window(&mut self, title: &str) {
        self.mac.close_window(self.mac.window_titled(title));
        self.collect();
    }

    pub fn open_mission_control(&mut self) {
        self.mac.open_mission_control();
        self.collect();
    }

    pub fn close_mission_control(&mut self) {
        self.mac.close_mission_control();
        self.collect();
    }

    pub fn switch_desktop(&mut self, number: usize) {
        self.mac.switch_desktop(number);
        self.collect();
    }

    pub fn move_window_to_desktop(&mut self, title: &str, number: usize) {
        self.mac.move_window_to_desktop(self.mac.window_titled(title), number);
        self.collect();
    }

    /// Start Glide. It learns about the screen first, then about each running
    /// app as that app's actor registers.
    pub fn start_glide(&mut self) {
        assert!(!self.glide_running);
        self.glide_running = true;
        // Nobody was observing the world until now.
        while self.mac.next_notification().is_some() {}
        self.send(
            Process::SpaceManager,
            Message::SpaceManager(space_manager::Event::ScreenParametersChanged {
                screens: vec![ScreenId::new(1)],
                frames: vec![self.mac.screen],
                spaces: vec![Some(self.mac.current_desktop())],
                scale_factors: vec![2.0],
                converter: CoordinateConverter::default(),
                on_screen: WindowsOnScreen::new(self.mac.windows_on_screen()),
            }),
        );
        let apps: Vec<_> = self.mac.apps().collect();
        self.starting_apps = apps.len();
        for pid in apps {
            self.send(Process::App(pid), Message::InitApp { startup: true });
        }
        if self.starting_apps == 0 {
            self.send(Process::Controller, Message::StartupComplete);
        }
    }

    // Running the system.

    /// Deliver messages until every inbox is empty.
    pub fn run(&mut self) {
        while self.step() {}
    }

    /// Deliver one message. Return false if there was none.
    pub fn step(&mut self) -> bool {
        self.collect();
        let ready: Vec<Process> =
            self.inboxes.iter().filter(|(_, q)| !q.is_empty()).map(|(&p, _)| p).collect();
        if ready.is_empty() {
            return false;
        }
        self.steps += 1;
        assert!(self.steps < MAX_STEPS, "the system did not settle");
        let process = match &mut self.schedule {
            Schedule::GlideFirst => ready[0],
            Schedule::MacFirst => ready[ready.len() - 1],
            Schedule::Seeded(state) => {
                // xorshift64
                *state ^= *state << 13;
                *state ^= *state >> 7;
                *state ^= *state << 17;
                ready[(*state % ready.len() as u64) as usize]
            }
        };
        let message = self.inbox(process).pop_front().unwrap();
        self.deliver(process, message);
        true
    }

    fn inbox(&mut self, process: Process) -> &mut VecDeque<Message> {
        self.inboxes.entry(process).or_default()
    }

    fn send(&mut self, process: Process, message: Message) {
        self.inbox(process).push_back(message);
    }

    /// Move everything that was sent into the inbox of the process it is for.
    fn collect(&mut self) {
        while let Some(notification) = self.mac.next_notification() {
            use Notification::*;
            if matches!(notification, PlaceWindow(_)) {
                self.send(Process::WindowServer, Message::Notification(notification));
                continue;
            }
            if !self.glide_running {
                continue;
            }
            match notification {
                WindowCreated(pid, _) | WindowDestroyed(pid, _) | MainWindowChanged(pid) => {
                    self.send(Process::App(pid), Message::Notification(notification));
                }
                AppActivated(pid) | AppDeactivated(pid) => {
                    self.send(Process::App(pid), Message::Notification(notification.clone()));
                    self.send(Process::Controller, Message::Notification(notification));
                }
                ActiveSpaceChanged => {
                    self.send(Process::Controller, Message::Notification(notification));
                }
                MissionControl(_) => {
                    self.send(Process::Dock, Message::Notification(notification));
                }
                PlaceWindow(_) => unreachable!(),
            }
        }
        while let Ok((_, event)) = self.reactor_rx.try_recv() {
            self.send(Process::Reactor, Message::Reactor(event));
        }
        while let Ok((_, event)) = self.raise_rx.try_recv() {
            self.send(Process::RaiseManager, Message::Raise(event));
        }
        while let Ok((_, event)) = self.sm_rx.try_recv() {
            self.send(Process::SpaceManager, Message::SpaceManager(event));
        }
        while let Ok((_, event)) = self.ws_rx.try_recv() {
            self.send(Process::WindowServerActor, Message::WindowServerActor(event));
        }
        let mut requests = Vec::new();
        for (&pid, rx) in &mut self.app_rx {
            while let Ok((_, request)) = rx.try_recv() {
                requests.push((pid, request));
            }
        }
        for (pid, request) in requests {
            self.send(Process::App(pid), Message::Request(request));
        }
    }

    fn deliver(&mut self, process: Process, message: Message) {
        match (process, message) {
            (Process::Reactor, Message::Reactor(event)) => self.reactor.step(event),
            (Process::RaiseManager, Message::Raise(event)) => {
                self.raise_manager.handle_message(event)
            }
            (Process::SpaceManager, Message::SpaceManager(event)) => {
                self.space_manager.on_event(event)
            }
            (Process::WindowServerActor, Message::WindowServerActor(event)) => {
                // The actor reads the window server while it handles the event.
                mock_window_server(
                    self.mac.windows_on_screen(),
                    self.mac.all_window_server_info(),
                    vec![Some(self.mac.current_desktop())],
                );
                self.window_server.on_event(event);
            }
            (Process::App(pid), Message::InitApp { startup }) => self.init_app(pid, startup),
            (Process::App(pid), Message::Request(request)) => self.app_request(pid, request),
            (Process::App(pid), Message::Notification(n)) => self.app_notification(pid, n),
            (Process::Controller, Message::StartupComplete) => {
                self.sm_tx.send(space_manager::Event::ReactorEvent(
                    reactor::Event::StartupComplete,
                ));
            }
            (Process::Controller, Message::Notification(n)) => match n {
                Notification::ActiveSpaceChanged => {
                    self.ws_tx.send(window_server::Event::SpaceChanged);
                }
                Notification::AppActivated(pid) => {
                    self.reactor_event(reactor::Event::ApplicationGloballyActivated(pid))
                }
                Notification::AppDeactivated(pid) => {
                    self.reactor_event(reactor::Event::ApplicationGloballyDeactivated(pid))
                }
                n => unreachable!("{n:?}"),
            },
            (Process::Dock, Message::Notification(Notification::MissionControl(open))) => {
                self.sm_tx.send(space_manager::Event::ExposeActive(open));
            }
            (Process::WindowServer, Message::Notification(Notification::PlaceWindow(wsid))) => {
                self.mac.place_window(wsid);
            }
            (process, _) => unreachable!("message delivered to the wrong process: {process:?}"),
        }
    }

    /// Send an event to the reactor the way actors do, through the window
    /// server actor.
    fn reactor_event(&self, event: reactor::Event) {
        self.ws_tx.send(window_server::Event::ReactorEvent(event));
    }

    // The app actor stand-in.

    fn window_id(pid: pid_t, wsid: WindowServerId) -> WindowId {
        WindowId::with_wsid(pid, wsid).unwrap()
    }

    fn window_info(&self, wsid: WindowServerId) -> WindowInfo {
        let window = self.mac.window(wsid).unwrap();
        WindowInfo {
            is_standard: true,
            is_resizable: true,
            title: window.title.clone().into(),
            frame: window.frame,
            sys_id: Some(wsid),
            ax_role: "AXWindow".into(),
            ax_subrole: Some("AXStandardWindow".into()),
        }
    }

    fn main_window(&self, pid: pid_t) -> Option<WindowId> {
        self.mac.app(pid).main_window.map(|wsid| Self::window_id(pid, wsid))
    }

    fn init_app(&mut self, pid: pid_t, startup: bool) {
        let (tx, rx) = unbounded_channel();
        self.app_rx.insert(pid, rx);
        let windows = self.mac.app(pid).windows.clone();
        self.app_actors.insert(
            pid,
            AppActor {
                known: windows.iter().copied().collect(),
            },
        );
        let name = &self.mac.app(pid).name;
        self.ws_tx.send(window_server::Event::ApplicationLaunched {
            pid,
            handle: AppThreadHandle::new_for_test(tx),
            info: AppInfo {
                bundle_id: Some(format!("com.example.{name}")),
                localized_name: Some(name.clone()),
            },
            is_frontmost: self.mac.frontmost() == Some(pid),
            main_window: self.main_window(pid),
            visible_windows: windows
                .iter()
                .map(|&wsid| (Self::window_id(pid, wsid), self.window_info(wsid)))
                .collect(),
        });
        if startup {
            self.starting_apps -= 1;
            if self.starting_apps == 0 {
                self.send(Process::Controller, Message::StartupComplete);
            }
        }
    }

    /// Report a window to Glide if this actor hasn't yet.
    fn register_window(&mut self, pid: pid_t, wsid: WindowServerId) {
        if self.mac.window(wsid).is_none()
            || !self.app_actors.entry(pid).or_default().known.insert(wsid)
        {
            return;
        }
        self.ws_tx.send(window_server::Event::WindowCreated(
            Self::window_id(pid, wsid),
            self.window_info(wsid),
            MouseState::Up,
        ));
    }

    fn app_notification(&mut self, pid: pid_t, notification: Notification) {
        if !self.app_rx.contains_key(&pid) {
            // The app actor hasn't started, so it isn't observing the app.
            return;
        }
        match notification {
            Notification::WindowCreated(_, wsid) => self.register_window(pid, wsid),
            Notification::WindowDestroyed(_, wsid) => {
                if self.app_actors.entry(pid).or_default().known.remove(&wsid) {
                    self.reactor_event(reactor::Event::WindowDestroyed(Self::window_id(pid, wsid)));
                }
            }
            Notification::MainWindowChanged(_) => {
                // This often arrives before the window's creation does.
                if let Some(wsid) = self.mac.app(pid).main_window {
                    self.register_window(pid, wsid);
                }
                self.ws_tx.send(window_server::Event::ApplicationMainWindowChanged(
                    pid,
                    self.main_window(pid),
                    Quiet::No,
                ));
            }
            Notification::AppActivated(_) => {
                self.reactor_event(reactor::Event::ApplicationActivated(pid, Quiet::No));
            }
            Notification::AppDeactivated(_) => {
                self.reactor_event(reactor::Event::ApplicationDeactivated(pid));
            }
            n => unreachable!("{n:?}"),
        }
    }

    fn app_request(&mut self, pid: pid_t, request: Request) {
        let mission_control = self.mac.mission_control();
        match request {
            Request::Terminate | Request::BeginWindowAnimation(_) | Request::WindowDestroyed(_) => {
            }
            Request::GetVisibleWindows => {
                let actor = self.app_actors.entry(pid).or_default();
                let mut new = Vec::new();
                let mut known_visible = Vec::new();
                for &wsid in &self.mac.app(pid).windows {
                    if actor.known.insert(wsid) {
                        new.push(wsid);
                    } else {
                        known_visible.push(Self::window_id(pid, wsid));
                    }
                }
                let new = new
                    .into_iter()
                    .map(|wsid| (Self::window_id(pid, wsid), self.window_info(wsid)))
                    .collect();
                self.reactor_event(reactor::Event::WindowsDiscovered { pid, new, known_visible });
            }
            Request::SetWindowFrame(wid, frame, txid)
            | Request::AnimationFrame { wid, frame, txid, .. } => {
                let Some(wsid) = wid.wsid() else { return };
                let Some(window) = self.mac.window(wsid) else {
                    return;
                };
                let old_frame = window.frame;
                self.actions.push((Action::Wrote(window.title.clone()), mission_control));
                self.mac.set_frame(wsid, frame);
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
            Request::EndWindowAnimation(_) => {}
            Request::Raise(wids, token, sequence_id, quiet) => {
                if token.is_cancelled() {
                    self.reactor_event(reactor::Event::RaiseRequestFailed {
                        windows: wids,
                        sequence_id,
                        quiet,
                    });
                    return;
                }
                let wsids: Vec<_> = wids.iter().filter_map(|wid| wid.wsid()).collect();
                for &wsid in &wsids {
                    if let Some(window) = self.mac.window(wsid) {
                        self.actions.push((Action::Raised(window.title.clone()), mission_control));
                    }
                }
                let outcome = self.mac.raise(pid, &wsids);
                // The app actor reports the changes its own raise made itself,
                // so drop the app's notifications about them.
                self.collect_raise_notifications(pid);
                if outcome.activated {
                    self.reactor_event(reactor::Event::ApplicationActivated(pid, quiet));
                }
                for wid in wids {
                    self.reactor_event(reactor::Event::RaiseCompleted {
                        window_id: wid,
                        sequence_id,
                    });
                }
                if outcome.main_window_changed {
                    self.ws_tx.send(window_server::Event::ApplicationMainWindowChanged(
                        pid,
                        self.main_window(pid),
                        quiet,
                    ));
                }
            }
        }
    }

    /// Queue the notifications a raise by `pid` caused, except the activation
    /// of `pid` that its app actor already accounted for.
    fn collect_raise_notifications(&mut self, pid: pid_t) {
        let mut kept = Vec::new();
        while let Some(notification) = self.mac.next_notification() {
            kept.push(notification);
        }
        for notification in kept {
            match notification {
                Notification::AppActivated(p) if p == pid => {
                    self.send(Process::Controller, Message::Notification(notification));
                }
                Notification::AppDeactivated(p) => {
                    self.send(Process::App(p), Message::Notification(notification.clone()));
                    self.send(Process::Controller, Message::Notification(notification));
                }
                Notification::ActiveSpaceChanged => {
                    self.send(Process::Controller, Message::Notification(notification));
                }
                n => unreachable!("{n:?}"),
            }
        }
    }

    // What a test can observe.

    /// The titles of the windows Glide has tiled on a desktop, sorted.
    pub fn tiled(&self, desktop: usize) -> Vec<String> {
        let mut titles: Vec<_> = self
            .reactor
            .tiled_windows(self.mac.desktop(desktop))
            .into_iter()
            .filter_map(|wid| Some(self.mac.window(wid.wsid()?)?.title.clone()))
            .collect();
        titles.sort();
        titles
    }

    pub fn frame(&self, title: &str) -> CGRect {
        self.mac.window(self.mac.window_titled(title)).unwrap().frame
    }

    /// The desktop being shown, counting from 1.
    pub fn current_desktop(&self) -> usize {
        self.mac.current_desktop_number()
    }

    /// What Glide did to windows while Mission Control was open.
    pub fn actions_during_mission_control(&self) -> Vec<Action> {
        self.actions.iter().filter(|(_, mc)| *mc).map(|(a, _)| a.clone()).collect()
    }

    /// Forget what Glide has done so far.
    pub fn clear_actions(&mut self) {
        self.actions.clear();
    }

    pub fn actions(&self) -> Vec<Action> {
        self.actions.iter().map(|(a, _)| a.clone()).collect()
    }
}
