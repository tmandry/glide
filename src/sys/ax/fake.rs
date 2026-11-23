// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Fakes with the shape of the accessibility interface.
//!
//! A fake element is only a name: a pid and an [`ElementId`]. The real
//! interface sends a request to the app and blocks for the reply, and the
//! fake does the same through [`System::ax`]. The [`System`] is installed per
//! thread with [`install`], and decides what apps, elements and attributes
//! exist. Each thread also has its own clock, which [`set_clock`] and
//! [`advance_clock`] move.
//!
//! Attributes are read and written by name as [`Value`]s. The typed methods
//! on [`AXUIElement`] are the ones the real interface has for the attributes
//! Glide uses.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::marker::PhantomData;
use std::rc::Rc;
use std::time::{Duration, Instant};

use accessibility::{AXError, Error};
use accessibility_sys::{
    kAXFrontmostAttribute, kAXMainWindowAttribute, kAXPositionAttribute, kAXRaiseAction,
    kAXRoleAttribute, kAXSizeAttribute, kAXSubroleAttribute, kAXTitleAttribute,
    kAXWindowsAttribute,
};
use objc2_core_foundation::{CFBoolean, CFRetained, CFString, CGPoint, CGRect, CGSize};

use crate::sys::app::pid_t;
use crate::sys::event::MouseState;
use crate::sys::window_server::WindowServerId;

pub const FRAME_ATTRIBUTE: &str = "AXFrame";
pub const ENHANCED_USER_INTERFACE_ATTRIBUTE: &str = "AXEnhancedUserInterface";

/// An element within one app.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ElementId {
    Application,
    /// A window, by a number its app chose.
    Window(u32),
}

/// The value of an attribute.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    Number(u32),
    String(String),
    Point(CGPoint),
    Size(CGSize),
    Rect(CGRect),
    Element(ElementId),
    Elements(Vec<ElementId>),
}

/// A request to an app's accessibility interface.
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    Attribute(ElementId, String),
    SetAttribute(ElementId, String, Value),
    IsSettable(ElementId, String),
    PerformAction(ElementId, String),
    /// The window server id of a window element, as a [`Value::Number`].
    WindowId(ElementId),
    AddNotification(ObserverId, ElementId, &'static str),
    RemoveNotification(ObserverId, ElementId, &'static str),
}

/// The system the fakes talk to.
pub trait System {
    /// Send `request` to the app and wait for its reply.
    fn ax(&self, pid: pid_t, request: Request) -> Result<Option<Value>, AXError>;
    /// Ask the window server to give a window the keyboard focus.
    fn make_key_window(&self, pid: pid_t, wsid: WindowServerId) -> Result<(), ()>;
    /// The bundle id of a running app, or `None` if `pid` isn't running.
    fn bundle_id(&self, pid: pid_t) -> Option<String>;
    fn mouse_state(&self) -> MouseState;
    /// Block the thread for `duration`.
    fn sleep(&self, duration: Duration);
}

thread_local! {
    static SYSTEM: RefCell<Option<Rc<dyn System>>> = const { RefCell::new(None) };
    static OBSERVERS: RefCell<HashMap<ObserverId, Rc<dyn Fn(Element, &str)>>> =
        RefCell::new(HashMap::new());
    static NEXT_OBSERVER: Cell<u32> = const { Cell::new(1) };
    static CLOCK: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Use `system` on this thread until the guard is dropped.
#[must_use]
pub fn install(system: Rc<dyn System>) -> SystemGuard {
    let previous = SYSTEM.replace(Some(system));
    SystemGuard(previous)
}

pub struct SystemGuard(Option<Rc<dyn System>>);

impl Drop for SystemGuard {
    fn drop(&mut self) {
        SYSTEM.set(self.0.take());
    }
}

fn system() -> Rc<dyn System> {
    SYSTEM
        .with_borrow(|s| s.clone())
        .expect("no fake system is installed on this thread")
}

// Elements.

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct AXUIElement {
    pid: pid_t,
    id: ElementId,
}

/// An owned reference to an accessibility element.
pub type Element = Rc<AXUIElement>;

impl std::fmt::Debug for AXUIElement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}({})", self.id, self.pid)
    }
}

