// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! A model of the parts of macOS that Glide talks to.
//!
//! The world holds the window server's state: desktops, windows and their
//! order, the frontmost app, and Mission Control. Changing it queues
//! [`Notification`]s, which stand for the messages macOS sends to the
//! processes observing it. The world knows nothing about Glide; delivering the
//! notifications is up to the caller.

use std::collections::{BTreeMap, VecDeque};

use objc2_core_foundation::{CGPoint, CGRect, CGSize};

use crate::sys::app::pid_t;
use crate::sys::screen::SpaceId;
use crate::sys::window_server::{WindowServerId, WindowServerInfo};

pub struct World {
    pub screen: CGRect,
    desktops: Vec<SpaceId>,
    current: usize,
    mission_control: bool,
    windows: BTreeMap<WindowServerId, Window>,
    /// All windows, front to back.
    order: Vec<WindowServerId>,
    apps: BTreeMap<pid_t, App>,
    frontmost: Option<pid_t>,
    next_wsid: u32,
    next_pid: pid_t,
    notifications: VecDeque<Notification>,
}

#[derive(Debug, Clone)]
pub struct Window {
    pub pid: pid_t,
    pub title: String,
    pub frame: CGRect,
    pub desktop: SpaceId,
    /// Whether the window server has assigned the window a desktop and frame.
    ///
    /// A window exists in its app before the window server finishes placing
    /// it. Until then the window server reports it with no spaces and a zero
    /// frame, and leaves it out of the windows on screen.
    pub placed: bool,
}

#[derive(Debug, Clone)]
pub struct App {
    pub name: String,
    /// The app's windows on every desktop, oldest first.
    pub windows: Vec<WindowServerId>,
    pub main_window: Option<WindowServerId>,
}

/// A message from macOS to a process observing it.
#[derive(Debug, Clone, PartialEq)]
pub enum Notification {
    /// Accessibility: the app created a window.
    WindowCreated(pid_t, WindowServerId),
    /// Accessibility: the app destroyed a window.
    WindowDestroyed(pid_t, WindowServerId),
    /// Accessibility: the app's main window changed.
    MainWindowChanged(pid_t),
    /// Workspace and accessibility: the app became frontmost.
    AppActivated(pid_t),
    /// Workspace and accessibility: the app stopped being frontmost.
    AppDeactivated(pid_t),
    /// Workspace: the desktop shown on the screen changed.
    ActiveSpaceChanged,
    /// Dock: Mission Control opened or closed.
    MissionControl(bool),
    /// Internal to the window server: finish placing a new window.
    PlaceWindow(WindowServerId),
}

/// What a raise changed, for the caller to report.
#[derive(Debug, Default, PartialEq)]
pub struct RaiseOutcome {
    pub activated: bool,
    pub main_window_changed: bool,
}

impl World {
    pub fn new(screen: CGRect, desktops: usize) -> World {
        World {
            screen,
            desktops: (1..=desktops as u64).map(SpaceId::new).collect(),
            current: 0,
            mission_control: false,
            windows: BTreeMap::new(),
            order: Vec::new(),
            apps: BTreeMap::new(),
            frontmost: None,
            next_wsid: 1,
            next_pid: 1,
            notifications: VecDeque::new(),
        }
    }

    /// Take the next queued notification.
    pub fn next_notification(&mut self) -> Option<Notification> {
        self.notifications.pop_front()
    }

    fn notify(&mut self, notification: Notification) {
        self.notifications.push_back(notification);
    }

    // Things the user and apps do.

    /// Launch an app and make it frontmost.
    pub fn launch_app(&mut self, name: &str) -> pid_t {
        let pid = self.next_pid;
        self.next_pid += 1;
        self.apps.insert(
            pid,
            App {
                name: name.to_owned(),
                windows: Vec::new(),
                main_window: None,
            },
        );
        self.activate(pid);
        pid
    }

    /// Open a window on the desktop being shown. It becomes its app's main
    /// window.
    pub fn open_window(&mut self, pid: pid_t, title: &str) -> WindowServerId {
        let wsid = WindowServerId::new(self.next_wsid);
        self.next_wsid += 1;
        let offset = 30.0 * f64::from(wsid.0);
        self.windows.insert(
            wsid,
            Window {
                pid,
                title: title.to_owned(),
                frame: CGRect::new(CGPoint::new(offset, offset), CGSize::new(400.0, 300.0)),
                desktop: self.current_desktop(),
                placed: false,
            },
        );
        self.order.insert(0, wsid);
        let app = self.apps.get_mut(&pid).expect("app is not running");
        app.windows.push(wsid);
        app.main_window = Some(wsid);
        self.notify(Notification::WindowCreated(pid, wsid));
        self.notify(Notification::MainWindowChanged(pid));
        self.notify(Notification::PlaceWindow(wsid));
        wsid
    }

    /// Close a window. The app's most recent remaining window becomes its main
    /// window.
    pub fn close_window(&mut self, wsid: WindowServerId) {
        let window = self.windows.remove(&wsid).expect("window does not exist");
        self.order.retain(|&w| w != wsid);
        self.notify(Notification::WindowDestroyed(window.pid, wsid));
        let app = self.apps.get_mut(&window.pid).unwrap();
        app.windows.retain(|&w| w != wsid);
        if app.main_window == Some(wsid) {
            app.main_window = app.windows.last().copied();
            self.notify(Notification::MainWindowChanged(window.pid));
        }
    }

    pub fn open_mission_control(&mut self) {
        assert!(!self.mission_control, "Mission Control is already open");
        self.mission_control = true;
        self.notify(Notification::MissionControl(true));
    }

