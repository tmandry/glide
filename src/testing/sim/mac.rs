// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! A model of the macOS processes Glide talks to.
//!
//! macOS is a set of processes that each own some state and learn about the
//! rest from messages. The model keeps that shape: the window server, the Dock
//! and every app are a [`Process`] with state only it changes. A change one
//! process makes reaches another as a [`Message`], which is handled later, in
//! an order the caller picks.
//!
//! Only these happen without a message in between:
//!
//! - Something the user does is handled by one process right away.
//! - An accessibility call ([`Ax`]) reads or changes its app before returning.
//! - A window server query reads the window server before returning.
//! - An app gets the id of a window it creates from the window server.
//!
//! A [`Notification`] is for a process observing macOS from outside. The model
//! knows nothing about Glide; delivering messages and notifications is up to
//! the caller, which takes them with [`Mac::next_sent`].

use std::collections::{BTreeMap, VecDeque};

use objc2_core_foundation::{CGPoint, CGRect, CGSize};

use crate::sys::app::pid_t;
use crate::sys::screen::SpaceId;
use crate::sys::window_server::{WindowServerId, WindowServerInfo};

/// A macOS process that is sent messages.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Process {
    App(pid_t),
    WindowServer,
}

/// A message from one macOS process to another.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    // To the window server.
    /// From an app: put a new window on screen with this frame.
    OrderIn(WindowServerId, CGRect),
    /// From an app: the window is gone.
    DestroyWindow(WindowServerId),
    /// From an app: the window has this frame now.
    SetFrame(WindowServerId, CGRect),
    /// From an app: bring the window to the front.
    OrderFront(WindowServerId),
    /// Give the window the keyboard focus, activating its app.
    MakeKey(WindowServerId),
    /// From the Dock: show another desktop, counting from 1.
    ShowDesktop(usize),
    /// From the Dock: put the window on another desktop, counting from 1.
    MoveToDesktop(WindowServerId, usize),
    /// From the Dock: give the keyboard focus to the front window on the
    /// desktop being shown.
    FocusFrontWindow,

    // To an app, from the window server.
    Activate,
    Deactivate,
    /// One of the app's windows took the keyboard focus.
    BecomeMain(WindowServerId),
}

/// Something a process observing macOS is told.
#[derive(Debug, Clone, PartialEq)]
pub enum Notification {
    /// Accessibility, from an app to its observers.
    Ax(pid_t, AxNotification),
    /// Accessibility, from the Dock: Mission Control opened or closed.
    MissionControl(bool),
    /// From the workspace notification center.
    Workspace(WorkspaceNotification),
}

#[derive(Debug, Clone, PartialEq)]
pub enum AxNotification {
    WindowCreated(WindowServerId),
    WindowDestroyed(WindowServerId),
    MainWindowChanged,
    Activated,
    Deactivated,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WorkspaceNotification {
    AppLaunched(pid_t),
    AppActivated(pid_t),
    AppDeactivated(pid_t),
    ActiveSpaceChanged,
}

/// Something a process sent while handling a message or a call.
#[derive(Debug, Clone, PartialEq)]
pub enum Sent {
    Message(Process, Message),
    Notification(Notification),
}

#[derive(Default)]
struct Outbox(VecDeque<Sent>);

impl Outbox {
    fn send(&mut self, to: Process, message: Message) {
        self.0.push_back(Sent::Message(to, message));
    }

    fn notify(&mut self, notification: Notification) {
        self.0.push_back(Sent::Notification(notification));
    }
}

/// The window server. It also stands for the system services that track the
/// frontmost app and post workspace notifications.
pub struct WindowServer {
    screen: CGRect,
    desktops: Vec<SpaceId>,
    current: usize,
    windows: BTreeMap<WindowServerId, ServerWindow>,
    /// The windows that have been put on screen, front to back.
    order: Vec<WindowServerId>,
    frontmost: Option<pid_t>,
    next_wsid: u32,
}

struct ServerWindow {
    pid: pid_t,
    frame: CGRect,
    /// Unset until the window's app puts it on screen.
    desktop: Option<SpaceId>,
}

impl WindowServer {
    fn new(screen: CGRect, desktops: usize) -> WindowServer {
        WindowServer {
            screen,
            desktops: (1..=desktops as u64).map(SpaceId::new).collect(),
            current: 0,
            windows: BTreeMap::new(),
            order: Vec::new(),
            frontmost: None,
            next_wsid: 1,
        }
    }