/// An attribute name, for [`AXUIElement::is_settable`].
pub struct AXAttribute(&'static str);

impl AXAttribute {
    pub fn size() -> AXAttribute {
        AXAttribute(kAXSizeAttribute)
    }
}

fn wrong_type<T>(attribute: &str, value: Option<Value>) -> Result<T, Error> {
    panic!("fake system returned {value:?} for {attribute}")
}

impl AXUIElement {
    pub fn application(pid: pid_t) -> Element {
        Self::new(pid, ElementId::Application)
    }

    pub fn new(pid: pid_t, id: ElementId) -> Element {
        Rc::new(AXUIElement { pid, id })
    }

    pub fn pid(&self) -> pid_t {
        self.pid
    }

    pub fn id(&self) -> ElementId {
        self.id
    }

    fn request(&self, request: Request) -> Result<Option<Value>, Error> {
        system().ax(self.pid, request).map_err(Error::Ax)
    }

    /// Read an attribute by name.
    pub fn attribute(&self, name: &str) -> Result<Value, Error> {
        self.request(Request::Attribute(self.id, name.to_owned()))?
            .ok_or(Error::Ax(AXError::NoValue))
    }

    /// Write an attribute by name.
    pub fn set_attribute(&self, name: &str, value: Value) -> Result<(), Error> {
        self.request(Request::SetAttribute(self.id, name.to_owned(), value)).map(|_| ())
    }

    pub fn perform_action(&self, name: &str) -> Result<(), Error> {
        self.request(Request::PerformAction(self.id, name.to_owned())).map(|_| ())
    }

    pub fn is_settable(&self, attribute: &AXAttribute) -> Result<bool, Error> {
        match self.request(Request::IsSettable(self.id, attribute.0.to_owned()))? {
            Some(Value::Bool(settable)) => Ok(settable),
            value => wrong_type(attribute.0, value),
        }
    }

    fn string(&self, name: &str) -> Result<CFRetained<CFString>, Error> {
        match self.attribute(name)? {
            Value::String(s) => Ok(CFString::from_str(&s)),
            value => wrong_type(name, Some(value)),
        }
    }
}

/// The typed attributes the real element has, for the ones Glide uses.
pub trait AXUIElementAttributes {
    fn role(&self) -> Result<CFRetained<CFString>, Error>;
    fn subrole(&self) -> Result<CFRetained<CFString>, Error>;
    fn title(&self) -> Result<CFRetained<CFString>, Error>;
    fn windows(&self) -> Result<Vec<Element>, Error>;
    fn main_window(&self) -> Result<Element, Error>;
    fn frontmost(&self) -> Result<&'static CFBoolean, Error>;
    fn frame(&self) -> Result<CGRect, Error>;
    fn set_position(&self, position: CGPoint) -> Result<(), Error>;
    fn set_size(&self, size: CGSize) -> Result<(), Error>;
}

impl AXUIElementAttributes for AXUIElement {
    fn role(&self) -> Result<CFRetained<CFString>, Error> {
        self.string(kAXRoleAttribute)
    }

    fn subrole(&self) -> Result<CFRetained<CFString>, Error> {
        self.string(kAXSubroleAttribute)
    }

    fn title(&self) -> Result<CFRetained<CFString>, Error> {
        self.string(kAXTitleAttribute)
    }

    fn windows(&self) -> Result<Vec<Element>, Error> {
        match self.attribute(kAXWindowsAttribute)? {
            Value::Elements(ids) => Ok(ids.into_iter().map(|id| Self::new(self.pid, id)).collect()),
            value => wrong_type(kAXWindowsAttribute, Some(value)),
        }
    }

    fn main_window(&self) -> Result<Element, Error> {
        match self.attribute(kAXMainWindowAttribute)? {
            Value::Element(id) => Ok(Self::new(self.pid, id)),
            value => wrong_type(kAXMainWindowAttribute, Some(value)),
        }
    }

    fn frontmost(&self) -> Result<&'static CFBoolean, Error> {
        match self.attribute(kAXFrontmostAttribute)? {
            Value::Bool(frontmost) => Ok(CFBoolean::new(frontmost)),
            value => wrong_type(kAXFrontmostAttribute, Some(value)),
        }
    }

    fn frame(&self) -> Result<CGRect, Error> {
        match self.attribute(FRAME_ATTRIBUTE)? {
            Value::Rect(frame) => Ok(frame),
            value => wrong_type(FRAME_ATTRIBUTE, Some(value)),
        }
    }

    fn set_position(&self, position: CGPoint) -> Result<(), Error> {
        self.set_attribute(kAXPositionAttribute, Value::Point(position))
    }

    fn set_size(&self, size: CGSize) -> Result<(), Error> {
        self.set_attribute(kAXSizeAttribute, Value::Size(size))
    }
}

pub trait AXUIElementActions {
    fn raise(&self) -> Result<(), Error>;
}

