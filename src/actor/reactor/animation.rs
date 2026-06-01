// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::time::Duration;

use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use tokio::sync::mpsc;

use super::TransactionId;
use crate::actor::app::{AppThreadHandle, Request, WindowId};
use crate::sys::timer::Timer;

pub type Sender = mpsc::UnboundedSender<Message>;
pub type Receiver = mpsc::UnboundedReceiver<Message>;

#[derive(Debug)]
pub enum Message {
    Replace(Animation),
    SkipToEnd(Animation),
}

#[derive(Debug, Default)]
pub struct AnimationManager {
    active: Option<ActiveAnimation>,
}

#[derive(Debug)]
struct ActiveAnimation {
    animation: Animation,
    next_frame: u32,
}

#[derive(Debug)]
pub struct Animation {
    interval: Duration,
    frames: u32,
    windows: Vec<AnimatedWindow>,
}

#[derive(Debug)]
struct AnimatedWindow {
    handle: AppThreadHandle,
    wid: WindowId,
    start: CGRect,
    finish: CGRect,
    is_focus: bool,
    txid: TransactionId,
}

impl AnimatedWindow {
    fn frame_after(&self, frame: u32, total_frames: u32) -> CGRect {
        if frame == 0 {
            return if self.is_focus {
                CGRect {
                    origin: self.start.origin,
                    size: self.finish.size,
                }
            } else {
                self.start
            };
        }

        let t = f64::from(frame) / f64::from(total_frames);
        let mut rect = get_frame(self.start, self.finish, t);
        if self.is_focus || frame * 2 >= total_frames {
            rect.size = self.finish.size;
        } else {
            rect.size = self.start.size;
        }
        rect
    }
}

impl AnimationManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn run(mut rx: Receiver) {
        let mut manager = Self::new();
        let mut tick_timer = Timer::manual();

        loop {
            tokio::select! {
                message = rx.recv() => {
                    let Some(message) = message else {
                        manager.finish_active();
                        break;
                    };
                    if let Some(delay) = manager.handle_message(message) {
                        tick_timer.set_next_fire(delay);
                    }
                }
                _ = tick_timer.next(), if manager.active.is_some() => {
                    if let Some(delay) = manager.tick() {
                        tick_timer.set_next_fire(delay);
                    }
                }
            }
        }
    }

    pub fn handle_message(&mut self, message: Message) -> Option<Duration> {
        match message {
            Message::Replace(animation) => {
                self.active = match self.active.take() {
                    Some(active) => Some(active.replace_with(animation)),
                    None => ActiveAnimation::start(animation),
                };
                self.active.as_ref().map(|active| active.animation.interval)
            }
            Message::SkipToEnd(animation) => {
                self.finish_active();
                animation.skip_to_end();
                None
            }
        }
    }

    pub fn tick(&mut self) -> Option<Duration> {
        let active = self.active.as_mut()?;
        active.send_next_frame();
        if active.is_complete() {
            let active = self.active.take().expect("animation disappeared while ticking");
            active.animation.end();
            None
        } else {
            Some(active.animation.interval)
        }
    }

    fn finish_active(&mut self) {
        if let Some(active) = self.active.take() {
            active.animation.skip_to_end_and_end();
        }
    }
}

impl ActiveAnimation {
    fn start(animation: Animation) -> Option<Self> {
        if animation.is_empty() {
            return None;
        }
        animation.begin();
        Some(Self { animation, next_frame: 1 })
    }

    fn replace_with(self, mut next: Animation) -> Self {
        let continuing = next.patch_starts_from(self.current_frames());
        next.begin_windows_not_in(&continuing);
        self.animation.finish_windows_not_in(&continuing);
        Self { animation: next, next_frame: 1 }
    }

    fn send_next_frame(&mut self) {
        self.animation.send_frame(self.next_frame);
        self.next_frame += 1;
    }

    fn is_complete(&self) -> bool {
        self.next_frame > self.animation.frames
    }

    fn current_frames(&self) -> Vec<(WindowId, CGRect)> {
        let frame = self.next_frame.saturating_sub(1);
        self.animation
            .windows
            .iter()
            .map(|window| (window.wid, window.frame_after(frame, self.animation.frames)))
            .collect()
    }
}

impl Animation {
    pub fn new() -> Self {
        const FPS: f64 = 100.0;
        const DURATION: f64 = 0.30;
        let interval = Duration::from_secs_f64(1.0 / FPS);
        Animation {
            interval,
            frames: (DURATION * FPS).round() as u32,
            windows: vec![],
        }
    }

    pub fn add_window(
        &mut self,
        handle: &AppThreadHandle,
        wid: WindowId,
        start: CGRect,
        finish: CGRect,
        is_focus: bool,
        txid: TransactionId,
    ) {
        self.windows.push(AnimatedWindow {
            handle: handle.clone(),
            wid,
            start,
            finish,
            is_focus,
            txid,
        });
    }

