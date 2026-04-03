// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! This tool is used to exercise glide and system APIs during development.

use std::env::VarError;
use std::future::Future;
use std::path::PathBuf;
use std::ptr;
use std::time::Instant;

use accessibility::{AXUIElement, AXUIElementAttributes};
use accessibility_sys::{AXUIElementCopyElementAtPosition, AXUIElementRef, pid_t};
use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use core_foundation::array::CFArray;
use core_foundation::base::{FromMutVoid, TCFType};
use core_foundation::dictionary::CFDictionaryRef;
use core_graphics::display::{CGDisplayBounds, CGMainDisplayID};
use core_graphics::window::{
    CGWindowID, CGWindowListCopyWindowInfo, kCGNullWindowID, kCGWindowListOptionOnScreenOnly,
};
use glide_wm::actor::{self, reactor};
use glide_wm::sys::app::{AXUIElementExt, AppInfo, NSRunningApplicationExt, WindowInfo};
use glide_wm::sys::event::{self, get_mouse_pos};
use glide_wm::sys::executor::Executor;
use glide_wm::sys::screen::{self, ScreenCache};
use glide_wm::sys::window_server::{self, WindowServerId, get_window};
use glide_wm::sys::{self};
use livesplit_hotkey::{ConsumePreference, Modifiers};
use objc2_app_kit::{NSRunningApplication, NSScreen, NSWindow, NSWindowNumberListOptions};
use objc2_foundation::{MainThreadMarker, NSString};
use tokio::sync::mpsc::{self, UnboundedReceiver, unbounded_channel};
use tracing::info;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[derive(Parser)]
struct Opt {
    #[arg(long)]
    bundle: Option<String>,
    #[command(subcommand)]
    command: Command,
    #[arg(long)]
    verbose: bool,
}

#[derive(Subcommand, Clone)]
enum Command {
    #[command(subcommand)]
    List(List),
    #[command(subcommand)]
    App(App),
    #[command(subcommand)]
    WindowServer(WindowServer),
    #[command()]
    Replay(Replay),
    #[command(subcommand)]
    Mouse(Mouse),
    #[command()]
    Inspect,
}

#[derive(Subcommand, Clone)]
enum List {
    All,
    Apps,
    Ax,
    Cg,
    Ns,
    Spaces,
}

#[derive(Subcommand, Clone)]
enum App {
    #[command()]
    SetMainWindow {
        pid: pid_t,
        window_server_id: CGWindowID,
        #[arg(long)]
        wait: bool,
    },
    #[command()]
    ReadMainWindow {
        pid: pid_t,
        #[arg(long)]
        wait: bool,
    },
    #[command()]
    Run { pid_or_bundle: String },
}

#[derive(Subcommand, Clone)]
enum WindowServer {
    #[command()]
    List {
        #[arg(short, long)]
        all: bool,
        /// Whether to show the raw window dictionaries. Implies --all.
        #[arg(short, long)]
        raw: bool,
    },
    #[command()]
    Get { id: u32 },
}

#[derive(Parser, Clone)]
struct Replay {
    path: PathBuf,
}

#[derive(Subcommand, Clone)]
enum Mouse {
    #[command()]
    Clicks,
    #[command()]
    Hide,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // Set default log level to info.
    if let Err(VarError::NotPresent) = std::env::var("RUST_LOG") {
        // SAFETY: No other threads running.
        unsafe {
            std::env::set_var("RUST_LOG", "info");
        }
    }
    tracing_subscriber::registry()
        .with(glide_wm::log::tree_layer())
        .with(EnvFilter::from_default_env())
        .init();
    let opt: Opt = Parser::parse();

