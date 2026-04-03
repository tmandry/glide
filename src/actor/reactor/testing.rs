// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::Arc;

use accessibility_sys::pid_t;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tracing::{Span, debug, info};

use super::{Event, Reactor, Record, Requested, TransactionId};
use crate::actor::app::{AppThreadHandle, Request, WindowId};
use crate::actor::layout::LayoutManager;
use crate::actor::reactor;
use crate::config::Config;
use crate::sys::app::{AppInfo, WindowInfo};
use crate::sys::geometry::SameAs;
use crate::sys::window_server::{WindowServerId, WindowServerInfo, WindowsOnScreen};

impl Reactor {
    pub fn new_for_test(layout: LayoutManager) -> Reactor {
        let mut config = Config::default();
        config.settings.default_disable = false;
        config.settings.animate = false;
        let record = Record::new_for_test(tempfile::NamedTempFile::new().unwrap());
        let (group_indicators_tx, _) = crate::actor::channel();
        Reactor::new(Arc::new(config), layout, record, group_indicators_tx)
    }

    pub fn handle_events(&mut self, events: Vec<Event>) {
        for event in events {
            self.handle_event(event);
        }
    }
}

impl Drop for Reactor {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        // Replay the recorded data to make sure we can do so without crashing.
        if let Some(temp) = self.record.temp() {
            temp.as_file().flush().unwrap();
            println!("Replaying recorded data in {temp:?}:");
            if let Err(e) = reactor::replay(temp.path(), |_span, request| {
                info!(?request);
            }) {
                let persist_result = self.record.keep();
                println!("Persisting temp file: {:?}", persist_result);
                panic!("replay failed: {e}");
            }
        }
    }
}

pub fn make_window(idx: usize) -> WindowInfo {
    WindowInfo {
        is_standard: true,
        is_resizable: true,
        title: format!("Window{idx}").into(),
        frame: CGRect::new(
            CGPoint::new(100.0 * f64::from(idx as u32), 100.0),
            CGSize::new(50.0, 50.0),
        ),
        // TODO: This is wrong and conflicts with windows from other apps.
        sys_id: Some(WindowServerId::new(idx as u32)),
    }
}

pub fn make_windows(count: usize) -> Vec<WindowInfo> {
    (1..=count).map(make_window).collect()
}

pub struct Apps {
    tx: UnboundedSender<(Span, Request)>,
    rx: UnboundedReceiver<(Span, Request)>,
    pub windows: BTreeMap<WindowId, WindowState>,
}

#[derive(Default, PartialEq, Debug, Clone)]
pub struct WindowState {
    pub last_seen_txid: TransactionId,
    pub animating: bool,
    pub frame: CGRect,
}

impl Apps {
    pub fn new() -> Apps {
        let (tx, rx) = unbounded_channel();
        Apps {
            tx,
            rx,
            windows: BTreeMap::new(),
        }
    }

    pub fn make_app(&mut self, pid: pid_t, windows: Vec<WindowInfo>) -> Vec<Event> {
        let frontmost = windows.first().map(|_| WindowId::new(pid, 1));
        self.make_app_with_opts(pid, windows, frontmost, false)
    }

    pub fn make_app_with_opts(
        &mut self,
        pid: pid_t,
        windows: Vec<WindowInfo>,
        main_window: Option<WindowId>,
        is_frontmost: bool,
    ) -> Vec<Event> {
        self.make_app_impl(pid, windows, main_window, is_frontmost, true)
    }

    pub fn make_app_without_ws_info(
        &mut self,
        pid: pid_t,
        windows: Vec<WindowInfo>,
        main_window: Option<WindowId>,
        is_frontmost: bool,
    ) -> Vec<Event> {
        self.make_app_impl(pid, windows, main_window, is_frontmost, false)
    }