    pub fn skip_to_end(&self) {
        for window in &self.windows {
            _ = window
                .handle
                .send(Request::SetWindowFrame(window.wid, window.finish, window.txid));
        }
    }

    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }

    fn begin(&self) {
        self.begin_windows_not_in(&[]);
    }

    fn begin_windows_not_in(&self, skip: &[WindowId]) {
        for window in &self.windows {
            if skip.contains(&window.wid) {
                continue;
            }
            _ = window.handle.send(Request::BeginWindowAnimation(window.wid));
            if window.is_focus {
                let frame = CGRect {
                    origin: window.start.origin,
                    size: window.finish.size,
                };
                _ = window.handle.send(Request::SetWindowFrame(window.wid, frame, window.txid));
            }
        }
    }

    fn finish_windows_not_in(&self, keep: &[WindowId]) {
        for window in &self.windows {
            if keep.contains(&window.wid) {
                continue;
            }
            _ = window
                .handle
                .send(Request::SetWindowFrame(window.wid, window.finish, window.txid));
            _ = window.handle.send(Request::EndWindowAnimation(window.wid));
        }
    }

    fn send_frame(&self, frame: u32) {
        let t = f64::from(frame) / f64::from(self.frames);
        for window in &self.windows {
            let mut rect = get_frame(window.start, window.finish, t);
            if frame * 2 == self.frames || frame == self.frames {
                rect.size = window.finish.size;
                _ = window.handle.send(Request::SetWindowFrame(window.wid, rect, window.txid));
            } else {
                _ = window.handle.send(Request::SetWindowPos(window.wid, rect.origin, window.txid));
            }
        }
    }

    fn end(&self) {
        for window in &self.windows {
            _ = window.handle.send(Request::EndWindowAnimation(window.wid));
        }
    }

    fn patch_starts_from(&mut self, current_frames: Vec<(WindowId, CGRect)>) -> Vec<WindowId> {
        let mut continuing = Vec::new();
        for (wid, current_frame) in current_frames {
            let Some(window) = self.windows.iter_mut().find(|window| window.wid == wid) else {
                continue;
            };
            window.start = current_frame;
            continuing.push(wid);
        }
        continuing
    }

    fn skip_to_end_and_end(self) {
        if self.windows.is_empty() {
            return;
        }
        self.skip_to_end();
        self.end();
    }
}

fn get_frame(a: CGRect, b: CGRect, t: f64) -> CGRect {
    let s = ease(t);
    CGRect {
        origin: CGPoint {
            x: blend(a.origin.x, b.origin.x, s),
            y: blend(a.origin.y, b.origin.y, s),
        },
        size: CGSize {
            width: blend(a.size.width, b.size.width, s),
            height: blend(a.size.height, b.size.height, s),
        },
    }
}

fn ease(t: f64) -> f64 {
    if t < 0.5 {
        (1.0 - f64::sqrt(1.0 - f64::powi(2.0 * t, 2))) / 2.0
    } else {
        (f64::sqrt(1.0 - f64::powi(-2.0 * t + 2.0, 2)) + 1.0) / 2.0
    }
}

fn blend(a: f64, b: f64, s: f64) -> f64 {
    (1.0 - s) * a + s * b
}

#[cfg(test)]
mod tests {
    use objc2_core_foundation::{CGPoint, CGSize};
    use tokio::sync::mpsc::unbounded_channel;

    use super::*;

    fn rect(origin_x: f64, origin_y: f64, width: f64, height: f64) -> CGRect {
        CGRect::new(CGPoint::new(origin_x, origin_y), CGSize::new(width, height))
    }

    fn animation(handle: &AppThreadHandle, wid: WindowId, from: CGRect, to: CGRect) -> Animation {
        let mut animation = Animation::new();
        animation.add_window(handle, wid, from, to, false, TransactionId::default());
        animation
    }

    fn collect_requests(
        rx: &mut mpsc::UnboundedReceiver<(tracing::Span, Request)>,
    ) -> Vec<Request> {
        let mut requests = Vec::new();
        while let Ok((_, request)) = rx.try_recv() {
            requests.push(request);
        }
        requests
    }

    fn assert_set_window_frame(request: &Request, wid: WindowId, frame: CGRect) {
        match request {
            Request::SetWindowFrame(req_wid, req_frame, txid) => {
                assert_eq!(*req_wid, wid);
                assert_eq!(*req_frame, frame);
                assert_eq!(*txid, TransactionId::default());
            }
            _ => panic!("expected SetWindowFrame, got {request:?}"),
        }
    }

    fn assert_set_window_pos(request: &Request, wid: WindowId, pos: CGPoint) {
        match request {
            Request::SetWindowPos(req_wid, req_pos, txid) => {
                assert_eq!(*req_wid, wid);
                assert_eq!(*req_pos, pos);
                assert_eq!(*txid, TransactionId::default());
            }
            _ => panic!("expected SetWindowPos, got {request:?}"),
        }
    }