    match opt.command {
        Command::List(List::Ax) => {
            time("accessibility", || get_windows_with_ax(&opt, false, true)).await
        }
        Command::List(List::Cg) => time("core-graphics", || get_windows_with_cg(&opt, true)).await,
        Command::List(List::Ns) => time("ns-window", || get_windows_with_ns(&opt, true)).await,
        Command::List(List::Apps) => get_apps(&opt),
        Command::List(List::All) => {
            //time("accessibility serial", || get_windows_with_ax(&opt, true)).await;
            time("core-graphics", || get_windows_with_cg(&opt, true)).await;
            time("ns-window", || get_windows_with_ns(&opt, true)).await;
            time("accessibility", || get_windows_with_ax(&opt, false, true)).await;
            time("core-graphics second time", || get_windows_with_cg(&opt, false)).await;
            time("ns-window second time", || get_windows_with_ns(&opt, false)).await;
            time("accessibility second time", || {
                get_windows_with_ax(&opt, false, false)
            })
            .await;
        }
        Command::List(List::Spaces) => {
            println!("Current space: {:?}", screen::diagnostic::cur_space());
            println!("Visible spaces: {:?}", screen::diagnostic::visible_spaces());
            println!("All spaces: {:?}", screen::diagnostic::all_spaces());
            println!(
                "Managed display spaces: {:?}",
                screen::diagnostic::managed_display_spaces()
            );

            dbg!(screen::diagnostic::managed_displays());
            let screens = NSScreen::screens(MainThreadMarker::new().unwrap());
            let frames: Vec<_> = screens.iter().map(|screen| screen.visibleFrame()).collect();
            println!("NSScreen sizes: {frames:?}");

            println!();
            let mtm = MainThreadMarker::new().unwrap();
            let mut sc = ScreenCache::new();
            let ns_screens = screen::get_ns_screens(mtm);
            println!("Frames: {:?}", sc.update_screen_config(ns_screens));
            println!("Spaces: {:?}", sc.get_screen_spaces());
        }
        Command::App(App::SetMainWindow { pid, window_server_id, wait }) => {
            let app = AXUIElement::application(pid);
            let windows = app.windows()?;
            let window = windows
                .iter()
                .filter(|w| {
                    let id: Result<window_server::WindowServerId, _> = (&**w).try_into();
                    id.is_ok_and(|id| id.as_u32() == window_server_id)
                })
                .next()
                .context("Could not find matching window")?;
            if wait {
                println!("Press enter to complete action");
                std::io::stdin().read_line(&mut String::new())?;
                window.set_messaging_timeout(3600.0)?;
            }
            window.set_main(true).context("Failed to set window as main")?;
        }
        Command::App(App::ReadMainWindow { pid, wait }) => {
            let app = AXUIElement::application(pid);
            println!("frontmost = {:?}", app.frontmost()?);
            let main_window = if opt.verbose {
                let main_window = dbg!(app.main_window()?);
                let main_window_id: WindowServerId = (&app.main_window()?).try_into()?;
                dbg!(main_window_id);
                main_window
            } else {
                let main_window = app.main_window();
                println!("main_window = {:?}", main_window.as_ref().map(|_| ()));
                // let main_window_id: WindowServerId = (&app.main_window()?).try_into()?;
                // dbg!(main_window_id);
                main_window?
            };
            if wait {
                println!("Press enter to complete action");
                std::io::stdin().read_line(&mut String::new())?;
                app.set_messaging_timeout(3600.0)?;
            }
            dbg!(main_window.main()?);
        }
        Command::App(App::Run { pid_or_bundle }) => {
            run_app_actor(pid_or_bundle).await?;
        }
        Command::WindowServer(WindowServer::List { all, raw }) => {
            if raw {
                for window in window_server::get_visible_windows_raw().iter() {
                    println!("{window:?}");
                }
            } else {
                let layer = if all { None } else { Some(0) };
                for window in window_server::get_visible_windows_with_layer(layer) {
                    println!("{window:?}");
                }
            }
        }
        Command::WindowServer(WindowServer::Get { id }) => {
            match window_server::get_window(WindowServerId(id)) {
                Some(win) => println!("{win:?}"),
                None => println!("Could not find window {id}"),
            }
        }
        Command::Replay(Replay { path }) => {
            reactor::replay(&path, |_span, request| {
                info!(?request);
            })?;
        }
        Command::Mouse(Mouse::Clicks) => {
            use core_foundation::runloop::{CFRunLoop, kCFRunLoopCommonModes};
            use core_graphics::event::{
                CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
            };
            let current = CFRunLoop::get_current();
            match CGEventTap::new(
                CGEventTapLocation::HID,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::Default,
                vec![CGEventType::LeftMouseUp],
                |_a, _b, d| {
                    println!("{:?}", d.location());
                    core_graphics::event::CallbackResult::Keep
                },
            ) {
                Ok(tap) => unsafe {
                    let loop_source = tap.mach_port().create_runloop_source(0).unwrap();
                    current.add_source(&loop_source, kCFRunLoopCommonModes);
                    tap.enable();
                    CFRunLoop::run_current();
                },
                Err(_) => assert!(false),
            }
        }
        Command::Mouse(Mouse::Hide) => {
            window_server::allow_hide_mouse().unwrap();
            event::hide_mouse().unwrap();

            println!("Press enter to show");
            std::io::stdin().read_line(&mut String::new())?;
            event::show_mouse().unwrap();

            println!("Press enter to exit");
            std::io::stdin().read_line(&mut String::new())?;
        }
        Command::Inspect => inspect(MainThreadMarker::new().unwrap()),
    }
    Ok(())
}

