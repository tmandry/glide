// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::backtrace::Backtrace;
use std::fs::File;
use std::io::Write;
use std::panic::PanicHookInfo;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use glide_wm::actor::dock::Dock;
use glide_wm::actor::group_bars::GroupBars;
use glide_wm::actor::layout::LayoutManager;
use glide_wm::actor::mouse::Mouse;
use glide_wm::actor::notification_center::NotificationCenter;
use glide_wm::actor::reactor::{self, Reactor};
use glide_wm::actor::server::MessageServer;
use glide_wm::actor::status::Status;
use glide_wm::actor::window_server::{self, SkylightWatcher};
use glide_wm::actor::wm_controller::{self, WmController};
use glide_wm::actor::{channel, server};
use glide_wm::config::{Config, restore_file};
use glide_wm::log;
use glide_wm::sys::executor::Executor;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSAlert, NSApp, NSApplicationActivationPolicy};
use objc2_foundation::ns_string;
use tokio::join;
use tracing::warn;

#[derive(Parser)]
#[command(version, name = "glide_server")]
struct Cli {
    /// Only run the window manager on the current space.
    #[arg(long)]
    one: bool,

    /// Disable new spaces by default.
    ///
    /// Ignored if --one is used.
    #[arg(long)]
    default_disable: bool,

    /// Disable animations.
    #[arg(long)]
    no_animate: bool,

    /// Check whether the restore file can be loaded without actually starting
    /// the window manager.
    #[arg(long)]
    validate: bool,

    /// Restore the layout saved with the save_and_exit command. This is only
    /// useful within the same login session.
    #[arg(long)]
    restore: bool,

    /// Record reactor events to the specified file path. Overwrites the file if
    /// exists.
    #[arg(long)]
    record: Option<PathBuf>,

    /// Path to a custom config file.
    #[arg(long, short)]
    config: Option<PathBuf>,
}

fn main() {
    let opt: Cli = Parser::parse();

    if std::env::var_os("RUST_BACKTRACE").is_none() {
        // SAFETY: We are single threaded at this point.
        unsafe { std::env::set_var("RUST_BACKTRACE", "1") };
    }
    log::init_logging();
    install_panic_hook();
    let mtm = MainThreadMarker::new().unwrap();

    // When run from the command line, alerts won't be visible unless this is called.
    if !NSApp(mtm).setActivationPolicy(NSApplicationActivationPolicy::Regular) {
        warn!("Failed to set activation policy");
    }

    if glide_wm::ui::permission_flow::obtain_permissions(mtm).is_err() {
        eprintln!("Permissions not granted; exiting");
        std::process::exit(2)
    }

    let config_result = Config::load(opt.config.as_deref());
    let Ok(mut config) = config_result else {
        let alert = NSAlert::new(mtm);
        alert.setMessageText(ns_string!(
            "Failed to load config.

            Run \"glide config verify\" on the command line to see errors."
        ));
        println!("{}", alert.messageText());
        alert.runModal();
        std::process::exit(3);
    };
    config.settings.animate &= !opt.no_animate;
    config.settings.default_disable |= opt.default_disable;
    let config = Arc::new(config);

    if !NSApp(mtm).setActivationPolicy(NSApplicationActivationPolicy::Accessory) {
        warn!("Failed to set activation policy");
    }
    NSApp(mtm).finishLaunching();

    if opt.validate {
        LayoutManager::load(restore_file()).unwrap();
        return;
    }

    let layout = if opt.restore {
        LayoutManager::load(restore_file()).unwrap()
    } else {
        LayoutManager::new()
    };
    let (mouse_tx, mouse_rx) = channel();
    let (status_tx, status_rx) = channel();

    let (group_indicators_tx, group_indicators_rx) = glide_wm::actor::channel();
    let (events_tx, events_rx) = reactor::channel();
    let (skylight_tx, skylight_rx) = glide_wm::actor::channel::<window_server::SkylightRequest>();
    let wm_config = wm_controller::Config {
        one_space: opt.one,
        restore_file: restore_file(),
        config: config.clone(),
    };
    let (ws_tx, ws_rx) = glide_wm::actor::channel();
    let notification_center_ws_tx = ws_tx.clone();
    let (sm_tx, sm_rx) = glide_wm::actor::channel();
    let (wm_controller, wm_controller_tx) = WmController::new(
        wm_config,
        sm_tx.clone(),
        mouse_tx.clone(),
        status_tx.clone(),
        ws_tx.clone(),
    );
    let dock_sm_tx = sm_tx.clone();
    let skylight_watcher = SkylightWatcher::new(mtm);
    Reactor::spawn(
        config.clone(),
        opt.one,
        layout,
        reactor::Record::new(opt.record.as_deref()),
        mouse_tx.clone(),
        status_tx.clone(),
        group_indicators_tx,
        events_tx.clone(),
        events_rx,
        wm_controller_tx.clone(),
        ws_tx,
        ws_rx,
        sm_tx,
        sm_rx,
        skylight_tx,
    );

    let notification_center =
        NotificationCenter::new(wm_controller_tx.clone(), notification_center_ws_tx);
    let mouse = Mouse::new(config.clone(), events_tx.clone(), mouse_rx);
    let status = Status::new(
        config.clone(),
        status_rx,
        mtm,
        events_tx.clone(),
        wm_controller_tx.clone(),
    );
    let group_bars = GroupBars::new(config.clone(), group_indicators_rx, mtm);
    let dock = Dock::new(dock_sm_tx);

    // TODO: Run on another thread so we don't tie up the main thread.
    let message_server = MessageServer::new(server::PORT_NAME, wm_controller_tx)
        .expect("Glide may be already running");

    Executor::run_main(mtm, async move {
        join!(
            wm_controller.run(),
            notification_center.watch_for_notifications(),
            mouse.run(),
            status.run(),
            skylight_watcher.run(skylight_rx),
            dock.run(),
            group_bars.run(),
            message_server.run(),
        );
    });
}

fn install_panic_hook() {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        write_panic_log(&info);
        original_hook(info);
        // Abort on panic instead of propagating panics to the main thread.
        // See Cargo.toml for why we don't use panic=abort everywhere.
        #[cfg(panic = "unwind")]
        std::process::abort();
    }));
}

fn write_panic_log(info: &PanicHookInfo) {
    let pid = std::process::id();
    let filename = format!("/tmp/glide.{pid}.panic.log");
    let mut file = File::options().append(true).create(true).write(true).open(&filename).unwrap();

    let payload = info
        .payload()
        .downcast_ref::<String>()
        .map(|s| &**s)
        .or(info.payload().downcast_ref::<&str>().map(|s| &**s))
        .unwrap_or("Unknown error");
    let location = info
        .location()
        .map(|l| format!(" at {}:{}", l.file(), l.line()))
        .unwrap_or_default();
    let thread = std::thread::current();
    let thread_id = thread.id();
    let thread_info = match thread.name() {
        Some(name) => format!("'{name}' {thread_id:?}"),
        None => format!("{thread_id:?}"),
    };

    let backtrace = Backtrace::force_capture();
    let log_message = format!(
        "thread {thread_info} panicked{location}:\n{payload}\nstack backtrace:\n{backtrace}"
    );

    if let Err(e) = writeln!(&mut file, "{}", log_message) {
        eprintln!("Failed to write panic message to file: {}", e);
    }
    eprintln!("wrote panic info to {filename}");
}