    #[test]
    fn replacement_uses_last_animated_frame_for_continuing_windows() {
        let (tx, mut rx) = unbounded_channel();
        let handle = AppThreadHandle::new_for_test(tx);
        let wid = WindowId::new(1, 1);
        let first = animation(
            &handle,
            wid,
            rect(0.0, 0.0, 10.0, 10.0),
            rect(50.0, 60.0, 10.0, 10.0),
        );
        let second = animation(
            &handle,
            wid,
            rect(50.0, 60.0, 10.0, 10.0),
            rect(80.0, 90.0, 10.0, 10.0),
        );

        let mut manager = AnimationManager::new();
        manager.handle_message(Message::Replace(first));
        assert!(matches!(
            collect_requests(&mut rx).as_slice(),
            [Request::BeginWindowAnimation(req_wid)] if *req_wid == wid
        ));

        manager.tick();
        let continuing_frame = manager.active.as_ref().unwrap().current_frames()[0].1;
        assert_set_window_pos(&collect_requests(&mut rx)[0], wid, continuing_frame.origin);

        manager.handle_message(Message::Replace(second));
        assert!(collect_requests(&mut rx).is_empty());

        let resumed_start = manager.active.as_ref().unwrap().animation.windows[0].start;
        assert_eq!(resumed_start, continuing_frame);

        manager.tick();
        let expected_next = get_frame(resumed_start, rect(80.0, 90.0, 10.0, 10.0), 1.0 / 30.0);
        assert_set_window_pos(&collect_requests(&mut rx)[0], wid, expected_next.origin);
    }

    #[test]
    fn replacement_only_restarts_changed_windows() {
        let (tx, mut rx) = unbounded_channel();
        let handle = AppThreadHandle::new_for_test(tx);
        let wid1 = WindowId::new(1, 1);
        let wid2 = WindowId::new(1, 2);
        let wid3 = WindowId::new(1, 3);
        let mut first = Animation::new();
        first.add_window(
            &handle,
            wid1,
            rect(0.0, 0.0, 10.0, 10.0),
            rect(50.0, 60.0, 10.0, 10.0),
            false,
            TransactionId::default(),
        );
        first.add_window(
            &handle,
            wid2,
            rect(10.0, 0.0, 10.0, 10.0),
            rect(60.0, 60.0, 10.0, 10.0),
            false,
            TransactionId::default(),
        );
        let mut second = Animation::new();
        second.add_window(
            &handle,
            wid1,
            rect(50.0, 60.0, 10.0, 10.0),
            rect(80.0, 90.0, 10.0, 10.0),
            false,
            TransactionId::default(),
        );
        second.add_window(
            &handle,
            wid3,
            rect(20.0, 0.0, 10.0, 10.0),
            rect(90.0, 90.0, 10.0, 10.0),
            false,
            TransactionId::default(),
        );

        let mut manager = AnimationManager::new();
        manager.handle_message(Message::Replace(first));
        assert_eq!(collect_requests(&mut rx).len(), 2);
        manager.handle_message(Message::Replace(second));

        let requests = collect_requests(&mut rx);
        assert_eq!(requests.len(), 3);
        assert!(matches!(requests[0], Request::BeginWindowAnimation(req_wid) if req_wid == wid3));
        assert_set_window_frame(&requests[1], wid2, rect(60.0, 60.0, 10.0, 10.0));
        assert!(matches!(requests[2], Request::EndWindowAnimation(req_wid) if req_wid == wid2));
    }

    #[test]
    fn skip_to_end_finishes_active_animation_and_applies_new_layout() {
        let (tx, mut rx) = unbounded_channel();
        let handle = AppThreadHandle::new_for_test(tx);
        let wid = WindowId::new(1, 1);
        let first = animation(
            &handle,
            wid,
            rect(0.0, 0.0, 10.0, 10.0),
            rect(50.0, 60.0, 10.0, 10.0),
        );
        let second = animation(
            &handle,
            wid,
            rect(50.0, 60.0, 10.0, 10.0),
            rect(80.0, 90.0, 10.0, 10.0),
        );

        let mut manager = AnimationManager::new();
        manager.handle_message(Message::Replace(first));
        manager.handle_message(Message::SkipToEnd(second));

        let requests = collect_requests(&mut rx);
        assert_eq!(requests.len(), 4);
        assert!(matches!(requests[0], Request::BeginWindowAnimation(req_wid) if req_wid == wid));
        assert_set_window_frame(&requests[1], wid, rect(50.0, 60.0, 10.0, 10.0));
        assert!(matches!(requests[2], Request::EndWindowAnimation(req_wid) if req_wid == wid));
        assert_set_window_frame(&requests[3], wid, rect(80.0, 90.0, 10.0, 10.0));
    }
}
