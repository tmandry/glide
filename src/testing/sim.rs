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

use std::collections::{BTreeMap, VecDeque};

use objc2_core_foundation::CGRect;

pub use self::glide::Action;
use self::glide::{Actor, Glide};
use self::mac::{Mac, Notification, Sent};
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

enum Message {
    Glide(glide::Message),
    Mac(mac::Message),
}

pub struct Sim {
    mac: Mac,
    glide: Glide,
    glide_running: bool,
    schedule: Schedule,
    inboxes: BTreeMap<Process, VecDeque<Message>>,
    steps: usize,
}

/// How many steps a scenario may take before it counts as never settling.
const MAX_STEPS: usize = 10_000;

impl Sim {
    /// Make a system with one screen and the given number of desktops. Glide
    /// isn't running yet.
    pub fn new(schedule: Schedule, screen: CGRect, desktops: usize) -> Sim {
        Sim {
            mac: Mac::new(screen, desktops),
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
        self.mac.launch_app(name)
    }

    pub fn open_window(&mut self, pid: pid_t, title: &str) {
        self.mac.open_window(pid, title);
    }

    pub fn close_window(&mut self, title: &str) {
        self.mac.close_window(self.mac.window_titled(title));
    }

    pub fn open_mission_control(&mut self) {
        self.mac.open_mission_control();
    }

    pub fn close_mission_control(&mut self) {
        self.mac.close_mission_control();
    }

    pub fn switch_desktop(&mut self, number: usize) {
        self.mac.switch_desktop(number);
    }

    pub fn drag_window_to_desktop(&mut self, title: &str, number: usize) {
        self.mac.drag_window_to_desktop(self.mac.window_titled(title), number);
    }

    pub fn start_glide(&mut self) {
        assert!(!self.glide_running);
        self.glide_running = true;
        self.glide.start(&self.mac);
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
        let message = self.inboxes.get_mut(&process).unwrap().pop_front().unwrap();
        match (process, message) {
            (Process::Glide(actor), Message::Glide(m)) => {
                self.glide.deliver(actor, m, &mut self.mac)
            }
            (Process::Mac(process), Message::Mac(m)) => self.mac.deliver(process, m),
            _ => unreachable!(),
        }
        true
    }

    fn send(&mut self, process: Process, message: Message) {
        self.inboxes.entry(process).or_default().push_back(message);
    }

    /// Move everything that was sent into the inbox of the process it is for.
    fn collect(&mut self) {
        while let Some(sent) = self.mac.next_sent() {
            match sent {
                Sent::Message(process, m) => self.send(Process::Mac(process), Message::Mac(m)),
                Sent::Notification(n) => {
                    // A notification only goes to a process that had asked for
                    // it by the time it was sent.
                    let actor = match n {
                        _ if !self.glide_running => continue,
                        Notification::Ax(pid, _) if !self.glide.observes(pid) => continue,
                        Notification::Ax(pid, _) => Actor::App(pid),
                        Notification::MissionControl(_) => Actor::Dock,
                        Notification::Workspace(_) => Actor::WmController,
                    };
                    let message = Message::Glide(glide::Message::Notification(n));
                    self.send(Process::Glide(actor), message);
                }
            }
        }
        for (actor, m) in self.glide.take_sent() {
            self.send(Process::Glide(actor), Message::Glide(m));
        }
    }

    // What a test can observe.

    /// The titles of the windows Glide has tiled on a desktop, sorted.
    pub fn tiled(&self, desktop: usize) -> Vec<String> {
        let mut titles: Vec<_> = self
            .glide
            .tiled_windows(self.mac.window_server().desktop(desktop))
            .into_iter()
            .filter_map(|wid| Some(self.mac.title(wid.wsid()?)?.to_owned()))
            .collect();
        titles.sort();
        titles
    }

    /// The windows on screen as the window server has them, labelled by
    /// title.
    pub fn scene(&self) -> Scene {
        let window_server = self.mac.window_server();
        let on_screen = window_server.windows_on_screen();
        let title = |wsid| self.mac.title(wsid).unwrap().to_owned();
        let scene = on_screen.iter().fold(Scene::new(window_server.screen()), |scene, w| {
            scene.window(title(w.id), w.frame)
        });
        scene.front_to_back(on_screen.iter().map(|w| title(w.id)))
    }

    /// The desktop being shown, counting from 1.
    pub fn current_desktop(&self) -> usize {
        self.mac.window_server().current_desktop_number()
    }

    /// What Glide did to windows while Mission Control was open.
    pub fn actions_during_mission_control(&self) -> Vec<Action> {
        self.glide
            .actions()
            .iter()
            .filter(|(_, mc)| *mc)
            .map(|(a, _)| a.clone())
            .collect()
    }

    /// Forget what Glide has done so far.
    pub fn clear_actions(&mut self) {
        self.glide.clear_actions();
    }

    pub fn actions(&self) -> Vec<Action> {
        self.glide.actions().iter().map(|(a, _)| a.clone()).collect()
    }
}