impl AXUIElementActions for AXUIElement {
    fn raise(&self) -> Result<(), Error> {
        self.perform_action(kAXRaiseAction)
    }
}

pub trait AXUIElementExt {
    fn enhanced_user_interface(&self) -> Result<bool, Error>;
    fn set_enhanced_user_interface(&self, enabled: bool) -> Result<(), Error>;
}

impl AXUIElementExt for AXUIElement {
    fn enhanced_user_interface(&self) -> Result<bool, Error> {
        Ok(self.attribute(ENHANCED_USER_INTERFACE_ATTRIBUTE)? == Value::Bool(true))
    }

    fn set_enhanced_user_interface(&self, enabled: bool) -> Result<(), Error> {
        self.set_attribute(ENHANCED_USER_INTERFACE_ATTRIBUTE, Value::Bool(enabled))
    }
}

impl TryFrom<&AXUIElement> for WindowServerId {
    type Error = Error;
    fn try_from(element: &AXUIElement) -> Result<Self, Error> {
        match element.request(Request::WindowId(element.id))? {
            Some(Value::Number(id)) => Ok(WindowServerId(id)),
            value => wrong_type("the window id", value),
        }
    }
}

// Observers.

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObserverId(u32);

/// An observer for accessibility notifications from one app.
pub struct Observer {
    id: ObserverId,
}

pub struct ObserverBuilder<F>(PhantomData<F>);

impl Observer {
    pub fn new<F: Fn(Element, &str) + 'static>(pid: pid_t) -> Result<ObserverBuilder<F>, Error> {
        if system().bundle_id(pid).is_none() {
            return Err(Error::Ax(AXError::Failure));
        }
        Ok(ObserverBuilder(PhantomData))
    }

    pub fn add_notification(
        &self,
        elem: &AXUIElement,
        notification: &'static str,
    ) -> Result<(), Error> {
        elem.request(Request::AddNotification(self.id, elem.id, notification))
            .map(|_| ())
    }

    pub fn remove_notification(
        &self,
        elem: &AXUIElement,
        notification: &'static str,
    ) -> Result<(), Error> {
        elem.request(Request::RemoveNotification(self.id, elem.id, notification))
            .map(|_| ())
    }
}

impl<F: Fn(Element, &str) + 'static> ObserverBuilder<F> {
    pub fn install(self, callback: F) -> Observer {
        let id = ObserverId(NEXT_OBSERVER.get());
        NEXT_OBSERVER.set(id.0 + 1);
        OBSERVERS.with_borrow_mut(|observers| observers.insert(id, Rc::new(callback)));
        Observer { id }
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        OBSERVERS.with_borrow_mut(|observers| observers.remove(&self.id));
    }
}

/// Deliver a notification about `element` to an observer. Return false if
/// the observer is gone.
pub fn notify(observer: ObserverId, pid: pid_t, element: ElementId, notification: &str) -> bool {
    let Some(callback) = OBSERVERS.with_borrow(|observers| observers.get(&observer).cloned())
    else {
        return false;
    };
    callback(AXUIElement::new(pid, element), notification);
    true
}

// The rest of what the app actor uses.

pub struct RunningApp {
    pid: pid_t,
}

pub fn running_app(pid: pid_t) -> Option<RunningApp> {
    system().bundle_id(pid).map(|_| RunningApp { pid })
}

#[allow(non_snake_case)]
impl RunningApp {
    pub fn bundleIdentifier(&self) -> Option<String> {
        system().bundle_id(self.pid)
    }

    pub fn isTerminated(&self) -> bool {
        system().bundle_id(self.pid).is_none()
    }
}

pub fn is_xpc(_pid: pid_t) -> Result<bool, ()> {
    Ok(false)
}

pub fn make_key_window(pid: pid_t, wsid: WindowServerId) -> Result<(), ()> {
    system().make_key_window(pid, wsid)
}

pub fn get_mouse_state() -> MouseState {
    system().mouse_state()
}

/// The time on this thread's clock.
pub fn now() -> Instant {
    let now = CLOCK.get().unwrap_or_else(Instant::now);
    CLOCK.set(Some(now));
    now
}

/// Block the thread for `duration`, through the installed system if there is
/// one.
pub fn sleep(duration: Duration) {
    match SYSTEM.with_borrow(|s| s.clone()) {
        Some(system) => system.sleep(duration),
        None => advance_clock(duration),
    }
}

/// Set this thread's clock.
pub fn set_clock(now: Instant) {
    CLOCK.set(Some(now));
}

/// Move this thread's clock forward by `duration`.
pub fn advance_clock(duration: Duration) {
    CLOCK.set(Some(now() + duration));
}