async fn run_app_actor(pid_or_bundle: String) -> anyhow::Result<()> {
    let pid = match pid_or_bundle.parse() {
        Ok(pid) => pid,
        Err(_) => {
            let apps = NSRunningApplication::runningApplicationsWithBundleIdentifier(
                &NSString::from_str(&pid_or_bundle),
            );
            match apps.len() {
                0 => bail!("Could not find any applications with bundle id {pid_or_bundle}"),
                1 => apps.firstObject().unwrap().pid(),
                _ => bail!("Found multiple applications with bundle id {pid_or_bundle}"),
            }
        }
    };
    let info = AppInfo::from(
        &*NSRunningApplication::with_process_id(pid).expect("Could not get running application"),
    );
    let (ws_tx, mut ws_rx) = actor::channel();
    actor::app::spawn_app_thread(pid, info, ws_tx, None);
    while let Some((span, event)) = ws_rx.recv().await {
        let _sp = span.enter();
        info!("{event:?}");
    }
    Ok(())
}

fn inspect(mtm: MainThreadMarker) {
    let (tx, rx) = unbounded_channel();
    let hook =
        livesplit_hotkey::Hook::with_consume_preference(ConsumePreference::MustConsume).unwrap();
    let key = event::Hotkey {
        key_code: event::KeyCode::KeyI,
        modifiers: Modifiers::ALT | Modifiers::SHIFT,
    };
    hook.register(key, move || _ = tx.send(())).unwrap();
    println!("Press {key:?} to inspect the window under the mouse");
    Executor::run(inspect_inner(rx, mtm));
}

async fn inspect_inner(mut rx: UnboundedReceiver<()>, mtm: MainThreadMarker) {
    let mut screen_cache = ScreenCache::new();
    let ns_screens = screen::get_ns_screens(mtm);
    let Some((_, converter)) = screen_cache.update_screen_config(ns_screens) else {
        return;
    };
    while let Some(()) = rx.recv().await {
        println!();
        let Some(pos) = get_mouse_pos(converter) else { continue };
        // This API doesn't always work, but for some reason get_window_at_point
        // *never* works from devtool.
        let mut element: AXUIElementRef = ptr::null_mut();
        let err = unsafe {
            AXUIElementCopyElementAtPosition(
                AXUIElement::system_wide().as_CFTypeRef() as _,
                pos.x as f32,
                pos.y as f32,
                &raw mut element,
            )
        };
        if err != 0 {
            println!("Failed to get element under cursor: {err:?}");
            continue;
        }
        let elem = unsafe { AXUIElement::from_mut_void(element as *mut _) };
        let ax_window = match elem.window() {
            Ok(win) => win,
            Err(e) => {
                println!(
                    "Warning: no window for element {element:#?} ({e}); inspecting the element"
                );
                elem.clone()
            }
        };
        println!("{:#?}", ax_window.privacy_sensitive_inspect());
        let Some(info) =
            WindowServerId::try_from(&ax_window).ok().and_then(|wsid| get_window(wsid))
        else {
            println!("Couldn't get window server info for {element:?}");
            continue;
        };
        println!("{info:#?}");
        if let Some(app) = NSRunningApplication::with_process_id(info.pid)
            && let Some(bundle_id) = app.bundleIdentifier()
        {
            println!("bundle_id: {:?}", bundle_id);
        }
    }
}

