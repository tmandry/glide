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
//! Only these happen without a queued message in between:
//!
//! - Something the user does is handled by one process right away.
//! - A window server query reads the window server before returning.
//! - An app gets the id of a window it creates from the window server.
//!
//! An accessibility request blocks its caller until the app replies. The app
//! gets to the request after everything already in its inbox, so
//! [`Mac::ax_request`] has the app handle its inbox and then the request.
//!
//! A [`Notification`] is for a process observing macOS from outside. The model
//! knows nothing about Glide; the caller picks which process handles its next
//! message with [`Mac::step`] and takes notifications with
//! [`Mac::next_notification`].

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;
use std::time::Duration;

use accessibility::AXError;
use accessibility_sys::{
    kAXApplicationActivatedNotification, kAXApplicationDeactivatedNotification,
    kAXFrontmostAttribute, kAXMainWindowAttribute, kAXMainWindowChangedNotification,
    kAXMinimizedAttribute, kAXPositionAttribute, kAXRaiseAction, kAXRoleAttribute,
    kAXSizeAttribute, kAXStandardWindowSubrole, kAXSubroleAttribute, kAXTitleAttribute,
    kAXUIElementDestroyedNotification, kAXWindowCreatedNotification,
    kAXWindowDeminiaturizedNotification, kAXWindowMiniaturizedNotification,
    kAXWindowMovedNotification, kAXWindowResizedNotification, kAXWindowRole, kAXWindowsAttribute,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};

use crate::sys::app::pid_t;
use crate::sys::ax::fake::{
    self, ENHANCED_USER_INTERFACE_ATTRIBUTE, ElementId, FRAME_ATTRIBUTE, ObserverId, Request, Value,
};
use crate::sys::event::MouseState;
use crate::sys::screen::SpaceId;
use crate::sys::window_server::{WindowServerId, WindowServerInfo};

/// How long an accessibility request waits for a reply.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(6);

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
    /// From an app: the window was minimized to the Dock, or restored from it.
    /// A restored window comes to the front and its app is activated.
    SetMinimized(WindowServerId, bool),
    /// Give the window the keyboard focus, activating its app.
    MakeKey(WindowServerId),
    /// From the user: click the window. It comes to the front and takes the
    /// keyboard focus.
    Click(WindowServerId),
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
    /// One of the app's windows is now on the desktop being shown, or no
    /// longer is.
    WindowShown(WindowServerId, bool),
}

/// Something a process observing macOS is told.
#[derive(Debug, Clone, PartialEq)]
pub enum Notification {
    /// Accessibility, from an app to one of its observers, about `element`.
    Ax {
        pid: pid_t,
        observer: ObserverId,
        element: ElementId,
        name: &'static str,
    },
    /// Accessibility, from the Dock: Mission Control opened or closed.
    MissionControl(bool),
    /// From the workspace notification center.
    Workspace(WorkspaceNotification),
}

#[derive(Debug, Clone, PartialEq)]
pub enum WorkspaceNotification {
    AppLaunched(pid_t),
    AppActivated(pid_t),
    AppDeactivated(pid_t),
    ActiveSpaceChanged,
}

/// How a window behaves, beyond what every window does.
#[derive(Debug, Clone, Default)]
pub struct WindowTraits {
    /// The smallest size the app lets the window have.
    pub min_size: Option<CGSize>,
    /// The app only lets the window have a size that is a multiple of this,
    /// as a terminal does with its character cells.
    pub size_increment: Option<CGSize>,
    /// The window's accessibility subrole, such as `AXDialog`. A standard
    /// window if unset.
    pub subrole: Option<&'static str>,
}

impl WindowTraits {
    /// The size the app gives a window that is asked to have `size`: at least
    /// the minimum, then rounded down to a multiple of the increment, or up
    /// if that would go below the minimum.
    fn adjust_size(&self, size: CGSize) -> CGSize {
        let min = self.min_size.unwrap_or(CGSize::new(0.0, 0.0));
        let size = CGSize::new(size.width.max(min.width), size.height.max(min.height));
        let Some(step) = self.size_increment else {
            return size;
        };
        let round = |length: f64, step: f64, min: f64| {
            if step <= 0.0 {
                return length;
            }
            let rounded = (length / step).floor() * step;
            if rounded < min {
                rounded + step
            } else {
                rounded
            }
        };
        CGSize::new(
            round(size.width, step.width, min.width),
            round(size.height, step.height, min.height),
        )
    }
}

