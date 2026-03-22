// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Watches Dock notifications for expose (mission control) events.

use std::cell::RefCell;
use std::future::pending;
use std::rc::Rc;

use accessibility::AXUIElement;
use anyhow::{Context, bail};
use objc2_app_kit::NSRunningApplication;
use objc2_foundation::ns_string;
use tracing::error;

use crate::actor::space_manager;
use crate::sys::app::NSRunningApplicationExt;
use crate::sys::observer::Observer;

pub struct Dock {
    #[expect(dead_code)]
    observer: Option<Observer>,
}

struct State {
    sm_tx: space_manager::Sender,
}

#[expect(non_upper_case_globals)]
mod consts {
    pub(super) const kAXExposeExit: &str = "AXExposeExit";
    pub(super) const kAXExposeShowAllWindows: &str = "AXExposeShowAllWindows";
    pub(super) const kAXExposeShowFrontWindows: &str = "AXExposeShowFrontWindows";
    pub(super) const kAXExposeShowDesktop: &str = "AXExposeShowDesktop";
}
use consts::*;

const NOTIFICATIONS: &[&str] = &[
    kAXExposeExit,
    kAXExposeShowAllWindows,
    kAXExposeShowFrontWindows,
    kAXExposeShowDesktop,
];

impl Dock {
    pub fn new(sm_tx: space_manager::Sender) -> Self {
        let observer = match Self::init(sm_tx) {
            Ok(observer) => Some(observer),
            Err(e) => {
                tracing::warn!("Failed to start dock actor: {e}");
                None
            }
        };
        Self { observer }
    }

    fn init(sm_tx: space_manager::Sender) -> anyhow::Result<Observer> {
        let apps = NSRunningApplication::runningApplicationsWithBundleIdentifier(ns_string!(
            "com.apple.dock"
        ))
        .to_vec();
        let [app] = apps.as_slice() else {
            bail!(
                "Expected one running Dock instance but found {}: {apps:?}",
                apps.len()
            );
        };
        let pid = app.pid();

        let state = Rc::new(RefCell::new(State { sm_tx }));
        let observer = Observer::new(pid)
            .context("Creating observer for Dock")?
            .install(move |_elem, notif| state.borrow_mut().handle_notification(notif));
        let elem = AXUIElement::application(pid);
        for notif in NOTIFICATIONS {
            observer
                .add_notification(&elem, notif)
                .with_context(|| format!("Addding {notif} notification to Dock observer"))?;
        }

        Ok(observer)
    }

    pub async fn run(self) {
        pending().await
    }
}

impl State {
    #[tracing::instrument(skip(self))]
    fn handle_notification(&mut self, notif: &str) {
        #[expect(non_upper_case_globals)]
        match notif {
            kAXExposeShowAllWindows | kAXExposeShowFrontWindows | kAXExposeShowDesktop => {
                self.sm_tx.send(space_manager::Event::ExposeActive(true));
            }
            kAXExposeExit => {
                self.sm_tx.send(space_manager::Event::ExposeActive(false));
            }
            _ => {
                error!("Unhandled notification {notif:?} from Dock");
            }
        }
    }
}