async fn get_windows_with_cg(opt: &Opt, print: bool) {
    let windows: CFArray<CFDictionaryRef> = unsafe {
        CFArray::wrap_under_get_rule(CGWindowListCopyWindowInfo(
            kCGWindowListOptionOnScreenOnly,
            kCGNullWindowID,
        ))
    };
    if print && opt.verbose {
        println!("{windows:?}");
    }
    if print {
        println!("visible window ids:");
        for window in window_server::get_visible_windows_with_layer(None) {
            if opt.verbose {
                println!("- {window:?}");
            } else {
                println!("- {id:?}, pid={pid:?}", id = window.id, pid = window.pid);
            }
        }
    }
    let display_id = unsafe { CGMainDisplayID() };
    let screen = unsafe { CGDisplayBounds(display_id) };
    if print {
        println!("main display = {screen:?}");
    }
}

async fn get_windows_with_ns(_opt: &Opt, print: bool) {
    let mtm = MainThreadMarker::new().unwrap();
    let windows =
        NSWindow::windowNumbersWithOptions(NSWindowNumberListOptions::AllApplications, mtm);
    if print {
        println!("{windows:?}");
    }
}

async fn get_windows_with_ax(opt: &Opt, serial: bool, print: bool) {
    let (sender, mut receiver) = mpsc::unbounded_channel();
    for (pid, bundle_id) in sys::app::running_apps(opt.bundle.clone()) {
        let sender = sender.clone();
        let verbose = opt.verbose;
        let task = move || {
            let app = AXUIElement::application(pid);
            let windows = get_windows_for_app(app, verbose);
            sender.send((bundle_id, windows)).unwrap()
        };
        if serial {
            task();
        } else {
            tokio::task::spawn_blocking(task);
        }
    }
    drop(sender);
    while let Some((info, windows)) = receiver.recv().await {
        //println!("{info:?}");
        match windows {
            Ok(windows) => {
                if print {
                    for (win, dbg) in windows {
                        println!("{win:?} from {}", info.bundle_id.as_deref().unwrap_or("?"));
                        if opt.verbose {
                            println!("=> {dbg}");
                        }
                    }
                }
            }
            Err(_) => (), //println!("  * Error reading windows: {err:?}"),
        }
    }
}

fn get_windows_for_app(
    app: AXUIElement,
    verbose: bool,
) -> Result<Vec<(WindowInfo, String)>, accessibility::Error> {
    let Ok(windows) = &app.windows() else {
        return Err(accessibility::Error::NotFound);
    };
    windows
        .iter()
        .map(|win| {
            Ok((
                WindowInfo::try_from(&*win)?,
                verbose.then(|| format!("{:#?}", &*win)).unwrap_or_default(),
            ))
        })
        .collect()
}

fn get_apps(opt: &Opt) {
    for (pid, _bundle_id) in sys::app::running_apps(opt.bundle.clone()) {
        let app = AXUIElement::application(pid);
        println!("{app:#?}");
    }
}

async fn time<O, F: Future<Output = O>>(desc: &str, f: impl FnOnce() -> F) -> O {
    let start = Instant::now();
    let out = f().await;
    let end = Instant::now();
    println!("{desc} took {:?}", end - start);
    out
}