/// Something a process was asked to do to a window through accessibility.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Wrote(String),
    Raised(String),
}

/// Something a process sent while handling a message or a request.
#[derive(Debug, Clone, PartialEq)]
enum Sent {
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
    /// A minimized window is in no on-screen list.
    minimized: bool,
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
                minimized: false,
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
            let (old, new) = (self.desktops[self.current], self.desktops[index]);
            self.current = index;
            for (&wsid, window) in &self.windows {
                if window.desktop == Some(old) {
                    out.send(Process::App(window.pid), Message::WindowShown(wsid, false));
                } else if window.desktop == Some(new) {
                    out.send(Process::App(window.pid), Message::WindowShown(wsid, true));
                }
            }
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
                out.send(Process::App(window.pid), Message::WindowShown(wsid, true));
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
            Message::SetMinimized(wsid, minimized) => {
                let Some(window) = self.windows.get_mut(&wsid) else {
                    return;
                };
                window.minimized = minimized;
                let pid = window.pid;
                if !minimized {
                    if let Some(at) = self.order.iter().position(|&w| w == wsid) {
                        self.order.remove(at);
                        self.order.insert(0, wsid);
                    }
                    self.activate(pid, out);
                }
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
            Message::Click(wsid) => {
                let Some(window) = self.windows.get(&wsid) else {
                    return;
                };
                let pid = window.pid;
                self.activate(pid, out);
                out.send(Process::App(pid), Message::BecomeMain(wsid));
                if let Some(at) = self.order.iter().position(|&w| w == wsid) {
                    self.order.remove(at);
                    self.order.insert(0, wsid);
                }
            }
            Message::ShowDesktop(number) => self.show_desktop(number - 1, out),
            Message::MoveToDesktop(wsid, number) => {
                let desktop = self.desktops[number - 1];
                let shown = desktop == self.current_desktop();
                if let Some(window) = self.windows.get_mut(&wsid) {
                    window.desktop = Some(desktop);
                    out.send(Process::App(window.pid), Message::WindowShown(wsid, shown));
                }
            }
            Message::FocusFrontWindow => {
                let desktop = Some(self.current_desktop());
                let shown = |w: &&WindowServerId| {
                    self.windows[*w].desktop == desktop && !self.windows[*w].minimized
                };
                let Some(&front) = self.order.iter().find(shown) else {
                    return;
                };
                let pid = self.windows[&front].pid;
                self.activate(pid, out);
                out.send(Process::App(pid), Message::BecomeMain(front));
            }
            Message::Activate
            | Message::Deactivate
            | Message::BecomeMain(_)
            | Message::WindowShown(..) => {
                unreachable!("{message:?} is for an app")
            }
        }
    }

    // What other processes can ask the window server.

    pub fn screen(&self) -> CGRect {
        self.screen
    }

    pub fn frontmost_app(&self) -> Option<pid_t> {
        self.frontmost
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
            .filter(|w| self.windows[w].desktop == desktop && !self.windows[w].minimized)
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
    enhanced_user_interface: bool,
    /// The notifications observers asked for, each on one element.
    observers: Vec<(ObserverId, ElementId, &'static str)>,
    /// Whether the app has stopped handling messages and requests.
    hung: bool,
    /// How long the app takes to handle each message and request.
    delay: Duration,
}

struct AppWindow {
    id: WindowServerId,
    title: String,
    frame: CGRect,
    traits: WindowTraits,
    minimized: bool,
    /// Whether the app has been told the window is on the desktop being
    /// shown. Accessibility only lists the app's windows that are.
    shown: bool,
}

fn element(wsid: WindowServerId) -> ElementId {
    ElementId::Window(wsid.0)
}

impl App {
    /// Send `name` about `subject` to the observers that asked for it on
    /// `registered_on`.
    fn notify(
        &self,
        registered_on: ElementId,
        name: &'static str,
        subject: ElementId,
        out: &mut Outbox,
    ) {
        for &(observer, on, wanted) in &self.observers {
            if on == registered_on && wanted == name {
                out.notify(Notification::Ax {
                    pid: self.pid,
                    observer,
                    element: subject,
                    name,
                });
            }
        }
    }

    fn set_main_window(&mut self, wsid: Option<WindowServerId>, out: &mut Outbox) {
        if self.main_window != wsid {
            self.main_window = wsid;
            let subject = wsid.map_or(ElementId::Application, element);
            self.notify(
                ElementId::Application,
                kAXMainWindowChangedNotification,
                subject,
                out,
            );
        }
    }

    /// Show a new window, which becomes the main window. The app reports the
    /// new main window before the window's creation if `main_first` is set.
    fn open_window(
        &mut self,
        id: WindowServerId,
        title: &str,
        traits: WindowTraits,
        main_first: bool,
        out: &mut Outbox,
    ) {
        let offset = 30.0 * f64::from(id.0);
        let size = traits.adjust_size(CGSize::new(400.0, 300.0));
        let frame = CGRect::new(CGPoint::new(offset, offset), size);
        self.windows.push(AppWindow {
            id,
            title: title.to_owned(),
            frame,
            traits,
            minimized: false,
            // The app puts the window on the desktop it believes is shown.
            // The window server says where it really went.
            shown: true,
        });
        if main_first {
            self.set_main_window(Some(id), out);
        }
        self.notify(
            ElementId::Application,
            kAXWindowCreatedNotification,
            element(id),
            out,
        );
        self.set_main_window(Some(id), out);
        out.send(Process::WindowServer, Message::OrderIn(id, frame));
    }

    /// Move or resize a window as the user would by dragging it.
    fn set_frame(&mut self, wsid: WindowServerId, frame: CGRect, out: &mut Outbox) {
        let Some(window) = self.windows.iter_mut().find(|w| w.id == wsid) else {
            return;
        };
        let old = std::mem::replace(&mut window.frame, frame);
        if old == frame {
            return;
        }
        out.send(Process::WindowServer, Message::SetFrame(wsid, frame));
        if old.origin != frame.origin {
            self.notify(element(wsid), kAXWindowMovedNotification, element(wsid), out);
        }
        if old.size != frame.size {
            self.notify(element(wsid), kAXWindowResizedNotification, element(wsid), out);
        }
    }

    /// Minimize a window to the Dock, or restore it. Minimizing the main
    /// window makes the most recent window that isn't minimized the main
    /// window, and a restored window becomes the main window.
    fn set_minimized(&mut self, wsid: WindowServerId, minimized: bool, out: &mut Outbox) {
        let Some(window) = self.windows.iter_mut().find(|w| w.id == wsid) else {
            return;
        };
        if window.minimized == minimized {
            return;
        }
        window.minimized = minimized;
        let name = if minimized {
            kAXWindowMiniaturizedNotification
        } else {
            kAXWindowDeminiaturizedNotification
        };
        self.notify(element(wsid), name, element(wsid), out);
        out.send(Process::WindowServer, Message::SetMinimized(wsid, minimized));
        if !minimized {
            self.set_main_window(Some(wsid), out);
        } else if self.main_window == Some(wsid) {
            let next = self.windows.iter().rev().find(|w| !w.minimized);
            self.set_main_window(next.map(|w| w.id), out);
        }
    }

    /// Close a window. The most recent remaining window that isn't minimized
    /// becomes the main window.
    fn close_window(&mut self, wsid: WindowServerId, out: &mut Outbox) {
        self.windows.retain(|w| w.id != wsid);
        self.notify(
            element(wsid),
            kAXUIElementDestroyedNotification,
            element(wsid),
            out,
        );
        self.observers.retain(|&(_, on, _)| on != element(wsid));
        if self.main_window == Some(wsid) {
            let next = self.windows.iter().rev().find(|w| !w.minimized);
            self.set_main_window(next.map(|w| w.id), out);
        }
        out.send(Process::WindowServer, Message::DestroyWindow(wsid));
    }

    fn handle(&mut self, message: Message, out: &mut Outbox) {
        let app = ElementId::Application;
        match message {
            Message::Activate => {
                self.active = true;
                self.notify(app, kAXApplicationActivatedNotification, app, out);
            }
            Message::Deactivate => {
                self.active = false;
                self.notify(app, kAXApplicationDeactivatedNotification, app, out);
            }
            Message::BecomeMain(wsid) => {
                if self.windows.iter().any(|w| w.id == wsid) {
                    self.set_main_window(Some(wsid), out);
                }
            }
            Message::WindowShown(wsid, shown) => {
                if let Some(window) = self.windows.iter_mut().find(|w| w.id == wsid) {
                    window.shown = shown;
                }
            }
            _ => unreachable!("{message:?} is for the window server"),
        }
    }

    fn window_mut(&mut self, id: u32) -> Result<&mut AppWindow, AXError> {
        self.windows.iter_mut().find(|w| w.id.0 == id).ok_or(AXError::InvalidUIElement)
    }

    /// Answer an accessibility request. `Ok(None)` is an attribute with no
    /// value.
    #[allow(non_upper_case_globals)]
    fn ax(&mut self, request: Request, out: &mut Outbox) -> Result<Option<Value>, AXError> {
        use ElementId::{Application, Window};
        let value = match request {
            Request::Attribute(Application, name) => match &*name {
                kAXRoleAttribute => Value::String("AXApplication".into()),
                kAXWindowsAttribute => {
                    let shown = self.windows.iter().filter(|w| w.shown);
                    Value::Elements(shown.map(|w| element(w.id)).collect())
                }
                kAXMainWindowAttribute => {
                    return Ok(self.main_window.map(element).map(Value::Element));
                }
                kAXFrontmostAttribute => Value::Bool(self.active),
                ENHANCED_USER_INTERFACE_ATTRIBUTE => Value::Bool(self.enhanced_user_interface),
                _ => return Err(AXError::AttributeUnsupported),
            },
            Request::Attribute(Window(id), name) => {
                let window = self.window_mut(id)?;
                match &*name {
                    kAXRoleAttribute => Value::String(kAXWindowRole.into()),
                    kAXSubroleAttribute => Value::String(
                        window.traits.subrole.unwrap_or(kAXStandardWindowSubrole).into(),
                    ),
                    kAXTitleAttribute => Value::String(window.title.clone()),
                    kAXMinimizedAttribute => Value::Bool(window.minimized),
                    kAXPositionAttribute => Value::Point(window.frame.origin),
                    kAXSizeAttribute => Value::Size(window.frame.size),
                    FRAME_ATTRIBUTE => Value::Rect(window.frame),
                    _ => return Err(AXError::AttributeUnsupported),
                }
            }
            Request::SetAttribute(Application, name, Value::Bool(enabled))
                if name == ENHANCED_USER_INTERFACE_ATTRIBUTE =>
            {
                self.enhanced_user_interface = enabled;
                return Ok(None);
            }
            Request::SetAttribute(Window(id), name, value) => {
                let window = self.window_mut(id)?;
                let old = window.frame;
                let notification = match (&*name, value) {
                    (kAXPositionAttribute, Value::Point(origin)) => {
                        window.frame.origin = origin;
                        kAXWindowMovedNotification
                    }
                    (kAXSizeAttribute, Value::Size(size)) => {
                        window.frame.size = window.traits.adjust_size(size);
                        kAXWindowResizedNotification
                    }
                    _ => return Err(AXError::AttributeUnsupported),
                };
                let (wsid, frame) = (window.id, window.frame);
                if frame != old {
                    out.send(Process::WindowServer, Message::SetFrame(wsid, frame));
                    self.notify(Window(id), notification, Window(id), out);
                }
                return Ok(None);
            }
            Request::IsSettable(Window(id), name) => {
                self.window_mut(id)?;
                Value::Bool(matches!(&*name, kAXPositionAttribute | kAXSizeAttribute))
            }
            Request::PerformAction(Window(id), name) if name == kAXRaiseAction => {
                let wsid = self.window_mut(id)?.id;
                self.set_main_window(Some(wsid), out);
                out.send(Process::WindowServer, Message::OrderFront(wsid));
                return Ok(None);
            }
            Request::WindowId(Window(id)) => Value::Number(self.window_mut(id)?.id.0),
            Request::WindowId(Application) => return Err(AXError::IllegalArgument),
            Request::AddNotification(observer, on, name) => {
                if let Window(id) = on {
                    self.window_mut(id)?;
                }
                if self.observers.contains(&(observer, on, name)) {
                    return Err(AXError::NotificationAlreadyRegistered);
                }
                self.observers.push((observer, on, name));
                return Ok(None);
            }
            Request::RemoveNotification(observer, on, name) => {
                let before = self.observers.len();
                self.observers.retain(|&registered| registered != (observer, on, name));
                if self.observers.len() == before {
                    return Err(AXError::NotificationNotRegistered);
                }
                return Ok(None);
            }
            Request::SetAttribute(..) | Request::IsSettable(..) => {
                return Err(AXError::AttributeUnsupported);
            }
            Request::PerformAction(..) => return Err(AXError::ActionUnsupported),
        };
        Ok(Some(value))
    }
}

pub struct Mac {
    window_server: WindowServer,
    dock: Dock,
    apps: BTreeMap<pid_t, App>,
    next_pid: pid_t,
    out: Outbox,
    inboxes: BTreeMap<Process, VecDeque<Message>>,
    notifications: VecDeque<Notification>,
    /// What apps were asked to do to windows through accessibility, with
    /// whether Mission Control was open at the time.
    actions: Vec<(Action, bool)>,
}

impl Mac {
    pub fn new(screen: CGRect, desktops: usize) -> Mac {
        Mac {
            window_server: WindowServer::new(screen, desktops),
            dock: Dock::default(),
            apps: BTreeMap::new(),
            next_pid: 1,
            out: Outbox::default(),
            inboxes: BTreeMap::new(),
            notifications: VecDeque::new(),
            actions: Vec::new(),
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
                enhanced_user_interface: false,
                observers: Vec::new(),
                hung: false,
                delay: Duration::ZERO,
            },
        );
        self.window_server.launch(pid, &mut self.out);
        self.route();
        pid
    }

    /// Have an app open a window.
    pub fn open_window(&mut self, pid: pid_t, title: &str) -> WindowServerId {
        self.open_window_ordered(pid, title, WindowTraits::default(), false)
    }

    /// Have an app open a window, reporting the new main window before the
    /// window's creation.
    pub fn open_window_main_first(&mut self, pid: pid_t, title: &str) -> WindowServerId {
        self.open_window_ordered(pid, title, WindowTraits::default(), true)
    }

    /// Have an app open a window that behaves as `traits` say.
    pub fn open_window_with(
        &mut self,
        pid: pid_t,
        title: &str,
        traits: WindowTraits,
    ) -> WindowServerId {
        self.open_window_ordered(pid, title, traits, false)
    }

    fn open_window_ordered(
        &mut self,
        pid: pid_t,
        title: &str,
        traits: WindowTraits,
        main_first: bool,
    ) -> WindowServerId {
        let wsid = self.window_server.new_window(pid);
        let app = self.apps.get_mut(&pid).expect("app is not running");
        app.open_window(wsid, title, traits, main_first, &mut self.out);
        self.route();
        wsid
    }

    pub fn close_window(&mut self, wsid: WindowServerId) {
        let app = self
            .apps
            .values_mut()
            .find(|app| app.windows.iter().any(|w| w.id == wsid))
            .expect("window does not exist");
        app.close_window(wsid, &mut self.out);
        self.route();
    }

    /// Minimize a window to the Dock.
    pub fn minimize_window(&mut self, wsid: WindowServerId) {
        let app = Self::app_of(&mut self.apps, wsid);
        app.set_minimized(wsid, true, &mut self.out);
        self.route();
    }

    /// Restore a minimized window from the Dock.
    pub fn restore_window(&mut self, wsid: WindowServerId) {
        let app = Self::app_of(&mut self.apps, wsid);
        app.set_minimized(wsid, false, &mut self.out);
        self.route();
    }

    fn app_of(apps: &mut BTreeMap<pid_t, App>, wsid: WindowServerId) -> &mut App {
        apps.values_mut()
            .find(|app| app.windows.iter().any(|w| w.id == wsid))
            .expect("window does not exist")
    }

    /// Click a window on screen.
    pub fn click_window(&mut self, wsid: WindowServerId) {
        let on_screen = self.window_server.windows_on_screen();
        assert!(
            on_screen.iter().any(|w| w.id == wsid),
            "{wsid:?} is not on screen"
        );
        self.out.send(Process::WindowServer, Message::Click(wsid));
        self.route();
    }

    /// Move or resize a window with the mouse, which is released by the time
    /// anything else sees the new frame.
    pub fn set_window_frame(&mut self, wsid: WindowServerId, frame: CGRect) {
        let app = self
            .apps
            .values_mut()
            .find(|app| app.windows.iter().any(|w| w.id == wsid))
            .expect("window does not exist");
        app.set_frame(wsid, frame, &mut self.out);
        self.route();
    }

    /// Make an app stop handling messages and accessibility requests, as it
    /// does when it beachballs, or start again.
    pub fn set_app_hung(&mut self, pid: pid_t, hung: bool) {
        self.apps.get_mut(&pid).expect("app is not running").hung = hung;
    }

    /// Make an app take `delay` to handle each message and accessibility
    /// request, as a busy app does.
    pub fn set_app_delay(&mut self, pid: pid_t, delay: Duration) {
        self.apps.get_mut(&pid).expect("app is not running").delay = delay;
    }

    pub fn open_mission_control(&mut self) {
        assert!(!self.dock.mission_control, "Mission Control is already open");
        self.dock.mission_control = true;
        self.out.notify(Notification::MissionControl(true));
        self.route();
    }

    pub fn close_mission_control(&mut self) {
        assert!(self.dock.mission_control, "Mission Control is not open");
        self.dock.mission_control = false;
        self.out.notify(Notification::MissionControl(false));
        self.out.send(Process::WindowServer, Message::FocusFrontWindow);
        self.route();
    }

    /// Show another desktop, counting from 1. Outside Mission Control this
    /// gives the keyboard focus to the front window there.
    pub fn switch_desktop(&mut self, number: usize) {
        self.out.send(Process::WindowServer, Message::ShowDesktop(number));
        if !self.dock.mission_control {
            self.out.send(Process::WindowServer, Message::FocusFrontWindow);
        }
        self.route();
    }

    /// Drag a window onto another desktop in Mission Control.
    pub fn drag_window_to_desktop(&mut self, wsid: WindowServerId, number: usize) {
        assert!(self.dock.mission_control, "Mission Control is not open");
        self.out.send(Process::WindowServer, Message::MoveToDesktop(wsid, number));
        self.route();
    }

    // Delivery.

    /// Put what was sent where it goes.
    fn route(&mut self) {
        while let Some(sent) = self.out.0.pop_front() {
            match sent {
                Sent::Message(to, message) => {
                    self.inboxes.entry(to).or_default().push_back(message)
                }
                Sent::Notification(notification) => self.notifications.push_back(notification),
            }
        }
    }

    /// The processes with a message waiting that can handle it.
    pub fn ready(&self) -> impl Iterator<Item = Process> + '_ {
        self.inboxes
            .iter()
            .filter(|(_, inbox)| !inbox.is_empty())
            .map(|(&p, _)| p)
            .filter(|&p| !self.is_hung(p))
    }

    fn is_hung(&self, process: Process) -> bool {
        match process {
            Process::App(pid) => self.apps.get(&pid).is_some_and(|app| app.hung),
            Process::WindowServer => false,
        }
    }

    /// Have `process` handle its next message. Return false if it has none.
    pub fn step(&mut self, process: Process) -> bool {
        if self.is_hung(process) {
            return false;
        }
        let Some(message) = self.inboxes.get_mut(&process).and_then(|inbox| inbox.pop_front())
        else {
            return false;
        };
        match process {
            Process::WindowServer => self.window_server.handle(message, &mut self.out),
            Process::App(pid) => {
                if let Some(app) = self.apps.get_mut(&pid) {
                    fake::sleep(app.delay);
                    app.handle(message, &mut self.out);
                }
            }
        }
        self.route();
        true
    }

    /// Take the next notification for a process outside macOS.
    pub fn next_notification(&mut self) -> Option<Notification> {
        self.notifications.pop_front()
    }

    // What a process outside macOS can do.

    pub fn window_server(&self) -> &WindowServer {
        &self.window_server
    }

    /// Send the window server a message.
    pub fn send_window_server(&mut self, message: Message) {
        self.out.send(Process::WindowServer, message);
        self.route();
    }

    /// Send an app an accessibility request and wait for the reply. A hung
    /// app never replies, and the request fails once it times out.
    pub fn ax_request(&mut self, pid: pid_t, request: Request) -> Result<Option<Value>, AXError> {
        let Some(app) = self.apps.get(&pid) else {
            return Err(AXError::CannotComplete);
        };
        let action: Option<(fn(String) -> Action, u32)> = match &request {
            Request::SetAttribute(ElementId::Window(id), ..) => Some((Action::Wrote, *id)),
            Request::PerformAction(ElementId::Window(id), _) => Some((Action::Raised, *id)),
            _ => None,
        };
        if let Some((action, id)) = action
            && let Some(window) = app.windows.iter().find(|w| w.id.0 == id)
        {
            self.actions.push((action(window.title.clone()), self.dock.mission_control));
        }
        if app.hung {
            fake::sleep(REQUEST_TIMEOUT);
            return Err(AXError::CannotComplete);
        }
        // The app gets to the request after what is already in its inbox.
        while self.step(Process::App(pid)) {}

        let app = self.apps.get_mut(&pid).unwrap();
        fake::sleep(app.delay);
        let reply = app.ax(request, &mut self.out);
        self.route();
        reply
    }

    pub fn bundle_id(&self, pid: pid_t) -> Option<String> {
        self.apps.get(&pid).map(|app| format!("com.example.{}", app.name))
    }

    pub fn app_name(&self, pid: pid_t) -> Option<&str> {
        self.apps.get(&pid).map(|app| &*app.name)
    }

    pub fn apps(&self) -> impl Iterator<Item = pid_t> + '_ {
        self.apps.keys().copied()
    }

    // What a test can see. Nothing in the system has this view.

    /// The title of the window with the keyboard focus: the main window of
    /// the app the window server has frontmost.
    pub fn key_window(&self) -> Option<&str> {
        let app = self.apps.get(&self.window_server.frontmost?)?;
        self.title(app.main_window?)
    }

    pub fn any_app_hung(&self) -> bool {
        self.apps.values().any(|app| app.hung)
    }

    pub fn mission_control_open(&self) -> bool {
        self.dock.mission_control
    }

    /// The notifications each observer asked for on each window, sorted, with
    /// the window's title.
    pub fn window_notifications(&self) -> Vec<(String, Vec<&'static str>)> {
        let mut asked: BTreeMap<(String, ObserverId), Vec<&'static str>> = BTreeMap::new();
        for app in self.apps.values() {
            for &(observer, on, name) in &app.observers {
                let ElementId::Window(id) = on else { continue };
                let Some(window) = app.windows.iter().find(|w| w.id.0 == id) else {
                    continue;
                };
                asked.entry((window.title.clone(), observer)).or_default().push(name);
            }
        }
        asked
            .into_iter()
            .map(|((title, _), mut names)| {
                names.sort();
                (title, names)
            })
            .collect()
    }

    pub fn actions(&self) -> &[(Action, bool)] {
        &self.actions
    }

    pub fn clear_actions(&mut self) {
        self.actions.clear();
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

/// Connects the fake accessibility interface to a [`Mac`].
pub struct System(pub Rc<RefCell<Mac>>);

impl fake::System for System {
    fn ax(&self, pid: pid_t, request: Request) -> Result<Option<Value>, AXError> {
        self.0.borrow_mut().ax_request(pid, request)
    }

    fn make_key_window(&self, _pid: pid_t, wsid: WindowServerId) -> Result<(), ()> {
        self.0.borrow_mut().send_window_server(Message::MakeKey(wsid));
        Ok(())
    }

    fn bundle_id(&self, pid: pid_t) -> Option<String> {
        self.0.borrow().bundle_id(pid)
    }

    fn mouse_state(&self) -> MouseState {
        MouseState::Up
    }
}
