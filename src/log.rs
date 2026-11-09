// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::backtrace::Backtrace;
use std::io::{Stderr, Write, stderr};
use std::panic::PanicHookInfo;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::info;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};
use tracing_timing::{Histogram, group};
use tracing_tree::time::UtcDateTime;

fn log_dir() -> PathBuf {
    dirs::home_dir().unwrap().join("Library/Logs/Glide")
}

pub fn init_logging() {
    let log_dir = log_dir();
    // The appender lists the directory to prune old logs before creating it.
    std::fs::create_dir_all(&log_dir).unwrap();
    let logfile = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("glide")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&log_dir)
        .unwrap();
    let (file_appender, file_appender_guard) = tracing_appender::non_blocking(logfile);
    let panic_writer = file_appender.clone();
    let (err_appender, err_appender_guard) = tracing_appender::non_blocking(stderr());
    let original_hook = std::panic::take_hook();
    tracing_subscriber::registry()
        // .with(EnvFilter::from_default_env())
        // .with(timing_layer())
        .with(
            tree_layer()
                .with_writer(err_appender)
                .with_filter(EnvFilter::from_default_env()),
        )
        .with(tracing_subscriber::fmt::layer().with_writer(file_appender).with_ansi(false))
        .init();
    info!(
        "Glide {} started with pid {}",
        env!("CARGO_PKG_VERSION"),
        std::process::id()
    );

    let appender_guards = Mutex::new(Some((file_appender_guard, err_appender_guard)));
    std::panic::set_hook(Box::new(move |info| {
        // Bypass the subscriber, which may be what panicked.
        if let Err(e) = panic_writer.clone().write_all(panic_report(info).as_bytes()) {
            eprintln!("Failed to write panic report to the log: {e}");
        }
        // Because we abort and don't unwind on panic, make sure to flush the
        // appenders before that happens.
        if let Ok(mut guards) = appender_guards.try_lock() {
            guards.take();
        }
        original_hook(info);
    }));
}

fn panic_report(info: &PanicHookInfo) -> String {
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
    format!("thread {thread_info} panicked{location}:\n{payload}\nstack backtrace:\n{backtrace}\n")
}

pub fn tree_layer() -> tracing_tree::HierarchicalLayer<fn() -> Stderr, UtcDateTime> {
    tracing_tree::HierarchicalLayer::default()
        .with_indent_amount(2)
        .with_indent_lines(true)
        .with_deferred_spans(true)
        .with_span_retrace(true)
        .with_targets(true)
        .with_timer(UtcDateTime::default())
}

type TimingLayer = tracing_timing::TimingLayer<group::ByName, group::ByMessage>;

#[expect(unused, reason = "not yet useful")]
fn timing_layer() -> TimingLayer {
    tracing_timing::Builder::default()
        //.events(group::ByName)
        .layer(|| Histogram::new_with_max(100_000_000, 2).unwrap())
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub enum MetricsCommand {
    ShowTiming,
}

pub fn handle_command(command: MetricsCommand) {
    match command {
        MetricsCommand::ShowTiming => show_timing(),
    }
}

pub fn show_timing() {
    tracing::dispatcher::get_default(|d| {
        let Some(timing_layer) = d.downcast_ref::<TimingLayer>() else {
            return;
        };
        print_histograms(timing_layer);
    })
}

fn print_histograms(timing_layer: &TimingLayer) {
    timing_layer.force_synchronize();
    timing_layer.with_histograms(|hs| {
        println!("\nHistograms:\n");
        for (span, hs) in hs {
            for (event, h) in hs {
                let ns = |nanos| Duration::from_nanos(nanos);
                println!("{span} -> {event} ({} events)", h.len());
                println!("    mean: {:?}", ns(h.mean() as u64));
                println!("    min: {:?}", ns(h.min()));
                println!("    p50: {:?}", ns(h.value_at_quantile(0.50)));
                println!("    p90: {:?}", ns(h.value_at_quantile(0.90)));
                println!("    p99: {:?}", ns(h.value_at_quantile(0.99)));
                println!("    max: {:?}", ns(h.max()));
            }
        }
        println!();
    });
}

#[macro_export]
macro_rules! trace_call {
    ($($path:ident)::*($($args:expr),*)) => { {
        let start = ::std::time::Instant::now();
        let out = $($path)::* ($($args),*);
        let end = ::std::time::Instant::now();
        ::tracing::trace!(time = ?(end - start), stringify!($($path)::*));
        out
    } };
}
