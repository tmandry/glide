// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The WM Controller handles major events like enabling and disabling the
//! window manager on certain spaces and launching app threads. It also
//! controls hotkey registration.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;

use accessibility_sys::pid_t;
use objc2_app_kit::NSScreen;
use objc2_core_foundation::CGRect;
use objc2_foundation::MainThreadMarker;
use serde::{Deserialize, Serialize};
use tokio::join;
use tokio::sync::mpsc;
use tracing::{Span, debug, error, info, info_span, instrument, trace, warn};

pub type Sender = mpsc::UnboundedSender<(Span, WmEvent)>;
type WeakSender = mpsc::WeakUnboundedSender<(Span, WmEvent)>;
type Receiver = mpsc::UnboundedReceiver<(Span, WmEvent)>;

pub type StartupToken = mpsc::UnboundedSender<()>;
type StartupReceiver = mpsc::UnboundedReceiver<()>;

use crate::actor::app::AppInfo;
use crate::actor::{self, group_bars, mouse, reactor, status, window_server};
use crate::collections::HashSet;
use crate::sys;
use crate::sys::event::HotkeyManager;
use crate::sys::screen::{CoordinateConverter, NSScreenExt, ScreenId, SpaceId};
use crate::sys::window_server::WindowsOnScreen;

#[derive(Debug)]
pub enum WmEvent {
    /// Sent by the NotificationCenter actor during startup.
    AppEventsRegistered,
    AppLaunch(pid_t, AppInfo),
    AppGloballyActivated(pid_t),
    AppGloballyDeactivated(pid_t),
    AppTerminated(pid_t),
    SpaceChanged(Vec<Option<SpaceId>>, WindowsOnScreen),
    ScreenParametersChanged {
        screens: Vec<ScreenId>,
        frames: Vec<CGRect>,
        spaces: Vec<Option<SpaceId>>,
        scale_factors: Vec<f64>,
        converter: CoordinateConverter,
        on_screen: WindowsOnScreen,
    },
    ExposeEntered,
    ExposeExited,
    Command(WmCommand),
    ConfigUpdated(Arc<crate::config::Config>),
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ExecCmd {
    String(String),
    Array(Vec<String>),
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
    events_tx: reactor::Sender,
    mouse_tx: mouse::Sender,
    status_tx: status::Sender,
    ws_tx: window_server::Sender,
    group_indicators_tx: group_bars::Sender,
    receiver: self::Receiver,
    sender: self::WeakSender,
    startup_channel_tx: Option<self::StartupToken>,
    starting_space: Option<SpaceId>,
    cur_space: Vec<Option<SpaceId>>,
    cur_screen_id: Vec<ScreenId>,
    disabled_spaces: HashSet<SpaceId>,
    enabled_spaces: HashSet<SpaceId>,
    login_window_pid: Option<pid_t>,
    login_window_active: bool,
    expose_active: bool,
    is_globally_enabled: bool,
    hotkeys: Option<HotkeyManager>,
    mtm: MainThreadMarker,
}

impl WmController {
    pub fn new(
        config: Config,
        events_tx: reactor::Sender,
        mouse_tx: mouse::Sender,
        status_tx: status::Sender,
        ws_tx: window_server::Sender,
        group_indicators_tx: group_bars::Sender,
    ) -> (Self, Sender) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let is_globally_enabled = true;
        let this = Self {
            config,
            events_tx,
            mouse_tx,
            status_tx: status_tx.clone(),
            ws_tx,
            group_indicators_tx,
            receiver,
            sender: sender.downgrade(),
            startup_channel_tx: None,
            starting_space: None,
            cur_space: Vec::new(),
            cur_screen_id: Vec::new(),
            disabled_spaces: HashSet::default(),
            enabled_spaces: HashSet::default(),
            login_window_pid: None,
            login_window_active: false,
            expose_active: false,
            is_globally_enabled,
            hotkeys: None,
            mtm: MainThreadMarker::new().unwrap(),
        };
        status_tx.send(status::Event::GlobalEnabledChanged(is_globally_enabled));
        (this, sender)
    }

    pub async fn run(mut self) {
        let (startup_channel_tx, startup_channel_rx) = mpsc::unbounded_channel();
        self.startup_channel_tx.replace(startup_channel_tx);
        let events_tx = self.events_tx.clone();
        join!(self.watch_events(), Self::startup(startup_channel_rx, events_tx));
    }

