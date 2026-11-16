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
//! [`mac`] models the macOS processes and knows nothing about Glide. [`glide`]
//! runs Glide's actors against it. This module only moves messages between
//! them.

mod glide;
mod mac;

#[cfg(test)]
mod tests;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use objc2_core_foundation::CGRect;

use self::glide::{Actor, Glide};
pub use self::mac::Action;
use self::mac::{Mac, Notification};
use crate::actor::layout::LayoutCommand;
use crate::actor::reactor;
use crate::sys::app::pid_t;
use crate::testing::scene::Scene;

/// Something with an inbox and its own event loop.
///
/// The order here is the order [`Schedule::GlideFirst`] serves them in.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Process {
    Glide(Actor),
    Mac(mac::Process),
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

impl Schedule {
    /// Run `scenario` under both fixed schedules and 16 seeded ones, on
    /// several threads. Panic with the first schedule that failed.
    ///
    /// For a deeper sweep, set `GLIDE_SIM_SEED_FACTOR` to run that many times
    /// as many seeds, and `GLIDE_SIM_FIRST_SEED` to start from a seed other
    /// than 1.
    pub fn each(scenario: impl Fn(Schedule) + Sync) {
        Schedule::sweep(16, scenario);
    }

    fn sweep(seeds: u64, scenario: impl Fn(Schedule) + Sync) {
        let first = env_u64("GLIDE_SIM_FIRST_SEED").unwrap_or(1);
        // A zero state never changes under xorshift.
        assert!(first > 0, "GLIDE_SIM_FIRST_SEED must be at least 1");
        let seeds = seeds * env_u64("GLIDE_SIM_SEED_FACTOR").unwrap_or(1);
        let schedules: Vec<Schedule> = [Schedule::GlideFirst, Schedule::MacFirst]
            .into_iter()
            .chain((first..first + seeds).map(Schedule::Seeded))
            .collect();
        let next = AtomicUsize::new(0);
        let failed = AtomicUsize::new(usize::MAX);
        let threads = thread::available_parallelism().map_or(1, |n| n.get());
        let panics: Vec<_> = thread::scope(|scope| {
            let workers: Vec<_> = (0..threads)
                .map(|_| {
                    scope.spawn(|| {
                        loop {
                            let index = next.fetch_add(1, Ordering::Relaxed);
                            if index >= schedules.len() || index > failed.load(Ordering::Relaxed) {
                                return None;
                            }
                            let run = AssertUnwindSafe(|| scenario(schedules[index]));
                            if let Err(panic) = catch_unwind(run) {
                                failed.fetch_min(index, Ordering::Relaxed);
                                return Some((index, panic));
                            }
                        }
                    })
                })
                .collect();
            workers.into_iter().filter_map(|worker| worker.join().unwrap()).collect()
        });
        if let Some((index, panic)) = panics.into_iter().min_by_key(|&(index, _)| index) {
            eprintln!("failed under {:?}", schedules[index]);
            resume_unwind(panic);
        }
    }
}

fn env_u64(name: &str) -> Option<u64> {
    let value = std::env::var(name).ok()?;
    Some(
        value
            .parse()
            .unwrap_or_else(|_| panic!("{name} must be a number, not {value:?}")),
    )
}

pub struct Sim {
    mac: Rc<RefCell<Mac>>,
    glide: Glide,
    glide_running: bool,
    schedule: Schedule,
    /// Messages waiting for Glide's actors. The macOS processes keep their
    /// own.
    inboxes: BTreeMap<Actor, VecDeque<glide::Message>>,
    steps: usize,
}

/// How many steps a scenario may take before it counts as never settling.
const MAX_STEPS: usize = 10_000;

impl Sim {
    /// Make a system with one screen and the given number of desktops. Glide
    /// isn't running yet.
    pub fn new(schedule: Schedule, screen: CGRect, desktops: usize) -> Sim {
        Sim {
            mac: Rc::new(RefCell::new(Mac::new(screen, desktops))),
            glide: Glide::new(),
            glide_running: false,
            schedule,
            inboxes: BTreeMap::new(),
            steps: 0,
        }
    }

    // What the user does. One process handles each of these right away; call
    // `run` to let the rest of the system react.

    pub fn launch_app(&mut self, name: &str) -> pid_t {
        self.mac.borrow_mut().launch_app(name)
    }

    pub fn open_window(&mut self, pid: pid_t, title: &str) {
        self.mac.borrow_mut().open_window(pid, title);
    }

    pub fn close_window(&mut self, title: &str) {
        let mut mac = self.mac.borrow_mut();
        let wsid = mac.window_titled(title);
        mac.close_window(wsid);
    }

    pub fn click_window(&mut self, title: &str) {
        let mut mac = self.mac.borrow_mut();
        let wsid = mac.window_titled(title);
        mac.click_window(wsid);
    }

    /// Move or resize a window with the mouse.
    pub fn set_window_frame(&mut self, title: &str, frame: CGRect) {
        let mut mac = self.mac.borrow_mut();
        let wsid = mac.window_titled(title);
        mac.set_window_frame(wsid, frame);
    }

