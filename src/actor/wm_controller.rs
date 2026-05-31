// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The WM Controller handles hotkey registration, app launching, and command
//! dispatch. Space/screen enablement state lives in SpaceManager.

use std::path::PathBuf;
use std::sync::Arc;

use accessibility_sys::pid_t;
use objc2_app_kit::NSScreen;
use objc2_foundation::MainThreadMarker;
use serde::{Deserialize, Serialize};
use tokio::join;
use tokio::sync::mpsc;
use tracing::{Span, debug, error, info_span, instrument, warn};

pub type Sender = mpsc::UnboundedSender<(Span, WmEvent)>;
type WeakSender = mpsc::WeakUnboundedSender<(Span, WmEvent)>;
type Receiver = mpsc::UnboundedReceiver<(Span, WmEvent)>;

pub type StartupToken = mpsc::UnboundedSender<()>;
type StartupReceiver = mpsc::UnboundedReceiver<()>;

use crate::actor::app::AppInfo;
use crate::actor::{self, mouse, reactor, space_manager, status, window_server};
use crate::sys;
use crate::sys::bundle::CommandOutput;
use crate::sys::event::HotkeyManager;
use crate::sys::screen::NSScreenExt;

#[derive(Debug)]
pub enum WmEvent {
    /// Sent by the NotificationCenter actor during startup.
    AppEventsRegistered,
    AppLaunch(pid_t, AppInfo),
    AppGloballyActivated(pid_t),
    AppGloballyDeactivated(pid_t),
    AppTerminated(pid_t),
    Command(WmCommand),
    ConfigUpdated(Arc<crate::config::Config>),
    /// Sent by SpaceManager to register or unregister hotkeys.
    HotkeysActive(bool),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WmCommand {
    Wm(WmCmd),
    ReactorCommand(reactor::Command),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WmCmd {
    ToggleGlobalEnabled,
    ToggleSpaceActivated,
    Exec(ExecCmd),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum ExecCmd {
    String(String),
    Array(Vec<String>),
    Options(ExecCmdOptions),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecCmdOptions {
    pub cmd: ExecCmdArgs,
    #[serde(default)]
    pub unsafe_privileged: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum ExecCmdArgs {
    String(String),
    Array(Vec<String>),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct NormalizedExecCmd {
    pub(crate) cmd_args: Vec<String>,
    pub(crate) unsafe_privileged: bool,
}

pub struct Config {
    /// Only enables the WM on the starting space. On all other spaces, hotkeys are disabled.
    ///
    /// This can be useful for development.
    pub one_space: bool,
    pub restore_file: PathBuf,
    pub config: Arc<crate::config::Config>,
}

pub struct WmController {
    config: Config,
    sm_tx: space_manager::Sender,
    mouse_tx: mouse::Sender,
    status_tx: status::Sender,
    ws_tx: window_server::Sender,
    receiver: self::Receiver,
    sender: self::WeakSender,
    startup_channel_tx: Option<self::StartupToken>,
    login_window_pid: Option<pid_t>,
    hotkeys: Option<HotkeyManager>,
    mtm: MainThreadMarker,
}

impl WmController {
    pub fn new(
        config: Config,
        sm_tx: space_manager::Sender,
        mouse_tx: mouse::Sender,
        status_tx: status::Sender,
        ws_tx: window_server::Sender,
    ) -> (Self, Sender) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let this = Self {
            config,
            sm_tx,
            mouse_tx,
            status_tx,
            ws_tx,
            receiver,
            sender: sender.downgrade(),
            startup_channel_tx: None,
            login_window_pid: None,
            hotkeys: None,
            mtm: MainThreadMarker::new().unwrap(),
        };
        (this, sender)
    }

    pub async fn run(mut self) {
        let (startup_channel_tx, startup_channel_rx) = mpsc::unbounded_channel();
        self.startup_channel_tx.replace(startup_channel_tx);
        let sm_tx = self.sm_tx.clone();
        join!(self.watch_events(), Self::startup(startup_channel_rx, sm_tx));
    }

    async fn watch_events(&mut self) {
        while let Some((span, event)) = self.receiver.recv().await {
            let _guard = span.enter();
            self.handle_event(event);
        }
    }

    async fn startup(mut receiver: StartupReceiver, sm_tx: space_manager::Sender) {
        let _span = info_span!("Startup");
        while let Some(()) = receiver.recv().await {}
        debug!("Startup channel closed; sending startup event");
        sm_tx.send(space_manager::Event::ReactorEvent(
            reactor::Event::StartupComplete,
        ));
    }

    #[instrument(skip(self))]
    pub fn handle_event(&mut self, event: WmEvent) {
        debug!("handle_event");
        use self::WmCmd::*;
        use self::WmCommand::*;
        use self::WmEvent::*;
        match event {
            AppEventsRegistered => {
                let startup_tx = self
                    .startup_channel_tx
                    .take()
                    .expect("AppEventsRegistered should only be sent once");
                for (pid, info) in sys::app::running_apps(None) {
                    self.new_app(pid, info, Some(startup_tx.clone()));
                }
            }
            AppLaunch(pid, info) => {
                self.new_app(pid, info, None);
            }
            AppGloballyActivated(pid) => {
                // Make sure the mouse cursor stays hidden after app switch.
                self.mouse_tx.send(mouse::Request::EnforceHidden);
                if let Some(screen_id) = self.get_focused_screen() {
                    self.sm_tx.send(space_manager::Event::FocusedScreenChanged(screen_id));
                }
                if self.login_window_pid == Some(pid) {
                    self.sm_tx.send(space_manager::Event::LoginWindowActive(true));
                }
                self.send_reactor_event(reactor::Event::ApplicationGloballyActivated(pid));
            }
            AppGloballyDeactivated(pid) => {
                if self.login_window_pid == Some(pid) {
                    self.sm_tx.send(space_manager::Event::LoginWindowActive(false));
                }
                self.send_reactor_event(reactor::Event::ApplicationGloballyDeactivated(pid));
            }
            AppTerminated(pid) => {
                self.send_reactor_event(reactor::Event::ApplicationTerminated(pid));
            }
            Command(Wm(ToggleSpaceActivated)) => {
                let Some(screen_id) = self.get_focused_screen() else {
                    return;
                };
                self.sm_tx.send(space_manager::Event::ToggleSpace(screen_id));
            }
            Command(Wm(ToggleGlobalEnabled)) => {
                self.sm_tx.send(space_manager::Event::ToggleGlobalEnabled);
            }
            Command(Wm(Exec(cmd))) => {
                self.exec_cmd(cmd);
            }
            Command(ReactorCommand(cmd)) => {
                self.sm_tx.send(space_manager::Event::ReactorCommand(cmd));
            }
            ConfigUpdated(config) => {
                self.mouse_tx.send(mouse::Request::ConfigUpdated(config.clone()));
                self.status_tx.send(status::Event::ConfigUpdated(config.clone()));
                self.sm_tx.send(space_manager::Event::ConfigUpdated(config.clone()));
                self.config.config = config;
                self.unregister_hotkeys();
                // Hotkeys will be re-registered when SpaceManager sends
                // HotkeysActive after processing the ConfigUpdated.
            }
            HotkeysActive(active) => {
                if active {
                    if self.hotkeys.is_none() {
                        self.register_hotkeys();
                    }
                } else {
                    self.unregister_hotkeys();
                }
            }
        }
    }

    fn new_app(&mut self, pid: pid_t, info: AppInfo, startup: Option<StartupToken>) {
        if info.bundle_id.as_deref() == Some("com.apple.loginwindow") {
            if let Some(prev) = self.login_window_pid {
                warn!("Multiple loginwindow instances found: {prev:?} and {pid:?}");
            }
            self.login_window_pid = Some(pid);
        }
        actor::app::spawn_app_thread(pid, info, self.ws_tx.clone(), startup.clone());
    }

    fn get_focused_screen(&self) -> Option<crate::sys::screen::ScreenId> {
        let screen = NSScreen::mainScreen(self.mtm)?;
        screen.get_number().ok()
    }

    fn send_reactor_event(&self, event: reactor::Event) {
        self.sm_tx.send(space_manager::Event::ReactorEvent(event));
    }

    fn register_hotkeys(&mut self) {
        debug!("register_hotkeys");
        self.hotkeys.take();
        let mgr = match HotkeyManager::new(self.sender.upgrade().unwrap()) {
            Ok(mgr) => mgr,
            Err(e) => {
                warn!("Failed to register hotkeys: {e:?}");
                return;
            }
        };
        for (key, cmd) in &self.config.config.keys {
            mgr.register_wm(key.modifiers, key.key_code, cmd.clone());
        }
        self.hotkeys = Some(mgr);
    }

    fn unregister_hotkeys(&mut self) {
        debug!("unregister_hotkeys");
        self.hotkeys = None;
    }

    fn exec_cmd(&self, cmd_args: ExecCmd) {
        // Spawn so we don't block the main thread.
        std::thread::spawn(move || {
            let cmd = cmd_args.normalize();
            let [program, args @ ..] = &*cmd.cmd_args else {
                error!("Empty argument list passed to exec");
                return;
            };

            let (mode, run_result) = if cmd.unsafe_privileged {
                ("directly", run_unsafe_privileged_exec(&cmd.cmd_args))
            } else {
                (
                    "via LaunchServices",
                    sys::bundle::launch_cli_with_open(&cmd.cmd_args),
                )
            };

            match run_result {
                Ok(output) if output.status == 0 => {}
                Ok(CommandOutput { status, output }) => {
                    error!("Exec command exited with status {status}: {program:?} {args:?}");
                    error!("output:\n{output}");
                }
                Err(e) => {
                    error!("Failed to execute command {mode} {program:?} {args:?}: {e:?}");
                }
            }
        });
    }
}

fn run_unsafe_privileged_exec(args: &[String]) -> anyhow::Result<sys::bundle::CommandOutput> {
    sys::bundle::launch_cli_privileged(args)
}

impl ExecCmd {
    pub(crate) fn normalize(self) -> NormalizedExecCmd {
        match self {
            ExecCmd::Array(vec) => NormalizedExecCmd {
                cmd_args: vec,
                unsafe_privileged: false,
            },
            ExecCmd::String(s) => NormalizedExecCmd {
                cmd_args: split_cmd_string(&s),
                unsafe_privileged: false,
            },
            ExecCmd::Options(opts) => NormalizedExecCmd {
                cmd_args: opts.cmd.into_array(),
                unsafe_privileged: opts.unsafe_privileged,
            },
        }
    }
}

impl ExecCmdArgs {
    fn into_array(self) -> Vec<String> {
        match self {
            ExecCmdArgs::Array(vec) => vec,
            ExecCmdArgs::String(s) => split_cmd_string(&s),
        }
    }
}

fn split_cmd_string(s: &str) -> Vec<String> {
    s.split_whitespace().map(ToOwned::to_owned).collect()
}