    async fn watch_events(&mut self) {
        while let Some((span, event)) = self.receiver.recv().await {
            let _guard = span.enter();
            self.handle_event(event);
        }
    }

    async fn startup(mut receiver: StartupReceiver, events_tx: reactor::Sender) {
        let _span = info_span!("Startup");
        while let Some(()) = receiver.recv().await {}
        debug!("Startup channel closed; sending startup event");
        events_tx.send(reactor::Event::StartupComplete);
    }

    #[instrument(skip(self))]
    pub fn handle_event(&mut self, event: WmEvent) {
        debug!("handle_event");
        use reactor::Event;

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
                if self.login_window_pid == Some(pid) {
                    // While the login screen is active AX APIs do not work.
                    // Disable all spaces to prevent errors.
                    info!("Login window activated");
                    self.login_window_active = true;
                    self.send_space_changed();
                }
                self.send_event(Event::ApplicationGloballyActivated(pid));
            }
            AppGloballyDeactivated(pid) => {
                if self.login_window_pid == Some(pid) {
                    // Re-enable spaces; this also causes the reactor to update
                    // the set of visible windows on screen and their positions.
                    info!("Login window deactivated");
                    self.login_window_active = false;
                    self.send_space_changed();
                }
                self.send_event(Event::ApplicationGloballyDeactivated(pid));
            }
            AppTerminated(pid) => {
                self.send_event(Event::ApplicationTerminated(pid));
            }
            ScreenParametersChanged {
                screens: ids,
                frames,
                scale_factors,
                spaces,
                converter,
                on_screen,
            } => {
                self.cur_screen_id = ids;
                self.handle_space_changed(spaces.clone());
                self.send_event(Event::ScreenParametersChanged {
                    frames: frames.clone(),
                    spaces: self.active_spaces(),
                    on_screen,
                    converter,
                    scale_factors,
                });
                self.status_tx.send(status::Event::SpaceChanged(spaces));
                self.status_tx.send(status::Event::SpaceEnabledChanged(
                    self.is_current_space_enabled(),
                ));
                self.mouse_tx.send(mouse::Request::ScreenParametersChanged(frames, converter));
            }
            SpaceChanged(spaces, on_screen) => {
                self.handle_space_changed(spaces.clone());
                if !self.expose_active {
                    // During expose windows from all spaces are returned to
                    // self.get_windows(), so we may send a faulty list to the
                    // reactor. This will be corrected when we get the
                    // ExposeExited event or switch back to the space again, but
                    // it adds visual noise so we try to avoid it.
                    self.send_event(Event::SpaceChanged(self.active_spaces(), Some(on_screen)));
                }
                self.status_tx.send(status::Event::SpaceChanged(spaces));
                self.status_tx.send(status::Event::SpaceEnabledChanged(
                    self.is_current_space_enabled(),
                ));
            }
            ExposeEntered => {
                self.expose_active = true;
            }
            ExposeExited => {
                self.expose_active = false;
                // We just need the reactor to update the list of visible
                // windows for the current space. Everything else is handled by
                // the SpaceChanged event.
                self.send_space_changed();
            }
            Command(Wm(ToggleSpaceActivated)) => {
                let Some(space) = self.get_focused_space() else { return };
                let toggle_set = if self.config.config.settings.default_disable {
                    &mut self.enabled_spaces
                } else {
                    &mut self.disabled_spaces
                };
                if !toggle_set.remove(&space) {
                    toggle_set.insert(space);
                }
                if !self.is_space_enabled(space) {
                    self.group_indicators_tx.send(group_bars::Event::SpaceDisabled(space));
                }
                self.status_tx
                    .send(status::Event::SpaceEnabledChanged(self.is_space_enabled(space)));
                self.send_space_changed();
            }
            Command(Wm(ToggleGlobalEnabled)) => {
                self.is_globally_enabled = !self.is_globally_enabled;
                if !self.is_globally_enabled {
                    self.group_indicators_tx.send(group_bars::Event::GlobalDisabled);
                }
                self.status_tx
                    .send(status::Event::GlobalEnabledChanged(self.is_globally_enabled));
                self.status_tx.send(status::Event::SpaceEnabledChanged(
                    self.is_current_space_enabled(),
                ));
                self.send_space_changed();
            }
            Command(Wm(Exec(cmd))) => {
                self.exec_cmd(cmd);
            }
            Command(ReactorCommand(cmd)) => {
                self.send_event(Event::Command(cmd));
            }
            ConfigUpdated(config) => {
                self.group_indicators_tx.send(group_bars::Event::ConfigChanged(config.clone()));
                self.mouse_tx.send(mouse::Request::ConfigUpdated(config.clone()));
                self.status_tx.send(status::Event::ConfigUpdated(config.clone()));
                self.status_tx.send(status::Event::SpaceEnabledChanged(
                    self.is_current_space_enabled(),
                ));
                self.send_event(reactor::Event::ConfigChanged(config.clone()));
                self.config.config = config;
                self.unregister_hotkeys();
                self.ensure_hotkey_registration();
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

    fn get_focused_space(&self) -> Option<SpaceId> {
        // The currently focused screen is what NSScreen calls the "main" screen.
        let screen = NSScreen::mainScreen(self.mtm)?;
        let number = screen.get_number().ok()?;
        *self.cur_screen_id.iter().zip(&self.cur_space).find(|(id, _)| **id == number)?.1
    }

    fn handle_space_changed(&mut self, spaces: Vec<Option<SpaceId>>) {
        self.cur_space = spaces;
        if self.starting_space.is_none() {
            self.starting_space = self.first_space();
        }
        self.ensure_hotkey_registration();
    }

    fn first_space(&self) -> Option<SpaceId> {
        self.cur_space.first().copied().flatten()
    }

    fn is_current_space_enabled(&self) -> bool {
        let Some(space) = self.get_focused_space() else {
            return false;
        };
        self.is_space_enabled(space)
    }

    fn is_space_enabled(&self, space: SpaceId) -> bool {
        match space {
            sp if self.config.config.settings.default_disable => self.enabled_spaces.contains(&sp),
            sp => !self.disabled_spaces.contains(&sp),
        }
    }

    fn active_spaces(&self) -> Vec<Option<SpaceId>> {
        if !self.is_globally_enabled {
            return vec![None; self.cur_space.len()];
        }
        let mut spaces = self.cur_space.clone();
        for space in &mut spaces {
            let enabled = match space {
                _ if self.login_window_active => false,
                Some(_) if self.config.one_space && *space != self.starting_space => false,
                Some(sp) if self.disabled_spaces.contains(sp) => false,
                Some(sp) if self.enabled_spaces.contains(sp) => true,
                _ if self.config.config.settings.default_disable => false,
                _ => true,
            };
            if !enabled {
                *space = None;
            }
        }
        spaces
    }

    fn send_event(&mut self, event: reactor::Event) {
        trace!(?event, "Sending event");
        self.events_tx.send(event);
    }

    fn send_space_changed(&mut self) {
        self.send_event(reactor::Event::SpaceChanged(self.active_spaces(), None));
    }

    fn ensure_hotkey_registration(&mut self) {
        let all_spaces = !self.config.one_space;
        let active = self.starting_space.is_some()
            && (all_spaces || self.starting_space == self.first_space());
        if active {
            if self.hotkeys.is_none() {
                self.register_hotkeys();
            }
        } else {
            self.unregister_hotkeys();
        }
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

    fn exec_cmd(&self, #[allow(unused)] cmd_args: ExecCmd) {
        #[cfg(not(feature = "exec_cmd"))]
        {
            error!(
                "exec_cmd is disabled in Glide due to security concerns. Enable it by rebuilding with the exec_cmd feature."
            );
            return;
        }

        // Spawn so we don't block the main thread.
        #[allow(unreachable_code)]
        std::thread::spawn(move || {
            let cmd_args = cmd_args.as_array();
            let [cmd, args @ ..] = &*cmd_args else {
                error!("Empty argument list passed to exec");
                return;
            };
            let output = std::process::Command::new(cmd).args(args).output();
            let output = match output {
                Ok(o) => o,
                Err(e) => {
                    error!("Failed to execute command {cmd:?}: {e:?}");
                    return;
                }
            };
            if !output.status.success() {
                error!(
                    "Exec command exited with status {}: {cmd:?} {args:?}",
                    output.status
                );
                error!("stdout: {}", String::from_utf8_lossy(&*output.stdout));
                error!("stderr: {}", String::from_utf8_lossy(&*output.stderr));
            }
        });
    }
}

impl ExecCmd {
    fn as_array(&self) -> Cow<'_, [String]> {
        match self {
            ExecCmd::Array(vec) => Cow::Borrowed(&*vec),
            ExecCmd::String(s) => s.split(' ').map(|s| s.to_owned()).collect::<Vec<_>>().into(),
        }
    }
}