    /// Make a window for `pid`. It has no desktop or frame until the app puts
    /// it on screen.
    fn new_window(&mut self, pid: pid_t) -> WindowServerId {
        let wsid = WindowServerId::new(self.next_wsid);
        self.next_wsid += 1;
        self.windows.insert(
            wsid,
            ServerWindow {
                pid,
                frame: CGRect::ZERO,
                desktop: None,
            },
        );
        wsid
    }

    fn launch(&mut self, pid: pid_t, out: &mut Outbox) {
        out.notify(Notification::Workspace(WorkspaceNotification::AppLaunched(pid)));
        self.activate(pid, out);
    }

    fn activate(&mut self, pid: pid_t, out: &mut Outbox) {
        use WorkspaceNotification::*;
        if self.frontmost == Some(pid) {
            return;
        }
        if let Some(old) = self.frontmost.replace(pid) {
            out.send(Process::App(old), Message::Deactivate);
            out.notify(Notification::Workspace(AppDeactivated(old)));
        }
        out.send(Process::App(pid), Message::Activate);
        out.notify(Notification::Workspace(AppActivated(pid)));
    }

    fn show_desktop(&mut self, index: usize, out: &mut Outbox) {
        if index != self.current {
            self.current = index;
            out.notify(Notification::Workspace(
                WorkspaceNotification::ActiveSpaceChanged,
            ));
        }
    }

    fn handle(&mut self, message: Message, out: &mut Outbox) {
        match message {
            Message::OrderIn(wsid, frame) => {
                let desktop = self.current_desktop();
                let Some(window) = self.windows.get_mut(&wsid) else {
                    return;
                };
                window.frame = frame;
                window.desktop = Some(desktop);
                self.order.insert(0, wsid);
            }
            Message::DestroyWindow(wsid) => {
                self.windows.remove(&wsid);
                self.order.retain(|&w| w != wsid);
            }
            Message::SetFrame(wsid, frame) => {
                if let Some(window) = self.windows.get_mut(&wsid) {
                    window.frame = frame;
                }
            }
            Message::OrderFront(wsid) => {
                let Some(window) = self.windows.get(&wsid) else {
                    return;
                };
                let Some(at) = self.order.iter().position(|&w| w == wsid) else {
                    return;
                };
                // The window goes in front of everything if its app is
                // frontmost, and in front of its app's other windows if not.
                let pid = window.pid;
                let to = if self.frontmost == Some(pid) {
                    0
                } else {
                    self.order.iter().position(|w| self.windows[w].pid == pid).unwrap()
                };
                self.order.remove(at);
                self.order.insert(to, wsid);
            }
            Message::MakeKey(wsid) => {
                let Some(window) = self.windows.get(&wsid) else {
                    return;
                };
                let (pid, desktop) = (window.pid, window.desktop);
                self.activate(pid, out);
                // The desktop of the window with the keyboard focus is shown.
                if let Some(index) = self.desktops.iter().position(|&d| Some(d) == desktop) {
                    self.show_desktop(index, out);
                }
            }
            Message::ShowDesktop(number) => self.show_desktop(number - 1, out),
            Message::MoveToDesktop(wsid, number) => {
                let desktop = self.desktops[number - 1];
                if let Some(window) = self.windows.get_mut(&wsid) {
                    window.desktop = Some(desktop);
                }
            }
            Message::FocusFrontWindow => {
                let desktop = Some(self.current_desktop());
                let Some(&front) = self.order.iter().find(|w| self.windows[w].desktop == desktop)
                else {
                    return;
                };
                let pid = self.windows[&front].pid;
                self.activate(pid, out);
                out.send(Process::App(pid), Message::BecomeMain(front));
            }
            Message::Activate | Message::Deactivate | Message::BecomeMain(_) => {
                unreachable!("{message:?} is for an app")
            }
        }
    }

    // What other processes can ask the window server.

    pub fn screen(&self) -> CGRect {
        self.screen
    }

    pub fn current_desktop(&self) -> SpaceId {
        self.desktops[self.current]
    }

    /// The desktop being shown, counting from 1.
    pub fn current_desktop_number(&self) -> usize {
        self.current + 1
    }

    pub fn desktop(&self, number: usize) -> SpaceId {
        self.desktops[number - 1]
    }

    pub fn info(&self, wsid: WindowServerId) -> Option<WindowServerInfo> {
        let window = self.windows.get(&wsid)?;
        Some(WindowServerInfo {
            id: wsid,
            pid: window.pid,
            layer: 0,
            frame: window.frame,
            spaces: window.desktop.into_iter().collect(),
        })
    }

    /// Every window on any desktop, including ones not on screen yet.
    pub fn all_windows(&self) -> Vec<WindowServerInfo> {
        let ordered = self.order.iter().copied();
        let unordered = self.windows.keys().copied().filter(|w| !self.order.contains(w));
        ordered.chain(unordered).filter_map(|w| self.info(w)).collect()
    }