    pub fn close_mission_control(&mut self) {
        assert!(self.mission_control, "Mission Control is not open");
        self.mission_control = false;
        self.notify(Notification::MissionControl(false));
        self.focus_front_window();
    }

    /// Show another desktop, counting from 1. Outside Mission Control this
    /// gives the keyboard focus to the front window there.
    pub fn switch_desktop(&mut self, number: usize) {
        self.show_desktop(self.desktops[number - 1]);
        if !self.mission_control {
            self.focus_front_window();
        }
    }

    /// Give the keyboard focus to the front window on the desktop being shown.
    fn focus_front_window(&mut self) {
        let desktop = self.current_desktop();
        let Some(&front) = self.order.iter().find(|w| self.windows[w].desktop == desktop) else {
            return;
        };
        let pid = self.windows[&front].pid;
        if self.frontmost != Some(pid) {
            self.activate(pid);
        }
        let app = self.apps.get_mut(&pid).unwrap();
        if app.main_window != Some(front) {
            app.main_window = Some(front);
            self.notify(Notification::MainWindowChanged(pid));
        }
    }

    /// Drag a window onto another desktop in Mission Control. Nothing is told
    /// about this; the window server just reports the window's new desktop.
    pub fn move_window_to_desktop(&mut self, wsid: WindowServerId, number: usize) {
        assert!(self.mission_control, "Mission Control is not open");
        let desktop = self.desktops[number - 1];
        self.windows.get_mut(&wsid).expect("window does not exist").desktop = desktop;
    }

    fn show_desktop(&mut self, desktop: SpaceId) {
        let index = self.desktops.iter().position(|&d| d == desktop).unwrap();
        if index != self.current {
            self.current = index;
            self.notify(Notification::ActiveSpaceChanged);
        }
    }

    // Things processes ask the window server and apps to do.

    /// Finish placing a new window.
    pub fn place_window(&mut self, wsid: WindowServerId) {
        if let Some(window) = self.windows.get_mut(&wsid) {
            window.placed = true;
        }
    }

    pub fn set_frame(&mut self, wsid: WindowServerId, frame: CGRect) {
        if let Some(window) = self.windows.get_mut(&wsid) {
            window.frame = frame;
        }
    }

    /// Make `pid` frontmost and bring `windows` to the front, the last one on
    /// top as the app's main window.
    ///
    /// The window server shows the desktop of a window that takes the keyboard
    /// focus, so this switches desktops when the last window is on another one.
    pub fn raise(&mut self, pid: pid_t, windows: &[WindowServerId]) -> RaiseOutcome {
        let mut outcome = RaiseOutcome::default();
        let windows: Vec<_> =
            windows.iter().copied().filter(|w| self.windows.contains_key(w)).collect();
        let Some(&top) = windows.last() else {
            return outcome;
        };
        if self.frontmost != Some(pid) {
            self.activate(pid);
            outcome.activated = true;
        }
        for &wsid in &windows {
            self.order.retain(|&w| w != wsid);
            self.order.insert(0, wsid);
        }
        let app = self.apps.get_mut(&pid).unwrap();
        if app.main_window != Some(top) {
            app.main_window = Some(top);
            outcome.main_window_changed = true;
        }
        self.show_desktop(self.windows[&top].desktop);
        outcome
    }

    fn activate(&mut self, pid: pid_t) {
        if let Some(old) = self.frontmost.replace(pid) {
            self.notify(Notification::AppDeactivated(old));
        }
        self.notify(Notification::AppActivated(pid));
    }

    // What processes can read.

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

    pub fn mission_control(&self) -> bool {
        self.mission_control
    }

    pub fn frontmost(&self) -> Option<pid_t> {
        self.frontmost
    }

    pub fn app(&self, pid: pid_t) -> &App {
        &self.apps[&pid]
    }

    pub fn apps(&self) -> impl Iterator<Item = pid_t> + '_ {
        self.apps.keys().copied()
    }

    pub fn window(&self, wsid: WindowServerId) -> Option<&Window> {
        self.windows.get(&wsid)
    }

    pub fn window_titled(&self, title: &str) -> WindowServerId {
        let mut matches = self.windows.iter().filter(|(_, w)| w.title == title);
        let (&wsid, _) = matches.next().unwrap_or_else(|| panic!("no window titled {title}"));
        assert!(matches.next().is_none(), "more than one window titled {title}");
        wsid
    }

    /// What the window server reports for one window.
    pub fn window_server_info(&self, wsid: WindowServerId) -> Option<WindowServerInfo> {
        let window = self.windows.get(&wsid)?;
        Some(WindowServerInfo {
            id: wsid,
            pid: window.pid,
            layer: 0,
            frame: if window.placed {
                window.frame
            } else {
                CGRect::ZERO
            },
            spaces: if window.placed {
                vec![window.desktop]
            } else {
                vec![]
            },
        })
    }

    /// What the window server reports for every window, on any desktop.
    pub fn all_window_server_info(&self) -> Vec<WindowServerInfo> {
        self.order.iter().filter_map(|&w| self.window_server_info(w)).collect()
    }

    /// The windows on screen, front to back.
    pub fn windows_on_screen(&self) -> Vec<WindowServerInfo> {
        let desktop = self.current_desktop();
        self.order
            .iter()
            .filter(|w| self.windows[w].placed && self.windows[w].desktop == desktop)
            .filter_map(|&w| self.window_server_info(w))
            .collect()
    }
}