    fn make_app_impl(
        &mut self,
        pid: pid_t,
        windows: Vec<WindowInfo>,
        main_window: Option<WindowId>,
        is_frontmost: bool,
        with_ws_info: bool,
    ) -> Vec<Event> {
        for (id, info) in (1..).map(|idx| WindowId::new(pid, idx)).zip(&windows) {
            self.windows.insert(
                id,
                WindowState {
                    frame: info.frame,
                    ..Default::default()
                },
            );
        }
        let handle = AppThreadHandle::new_for_test(self.tx.clone());
        let mut events = vec![];
        if with_ws_info {
            let ws_info: Vec<WindowServerInfo> = windows
                .iter()
                .filter_map(|info| {
                    Some(WindowServerInfo {
                        pid,
                        id: info.sys_id?,
                        layer: 0,
                        frame: info.frame,
                    })
                })
                .collect();
            events.push(Event::WindowsOnScreenUpdated {
                pid: Some(pid),
                on_screen: WindowsOnScreen::new(ws_info),
            });
        }
        events.push(Event::ApplicationLaunched {
            pid,
            info: AppInfo {
                bundle_id: Some(format!("com.testapp{pid}")),
                localized_name: Some(format!("TestApp{pid}")),
            },
            handle,
            is_frontmost,
            main_window,
            visible_windows: (1..).map(|idx| WindowId::new(pid, idx)).zip(windows).collect(),
        });
        events
    }

    pub fn requests(&mut self) -> Vec<Request> {
        let mut requests = Vec::new();
        while let Ok((_, req)) = self.rx.try_recv() {
            requests.push(req);
        }
        requests
    }

    pub fn simulate_until_quiet(&mut self, reactor: &mut Reactor) {
        let mut requests = self.requests();
        while !requests.is_empty() {
            for event in self.simulate_events_for_requests(requests) {
                reactor.handle_event(event);
            }
            requests = self.requests();
        }
    }

    pub fn simulate_events(&mut self) -> Vec<Event> {
        let requests = self.requests();
        self.simulate_events_for_requests(requests)
    }

    pub fn simulate_events_for_requests(&mut self, requests: Vec<Request>) -> Vec<Event> {
        let mut events = vec![];
        let mut got_visible_windows = false;
        for request in requests {
            debug!(?request);
            match request {
                Request::Terminate => break,
                Request::GetVisibleWindows => {
                    // Only do this once per cycle, since we simulate responding
                    // from all apps.
                    if got_visible_windows {
                        continue;
                    }
                    got_visible_windows = true;
                    let mut app_windows = BTreeMap::<pid_t, Vec<WindowId>>::new();
                    for &wid in self.windows.keys() {
                        app_windows.entry(wid.pid).or_default().push(wid);
                    }
                    for (pid, windows) in app_windows {
                        events.push(Event::WindowsDiscovered {
                            pid,
                            new: vec![],
                            known_visible: windows,
                        });
                    }
                }
                Request::SetWindowFrame(wid, frame, txid) => {
                    let window = self.windows.entry(wid).or_default();
                    window.last_seen_txid = txid;
                    let old_frame = window.frame;
                    window.frame = frame;
                    if !window.animating && !old_frame.same_as(frame) {
                        events.push(Event::WindowFrameChanged(
                            wid,
                            frame,
                            txid,
                            Requested(true),
                            None,
                        ));
                    }
                }
                Request::SetWindowPos(wid, pos, txid) => {
                    let window = self.windows.entry(wid).or_default();
                    window.last_seen_txid = txid;
                    let old_frame = window.frame;
                    window.frame.origin = pos;
                    if !window.animating && !old_frame.same_as(window.frame) {
                        events.push(Event::WindowFrameChanged(
                            wid,
                            window.frame,
                            txid,
                            Requested(true),
                            None,
                        ));
                    }
                }
                Request::BeginWindowAnimation(wid) => {
                    self.windows.entry(wid).or_default().animating = true;
                }
                Request::EndWindowAnimation(wid) => {
                    let window = self.windows.entry(wid).or_default();
                    window.animating = false;
                    events.push(Event::WindowFrameChanged(
                        wid,
                        window.frame,
                        window.last_seen_txid,
                        Requested(true),
                        None,
                    ));
                }
                Request::Raise(..) => todo!(),
                Request::WindowDestroyed(..) => todo!(),
            }
        }
        debug!(?events);
        events
    }
}