    /// The windows on the desktop being shown, front to back.
    ///
    /// TODO: While Mission Control is open the real window server can list
    /// windows from every desktop here. The model doesn't, because the window
    /// server doesn't know Mission Control is open; only the Dock does.
    pub fn windows_on_screen(&self) -> Vec<WindowServerInfo> {
        let desktop = Some(self.current_desktop());
        self.order
            .iter()
            .filter(|w| self.windows[w].desktop == desktop)
            .filter_map(|&w| self.info(w))
            .collect()
    }
}

/// The Dock, which runs Mission Control and switches desktops. It only acts on
/// what the user does, so it has no inbox.
#[derive(Default)]
pub struct Dock {
    mission_control: bool,
}

pub struct App {
    pid: pid_t,
    name: String,
    /// The app's windows on every desktop, oldest first.
    windows: Vec<AppWindow>,
    main_window: Option<WindowServerId>,
    /// Whether the app has been told it is frontmost.
    active: bool,
}

#[derive(Debug, Clone)]
pub struct AppWindow {
    pub id: WindowServerId,
    pub title: String,
    pub frame: CGRect,
}

impl App {
    fn notify(&self, notification: AxNotification, out: &mut Outbox) {
        out.notify(Notification::Ax(self.pid, notification));
    }

    fn set_main_window(&mut self, wsid: Option<WindowServerId>, out: &mut Outbox) {
        if self.main_window != wsid {
            self.main_window = wsid;
            self.notify(AxNotification::MainWindowChanged, out);
        }
    }

    /// Show a new window, which becomes the main window.
    fn open_window(&mut self, id: WindowServerId, title: &str, out: &mut Outbox) {
        let offset = 30.0 * f64::from(id.0);
        let frame = CGRect::new(CGPoint::new(offset, offset), CGSize::new(400.0, 300.0));
        self.windows.push(AppWindow {
            id,
            title: title.to_owned(),
            frame,
        });
        self.notify(AxNotification::WindowCreated(id), out);
        self.set_main_window(Some(id), out);
        out.send(Process::WindowServer, Message::OrderIn(id, frame));
    }

    /// Close a window. The most recent remaining window becomes the main
    /// window.
    fn close_window(&mut self, wsid: WindowServerId, out: &mut Outbox) {
        self.windows.retain(|w| w.id != wsid);
        self.notify(AxNotification::WindowDestroyed(wsid), out);
        if self.main_window == Some(wsid) {
            self.set_main_window(self.windows.last().map(|w| w.id), out);
        }
        out.send(Process::WindowServer, Message::DestroyWindow(wsid));
    }

    fn handle(&mut self, message: Message, out: &mut Outbox) {
        match message {
            Message::Activate => {
                self.active = true;
                self.notify(AxNotification::Activated, out);
            }
            Message::Deactivate => {
                self.active = false;
                self.notify(AxNotification::Deactivated, out);
            }
            Message::BecomeMain(wsid) => {
                if self.windows.iter().any(|w| w.id == wsid) {
                    self.set_main_window(Some(wsid), out);
                }
            }
            _ => unreachable!("{message:?} is for the window server"),
        }
    }
}

/// The accessibility interface of one app.
pub struct Ax<'a> {
    app: &'a mut App,
    out: &'a mut Outbox,
}

impl Ax<'_> {
    pub fn name(&self) -> &str {
        &self.app.name
    }

    pub fn windows(&self) -> &[AppWindow] {
        &self.app.windows
    }

    pub fn window(&self, wsid: WindowServerId) -> Option<&AppWindow> {
        self.app.windows.iter().find(|w| w.id == wsid)
    }

    pub fn main_window(&self) -> Option<WindowServerId> {
        self.app.main_window
    }

    pub fn is_frontmost(&self) -> bool {
        self.app.active
    }

    /// Set a window's frame. Return the frame it had.
    pub fn set_frame(&mut self, wsid: WindowServerId, frame: CGRect) -> Option<CGRect> {
        let window = self.app.windows.iter_mut().find(|w| w.id == wsid)?;
        let old = std::mem::replace(&mut window.frame, frame);
        self.out.send(Process::WindowServer, Message::SetFrame(wsid, frame));
        Some(old)
    }

    /// Bring a window to the front and make it the main window. Return false
    /// if the app has no such window.
    pub fn raise(&mut self, wsid: WindowServerId) -> bool {
        if self.window(wsid).is_none() {
            return false;
        }
        self.app.set_main_window(Some(wsid), self.out);
        self.out.send(Process::WindowServer, Message::OrderFront(wsid));
        true
    }
}