    /// Give Glide a layout command, as a key binding would.
    pub fn command(&mut self, command: LayoutCommand) {
        assert!(self.glide_running);
        let event = reactor::Event::Command(reactor::Command::Layout(command));
        self.send(Actor::Reactor, glide::Message::Reactor(event));
    }

    pub fn open_mission_control(&mut self) {
        self.mac.borrow_mut().open_mission_control();
    }

    pub fn close_mission_control(&mut self) {
        self.mac.borrow_mut().close_mission_control();
    }

    pub fn switch_desktop(&mut self, number: usize) {
        self.mac.borrow_mut().switch_desktop(number);
    }

    pub fn drag_window_to_desktop(&mut self, title: &str, number: usize) {
        let mut mac = self.mac.borrow_mut();
        let wsid = mac.window_titled(title);
        mac.drag_window_to_desktop(wsid, number);
    }

    pub fn start_glide(&mut self) {
        assert!(!self.glide_running);
        self.glide_running = true;
        self.glide.start(&self.mac.borrow());
    }

    // Running the system.

    /// Deliver messages until every inbox is empty.
    pub fn run(&mut self) {
        while self.step() {}
    }

    /// Let one process take a step. Return false if none has anything to do.
    pub fn step(&mut self) -> bool {
        self.collect();
        let mut ready: BTreeSet<Process> = BTreeSet::new();
        ready.extend(
            self.inboxes
                .iter()
                .filter(|(_, q)| !q.is_empty())
                .map(|(&a, _)| Process::Glide(a)),
        );
        ready.extend(self.glide.woken_apps().map(Process::Glide));
        ready.extend(self.mac.borrow().ready().map(Process::Mac));
        let ready: Vec<Process> = ready.into_iter().collect();
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
        match process {
            Process::Glide(actor) => {
                let message = self.inboxes.get_mut(&actor).and_then(|inbox| inbox.pop_front());
                self.glide.deliver(actor, message, &self.mac);
            }
            Process::Mac(process) => {
                self.mac.borrow_mut().step(process);
            }
        }
        true
    }

    fn send(&mut self, actor: Actor, message: glide::Message) {
        self.inboxes.entry(actor).or_default().push_back(message);
    }

    /// Move what was sent to Glide's actors into their inboxes.
    fn collect(&mut self) {
        while let Some(n) = self.mac.borrow_mut().next_notification() {
            let actor = match n {
                // Nothing observes macOS until Glide starts.
                _ if !self.glide_running => continue,
                Notification::Ax { pid, .. } => Actor::App(pid),
                Notification::MissionControl(_) => Actor::Dock,
                Notification::Workspace(_) => Actor::WmController,
            };
            let message = glide::Message::Notification(n);
            self.inboxes.entry(actor).or_default().push_back(message);
        }
        for (actor, m) in self.glide.take_sent() {
            self.send(actor, m);
        }
    }

    // What a test can observe.

    /// The titles of the windows Glide has tiled on a desktop, sorted.
    pub fn tiled(&self, desktop: usize) -> Vec<String> {
        let mac = self.mac.borrow();
        let mut titles: Vec<_> = self
            .glide
            .tiled_windows(mac.window_server().desktop(desktop))
            .into_iter()
            .filter_map(|wid| Some(mac.title(wid.wsid()?)?.to_owned()))
            .collect();
        titles.sort();
        titles
    }

    /// The windows on screen as the window server has them, labelled by
    /// title.
    pub fn scene(&self) -> Scene {
        let mac = self.mac.borrow();
        let window_server = mac.window_server();
        let on_screen = window_server.windows_on_screen();
        let title = |wsid| mac.title(wsid).unwrap().to_owned();
        let scene = on_screen.iter().fold(Scene::new(window_server.screen()), |scene, w| {
            scene.window(title(w.id), w.frame)
        });
        scene.front_to_back(on_screen.iter().map(|w| title(w.id)))
    }

    /// The title of the window Glide has selected on the desktop being
    /// shown.
    pub fn selected(&mut self) -> Option<String> {
        let mac = self.mac.borrow();
        let space = mac.window_server().current_desktop();
        // Glide has no layout to select in until it has tiled a window there.
        if self.glide.tiled_windows(space).is_empty() {
            return None;
        }
        let wid = self.glide.selected_window(space)?;
        Some(mac.title(wid.wsid()?)?.to_owned())
    }

    /// The title of the window with the keyboard focus.
    pub fn key_window(&self) -> Option<String> {
        self.mac.borrow().key_window().map(str::to_owned)
    }

    /// The desktop being shown, counting from 1.
    pub fn current_desktop(&self) -> usize {
        self.mac.borrow().window_server().current_desktop_number()
    }

    /// What Glide did to windows while Mission Control was open.
    pub fn actions_during_mission_control(&self) -> Vec<Action> {
        self.mac
            .borrow()
            .actions()
            .iter()
            .filter(|(_, mc)| *mc)
            .map(|(a, _)| a.clone())
            .collect()
    }

    /// Forget what Glide has done so far.
    pub fn clear_actions(&mut self) {
        self.mac.borrow_mut().clear_actions();
    }

    pub fn actions(&self) -> Vec<Action> {
        self.mac.borrow().actions().iter().map(|(a, _)| a.clone()).collect()
    }
}
