// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Glide's app actors, each on a thread of its own as in Glide.
//!
//! Only one thread runs at a time. The simulator hands control to an app
//! actor's thread and waits until the actor's task is pending or the actor
//! makes a system call. The thread forwards every call to the simulator, which
//! owns the simulated macOS and the clock. While the actor waits for the reply
//! to a call that blocks, such as an accessibility request, other processes
//! run.

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::pin::pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::task::{Context, Wake, Waker};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use accessibility::AXError;

use crate::actor::app::{self, RaiseLock};
use crate::actor::{window_server, wm_controller};
use crate::sys::app::{AppInfo, pid_t};
use crate::sys::ax::fake::{self, ElementId, ObserverId, Request, Value};
use crate::sys::event::MouseState;
use crate::sys::window_server::WindowServerId;

/// Records that a task was woken.
pub struct Woken(AtomicBool);

impl Woken {
    pub fn new() -> Arc<Woken> {
        Arc::new(Woken(AtomicBool::new(true)))
    }

    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    fn take(&self) -> bool {
        self.0.swap(false, Ordering::Relaxed)
    }
}

impl Wake for Woken {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// A system call from an app actor.
#[derive(Debug)]
pub enum Call {
    Ax(pid_t, Request),
    MakeKeyWindow(WindowServerId),
    BundleId(pid_t),
    Sleep(Duration),
}

/// The simulator's reply to a [`Call`].
pub enum Reply {
    Ax(Result<Option<Value>, AXError>),
    MakeKeyWindow(Result<(), ()>),
    BundleId(Option<String>),
    Slept,
}

/// A notification for one of an app actor's observers.
pub type AxNotification = (ObserverId, pid_t, ElementId, &'static str);

/// What an app actor's thread did with control.
#[derive(Debug)]
pub enum Event {
    /// It made a call and is waiting for the reply.
    Call(Call),
    /// Its task is waiting to be woken, or finished if `done`.
    Yielded { done: bool },
}

enum ToActor {
    /// Deliver a notification if there is one, then poll the task if it was
    /// woken.
    Run {
        now: Instant,
        notification: Option<AxNotification>,
    },
    Reply {
        now: Instant,
        reply: Reply,
    },
}

enum FromActor {
    Event(Event),
    Panicked(Box<dyn Any + Send>),
}

/// The simulator's end of an app actor's thread.
pub struct AppThread {
    to: Option<Sender<ToActor>>,
    from: Receiver<FromActor>,
    handle: Option<JoinHandle<()>>,
}

impl AppThread {
    /// Start an app actor's thread. It does nothing until it is first run.
    pub fn spawn(
        pid: pid_t,
        info: AppInfo,
        ws_tx: window_server::Sender,
        startup: Option<wm_controller::StartupToken>,
        raise_lock: RaiseLock,
        woken: Arc<Woken>,
    ) -> AppThread {
        let (to, to_rx) = channel();
        let (from_tx, from) = channel();
        let handle = thread::Builder::new()
            .name(format!("app {pid}"))
            .spawn(move || {
                let link = Rc::new(Link {
                    to_sim: from_tx.clone(),
                    from_sim: to_rx,
                });
                let run = AssertUnwindSafe(|| {
                    actor_main(pid, info, ws_tx, startup, raise_lock, woken, link)
                });
                if let Err(panic) = catch_unwind(run)
                    && !panic.is::<Shutdown>()
                {
                    _ = from_tx.send(FromActor::Panicked(panic));
                }
            })
            .unwrap();
        AppThread {
            to: Some(to),
            from,
            handle: Some(handle),
        }
    }

    /// Let the actor handle a notification, if there is one, and then poll its
    /// task if it was woken. Return once it yields or makes a call.
    pub fn run(&self, notification: Option<AxNotification>) -> Event {
        self.send(ToActor::Run { now: fake::now(), notification })
    }

    /// Give the actor the reply to the call it is waiting on. Return once it
    /// yields or makes another call.
    pub fn reply(&self, reply: Reply) -> Event {
        self.send(ToActor::Reply { now: fake::now(), reply })
    }

    fn send(&self, message: ToActor) -> Event {
        let sent = self.to.as_ref().unwrap().send(message);
        match (sent, self.from.recv()) {
            (_, Ok(FromActor::Event(event))) => event,
            (_, Ok(FromActor::Panicked(panic))) => resume_unwind(panic),
            (_, Err(_)) => panic!("an app actor's thread exited"),
        }
    }
}

impl Drop for AppThread {
    fn drop(&mut self) {
        // The thread unwinds out of whatever it is waiting on and exits.
        drop(self.to.take());
        if let Some(handle) = self.handle.take() {
            _ = handle.join();
        }
    }
}

/// Unwinds an app actor's thread once the simulator is gone.
struct Shutdown;

fn shutdown() -> ! {
    resume_unwind(Box::new(Shutdown))
}

fn actor_main(
    pid: pid_t,
    info: AppInfo,
    ws_tx: window_server::Sender,
    startup: Option<wm_controller::StartupToken>,
    raise_lock: RaiseLock,
    woken: Arc<Woken>,
    link: Rc<Link>,
) {
    let _system = fake::install(Rc::clone(&link) as Rc<dyn fake::System>);
    let ToActor::Run { now, notification: None } = link.recv() else {
        panic!("an app actor's thread must be run first");
    };
    fake::set_clock(now);
    let Some(task) = app::app_task(pid, info, ws_tx, startup, raise_lock) else {
        link.send(Event::Yielded { done: true });
        return;
    };
    let mut task = pin!(task);
    let waker = Waker::from(Arc::clone(&woken));
    loop {
        if woken.take() && task.as_mut().poll(&mut Context::from_waker(&waker)).is_ready() {
            link.send(Event::Yielded { done: true });
            return;
        }
        link.send(Event::Yielded { done: false });
        match link.recv() {
            ToActor::Run { now, notification } => {
                fake::set_clock(now);
                if let Some((observer, pid, element, name)) = notification {
                    fake::notify(observer, pid, element, name);
                }
            }
            ToActor::Reply { .. } => panic!("a reply came for no call"),
        }
    }
}

/// The app actor's end of its thread. It is the fake system on that thread.
struct Link {
    to_sim: Sender<FromActor>,
    from_sim: Receiver<ToActor>,
}

impl Link {
    fn send(&self, event: Event) {
        if self.to_sim.send(FromActor::Event(event)).is_err() {
            shutdown();
        }
    }

    fn recv(&self) -> ToActor {
        self.from_sim.recv().unwrap_or_else(|_| shutdown())
    }

    fn call(&self, call: Call) -> Reply {
        self.send(Event::Call(call));
        match self.recv() {
            ToActor::Reply { now, reply } => {
                fake::set_clock(now);
                reply
            }
            ToActor::Run { .. } => panic!("an app actor was run while it waited on a call"),
        }
    }
}

impl fake::System for Link {
    fn ax(&self, pid: pid_t, request: Request) -> Result<Option<Value>, AXError> {
        match self.call(Call::Ax(pid, request)) {
            Reply::Ax(reply) => reply,
            _ => unreachable!(),
        }
    }

    fn make_key_window(&self, _pid: pid_t, wsid: WindowServerId) -> Result<(), ()> {
        match self.call(Call::MakeKeyWindow(wsid)) {
            Reply::MakeKeyWindow(reply) => reply,
            _ => unreachable!(),
        }
    }

    fn bundle_id(&self, pid: pid_t) -> Option<String> {
        match self.call(Call::BundleId(pid)) {
            Reply::BundleId(reply) => reply,
            _ => unreachable!(),
        }
    }

    fn mouse_state(&self) -> MouseState {
        MouseState::Up
    }

    fn sleep(&self, duration: Duration) {
        match self.call(Call::Sleep(duration)) {
            Reply::Slept => {}
            _ => unreachable!(),
        }
    }
}