pub struct Mac {
    window_server: WindowServer,
    dock: Dock,
    apps: BTreeMap<pid_t, App>,
    next_pid: pid_t,
    out: Outbox,
}

impl Mac {
    pub fn new(screen: CGRect, desktops: usize) -> Mac {
        Mac {
            window_server: WindowServer::new(screen, desktops),
            dock: Dock::default(),
            apps: BTreeMap::new(),
            next_pid: 1,
            out: Outbox::default(),
        }
    }

    // What the user does.

    /// Launch an app and make it frontmost.
    pub fn launch_app(&mut self, name: &str) -> pid_t {
        let pid = self.next_pid;
        self.next_pid += 1;
        self.apps.insert(
            pid,
            App {
                pid,
                name: name.to_owned(),
                windows: Vec::new(),
                main_window: None,
                active: false,
            },
        );
        self.window_server.launch(pid, &mut self.out);
        pid
    }

    /// Have an app open a window.
    pub fn open_window(&mut self, pid: pid_t, title: &str) -> WindowServerId {
        let wsid = self.window_server.new_window(pid);
        let app = self.apps.get_mut(&pid).expect("app is not running");
        app.open_window(wsid, title, &mut self.out);
        wsid
    }

    pub fn close_window(&mut self, wsid: WindowServerId) {
        let app = self
            .apps
            .values_mut()
            .find(|app| app.windows.iter().any(|w| w.id == wsid))
            .expect("window does not exist");
        app.close_window(wsid, &mut self.out);
    }

    pub fn open_mission_control(&mut self) {
        assert!(!self.dock.mission_control, "Mission Control is already open");
        self.dock.mission_control = true;
        self.out.notify(Notification::MissionControl(true));
    }

    pub fn close_mission_control(&mut self) {
        assert!(self.dock.mission_control, "Mission Control is not open");
        self.dock.mission_control = false;
        self.out.notify(Notification::MissionControl(false));
        self.out.send(Process::WindowServer, Message::FocusFrontWindow);
    }

    /// Show another desktop, counting from 1. Outside Mission Control this
    /// gives the keyboard focus to the front window there.
    pub fn switch_desktop(&mut self, number: usize) {
        self.out.send(Process::WindowServer, Message::ShowDesktop(number));
        if !self.dock.mission_control {
            self.out.send(Process::WindowServer, Message::FocusFrontWindow);
        }
    }

    /// Drag a window onto another desktop in Mission Control.
    pub fn drag_window_to_desktop(&mut self, wsid: WindowServerId, number: usize) {
        assert!(self.dock.mission_control, "Mission Control is not open");
        self.out.send(Process::WindowServer, Message::MoveToDesktop(wsid, number));
    }

    // Delivery.

    /// Take the next thing a process sent.
    pub fn next_sent(&mut self) -> Option<Sent> {
        self.out.0.pop_front()
    }

    /// Have `process` handle `message`.
    pub fn deliver(&mut self, process: Process, message: Message) {
        match process {
            Process::WindowServer => self.window_server.handle(message, &mut self.out),
            Process::App(pid) => {
                if let Some(app) = self.apps.get_mut(&pid) {
                    app.handle(message, &mut self.out);
                }
            }
        }
    }

    // What a process outside macOS can do.

    pub fn window_server(&self) -> &WindowServer {
        &self.window_server
    }

    /// Send the window server a message.
    pub fn send_window_server(&mut self, message: Message) {
        self.out.send(Process::WindowServer, message);
    }

    pub fn ax(&mut self, pid: pid_t) -> Ax<'_> {
        Ax {
            app: self.apps.get_mut(&pid).expect("app is not running"),
            out: &mut self.out,
        }
    }

    pub fn apps(&self) -> impl Iterator<Item = pid_t> + '_ {
        self.apps.keys().copied()
    }

    // What a test can see. Nothing in the system has this view.

    pub fn mission_control(&self) -> bool {
        self.dock.mission_control
    }

    pub fn title(&self, wsid: WindowServerId) -> Option<&str> {
        let mut windows = self.apps.values().flat_map(|app| &app.windows);
        windows.find(|w| w.id == wsid).map(|w| &*w.title)
    }

    pub fn window_titled(&self, title: &str) -> WindowServerId {
        let mut matches =
            self.apps.values().flat_map(|app| &app.windows).filter(|w| w.title == title);
        let window = matches.next().unwrap_or_else(|| panic!("no window titled {title}"));
        assert!(matches.next().is_none(), "more than one window titled {title}");
        window.id
    }
}
