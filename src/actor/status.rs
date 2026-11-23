// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Manages the status bar icon.

use std::sync::Arc;

use objc2::MainThreadMarker;
use objc2_app_kit::NSRunningApplication;
use tracing::{info, instrument, warn};

use crate::actor::updater::{self, UpdateStatus};
use crate::actor::wm_controller;
use crate::config::Config;
use crate::sys::app::NSRunningApplicationExt;
use crate::sys::screen::{SpaceId, get_active_space_number};
use crate::sys::session::{SecureInput, on_secure_input_change, secure_input};
use crate::ui::status_bar::StatusIcon;
use crate::{actor, trace_call};

#[derive(Debug)]
pub enum Event {
    // Note: These should not be filtered for active (they should all be Some)
    // so we can always show the user the current space id.
    SpaceChanged(Vec<Option<SpaceId>>),
    FocusedScreenChanged,
    GlobalEnabledChanged(bool),
    SpaceEnabledChanged(bool),
    ConfigUpdated(Arc<Config>),
    UpdateStatusChanged(Option<UpdateStatus>),
}

pub struct Status {
    config: Arc<Config>,
    rx: Receiver,
    icon: Option<StatusIcon>,
    mtm: MainThreadMarker,
    wm_tx: wm_controller::Sender,
    updater_tx: updater::Sender,
    update_status: Option<UpdateStatus>,
    secure_input: Option<SecureInput>,
}

pub type Sender = actor::Sender<Event>;
pub type Receiver = actor::Receiver<Event>;

impl Status {
    pub fn new(
        config: Arc<Config>,
        rx: Receiver,
        mtm: MainThreadMarker,
        wm_tx: wm_controller::Sender,
        updater_tx: updater::Sender,
    ) -> Self {
        let mut this = Self {
            icon: None,
            config,
            rx,
            mtm,
            wm_tx,
            updater_tx,
            update_status: None,
            secure_input: None,
        };
        this.apply_config();
        this.update_toggle_title(true);
        this
    }

    fn apply_config(&mut self) {
        let icon = self.icon.take();
        if self.config.settings.status_icon.enable {
            self.icon = icon.or_else(|| {
                Some(StatusIcon::new(
                    &self.config.settings.experimental.status_icon,
                    self.mtm,
                    self.wm_tx.clone(),
                    self.updater_tx.clone(),
                ))
            });
        }
        self.update_space();
        self.show_update_status();
        self.show_secure_input();
    }

    pub async fn run(mut self) {
        if self.icon.is_none() {
            return;
        }
        let (secure_input_tx, mut secure_input_rx) = tokio::sync::mpsc::unbounded_channel();
        let _secure_input_notifiers = on_secure_input_change(move || _ = secure_input_tx.send(()))
            .inspect_err(|err| warn!("Can't watch for Secure Input changes: {err:?}"));
        self.check_secure_input();
        loop {
            tokio::select! {
                event = self.rx.recv() => {
                    let Some((span, event)) = event else { break };
                    let _guard = span.enter();
                    self.handle_event(event);
                }
                Some(()) = secure_input_rx.recv() => self.check_secure_input(),
            }
        }
    }

    fn check_secure_input(&mut self) {
        let state = secure_input();
        if state == self.secure_input {
            return;
        }
        match state {
            Some(SecureInput { likely_holder }) => {
                info!(?likely_holder, "Secure Input is on; hotkeys are blocked")
            }
            None => info!("Secure Input is off"),
        }
        self.secure_input = state;
        self.show_secure_input();
    }

    fn show_secure_input(&mut self) {
        let Some(icon) = &mut self.icon else { return };
        let blocked = self.secure_input.filter(|_| !self.config.keys.is_empty());
        let details = blocked.map(|SecureInput { likely_holder }| {
            let app = likely_holder
                .and_then(NSRunningApplication::with_process_id)
                .and_then(|app| app.localized_name())
                .map(|name| name.to_string());
            secure_input_details(app.as_deref())
        });
        icon.set_secure_input_warning(details.as_deref());
    }

    #[instrument(skip(self))]
    fn handle_event(&mut self, event: Event) {
        match event {
            Event::SpaceChanged(_) | Event::FocusedScreenChanged => self.update_space(),
            Event::GlobalEnabledChanged(enabled) => self.update_toggle_title(enabled),
            Event::SpaceEnabledChanged(enabled) => self.update_space_toggle_title(enabled),
            Event::ConfigUpdated(config) => {
                if self.config.settings.experimental.status_icon
                    != config.settings.experimental.status_icon
                {
                    // Remove icon so it gets recreated.
                    self.icon.take();
                }
                self.config = config;
                self.apply_config();
            }
            Event::UpdateStatusChanged(status) => {
                self.update_status = status;
                self.show_update_status();
            }
        }
    }

    fn show_update_status(&mut self) {
        let Some(icon) = &mut self.icon else { return };
        match &self.update_status {
            None => icon.set_update_item(None, false),
            Some(UpdateStatus::Available(v)) => {
                icon.set_update_item(Some(&format!("Update to Glide v{v}…")), true)
            }
            Some(UpdateStatus::Installing(v)) => {
                icon.set_update_item(Some(&format!("Installing Glide v{v}…")), false)
            }
            Some(UpdateStatus::Failed(v)) => icon
                .set_update_item(Some(&format!("Update to Glide v{v} failed; try again…")), true),
        }
    }

    fn update_space(&mut self) {
        let Some(icon) = &mut self.icon else { return };
        if self.config.settings.experimental.status_icon.space_index {
            // TODO: Move this off the main thread.
            let label = trace_call!(get_active_space_number())
                .map(|n| n.to_string())
                .unwrap_or_default();
            icon.set_text(&label);
        } else {
            icon.set_text("");
        }
    }

    fn update_toggle_title(&mut self, enabled: bool) {
        let Some(icon) = &mut self.icon else { return };
        icon.set_toggle_title(if enabled {
            "Pause Glide"
        } else {
            "Resume Glide"
        });
        icon.set_space_toggle_enabled(enabled);
    }

    fn update_space_toggle_title(&mut self, enabled: bool) {
        let Some(icon) = &mut self.icon else { return };
        icon.set_space_toggle_title(if enabled {
            "Stop Managing Space"
        } else {
            "Start Managing Space"
        });
    }
}

fn secure_input_details(app: Option<&str>) -> String {
    let what = "Secure Input is on, so macOS is keeping your keyboard shortcuts from \
        Glide. Apps turn it on while you type into a password field, and it \
        normally turns off again when you leave the field.";
    let fix = match app {
        Some(app) => format!(
            "Probably turned on by:\n{app}\n\nIf it stays on, switch to that app and \
            click into and back out of a password field, or quit it."
        ),
        None => "If it stays on, find the app with a password field open and click out \
            of the field, or quit that app."
            .to_owned(),
    };
    format!("{what}\n\n{fix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secure_input_details_name_the_likely_app() {
        let details = secure_input_details(Some("Google Chrome"));
        assert!(
            details.contains("Probably turned on by:\nGoogle Chrome\n"),
            "{details}"
        );
        let details = secure_input_details(None);
        assert!(!details.contains("Probably"), "{details}");
    }
}
