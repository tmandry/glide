// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Test support for checking window layouts by how windows relate to each
//! other rather than by exact pixels.
//!
//! A [`Scene`] is a screen plus labelled window frames, optionally with a
//! front-to-back order. [`Scene::assert`] checks one relation between windows,
//! and [`Scene::draw`] renders the scene as a box drawing that every failed
//! assertion prints and that tests can compare against an `expect!` snapshot.

use std::fmt;

use objc2_core_foundation::CGRect;

use crate::sys::window_server::WindowId;

/// Allowed difference in pixels between edges or sizes that should match.
const TOLERANCE: f64 = 1.0;
const COLS: f64 = 24.0;
const MIN_COLS: f64 = 8.0;
const MIN_ROWS: f64 = 4.0;
const MAX_ROWS: f64 = 12.0;

const LEFT: u8 = 1;
const RIGHT: u8 = 2;
const UP: u8 = 4;
const DOWN: u8 = 8;
/// Box drawing characters indexed by a mask of `LEFT | RIGHT | UP | DOWN`.
const BOX_CHARS: [char; 16] = [
    ' ', '╴', '╶', '─', '╵', '┘', '└', '┴', '╷', '┐', '┌', '┬', '│', '┤', '├', '┼',
];

/// A name for a window in a [`Scene`].
pub trait Label {
    fn label(&self) -> String;
}

impl Label for &str {
    fn label(&self) -> String {
        self.to_string()
    }
}

impl Label for String {
    fn label(&self) -> String {
        self.clone()
    }
}

impl Label for WindowId {
    /// Name window `n` of pid 1 `a{n}`, of pid 2 `b{n}`, and so on. Windows
    /// identified by a window server id are named `a#{id}`.
    fn label(&self) -> String {
        let app = match u8::try_from(self.pid) {
            Ok(pid @ 1..=26) => char::from(b'a' + pid - 1).to_string(),
            _ => format!("p{}.", self.pid),
        };
        match self.wsid() {
            Some(wsid) => format!("{app}#{}", wsid.as_u32()),
            None => format!("{app}{}", self.manual_index()),
        }
    }
}

/// A screen and the windows on it.
pub struct Scene {
    screen: CGRect,
    windows: Vec<(String, CGRect)>,
    front_to_back: Option<Vec<String>>,
}

impl Scene {
    /// Make a scene of `screen` with no windows.
    pub fn new(screen: CGRect) -> Scene {
        Scene {
            screen,
            windows: Vec::new(),
            front_to_back: None,
        }
    }

    /// Make a scene of `screen` with windows labelled by their [`WindowId`].
    pub fn of(screen: CGRect, windows: impl IntoIterator<Item = (WindowId, CGRect)>) -> Scene {
        windows.into_iter().fold(Scene::new(screen), |scene, (wid, frame)| {
            scene.window(wid, frame)
        })
    }

    /// Add a window.
    pub fn window(mut self, label: impl Label, frame: CGRect) -> Scene {
        self.windows.push((label.label(), frame));
        self
    }

    /// Set the stacking order of the windows, frontmost first.
    ///
    /// Windows left out of the order are behind all the others.
    pub fn front_to_back<L: Label>(mut self, order: impl IntoIterator<Item = L>) -> Scene {
        self.front_to_back = Some(order.into_iter().map(|l| l.label()).collect());
        self
    }

    /// Return the frame of a window.
    #[track_caller]
    pub fn frame(&self, label: impl Label) -> CGRect {
        let label = label.label();
        match self.windows.iter().find(|(l, _)| *l == label) {
            Some(&(_, frame)) => frame,
            None => panic!("no window {label} in scene\n\n{self}"),
        }
    }

    /// Start an assertion about a window.
    #[track_caller]
    pub fn assert(&self, label: impl Label) -> Subject<'_> {
        let label = label.label();
        Subject {
            scene: self,
            frame: self.frame(&*label),
            label,
        }
    }

    /// Render the scene as a box drawing scaled to a small character grid.
    ///
    /// Windows are drawn back to front, each hiding what is behind it. Without
    /// a front-to-back order, later windows are drawn over earlier ones. Lines
    /// below the drawing list the stacking order and the windows the drawing
    /// cannot show.
    pub fn draw(&self) -> String {
        let (cols, rows) = self.grid_size();
        let width = cols + 1;
        let mut lines = vec![0u8; width * (rows + 1)];
        let mut text: Vec<Option<(usize, char)>> = vec![None; lines.len()];
        let mut placed = Vec::new();
        let (mut off_screen, mut too_small, mut hidden) = (vec![], vec![], vec![]);

        stroke(&mut lines, width, [0, 0, cols, rows]);
        for i in self.back_to_front() {
            let (label, frame) = &self.windows[i];
            let Some([x0, y0, x1, y1]) = self.to_grid(*frame, cols, rows) else {
                off_screen.push(label.as_str());
                continue;
            };
            for y in y0..=y1 {
                for x in x0..=x1 {
                    let cell = &mut lines[y * width + x];
                    let (side, top_or_bottom) = (x == x0 || x == x1, y == y0 || y == y1);
                    match (side, top_or_bottom) {
                        (false, false) => *cell = 0,
                        (false, true) => *cell &= if y == y0 { !DOWN } else { !UP },
                        (true, false) => *cell &= if x == x0 { !RIGHT } else { !LEFT },
                        (true, true) => {}
                    }
                    text[y * width + x] = None;
                }
            }
            stroke(&mut lines, width, [x0, y0, x1, y1]);

            let len = label.chars().count();
            let inner = x1 - x0 - 1;
            if y1 - y0 < 2 || inner < len {
                too_small.push(label.as_str());
                continue;
            }
            let start = (y0 + 1) * width + if inner > len { x0 + 2 } else { x0 + 1 };
            for (k, c) in label.chars().enumerate() {
                text[start + k] = Some((i, c));
            }
            placed.push((i, start..start + len));
        }
        for (i, range) in placed {
            let owned = |t: &Option<(usize, char)>| matches!(t, Some((owner, _)) if *owner == i);
            let cells = &mut text[range];
            if !cells.iter().all(owned) {
                cells.iter_mut().filter(|t| owned(t)).for_each(|t| *t = None);
                hidden.push(self.windows[i].0.as_str());
            }
        }

        let mut out = String::new();
        for y in 0..=rows {
            let row: String = (0..width)
                .map(|x| match text[y * width + x] {
                    Some((_, c)) => c,
                    None => BOX_CHARS[usize::from(lines[y * width + x])],
                })
                .collect();
            out.push_str(row.trim_end());
            out.push('\n');
        }
        if let Some(order) = &self.front_to_back {
            out.push_str(&format!("front to back: {}\n", order.join(", ")));
        }
        for (what, labels) in [
            ("off screen", off_screen),
            ("too small to label", too_small),
            ("hidden", hidden),
        ] {
            if !labels.is_empty() {
                out.push_str(&format!("{what}: {}\n", labels.join(", ")));
            }
        }
        out
    }

    fn grid_size(&self) -> (usize, usize) {
        let rows_per_col = self.screen.size.height / self.screen.size.width / 2.0;
        let mut cols = COLS;
        let mut rows = (cols * rows_per_col).round();
        if rows > MAX_ROWS {
            rows = MAX_ROWS;
            cols = (rows / rows_per_col).round().max(MIN_COLS);
        }
        (cols as usize, rows.max(MIN_ROWS) as usize)
    }

    /// Map a frame to `[x0, y0, x1, y1]` grid lines, clipped to the screen and
    /// at least one cell in each direction.
    fn to_grid(&self, frame: CGRect, cols: usize, rows: usize) -> Option<[usize; 4]> {
        let s = self.screen;
        if frame.max().x <= s.min().x
            || frame.min().x >= s.max().x
            || frame.max().y <= s.min().y
            || frame.min().y >= s.max().y
        {
            return None;
        }
        let scale = |v: f64, origin: f64, len: f64, cells: usize| {
            (((v - origin) / len * cells as f64).round().max(0.0) as usize).min(cells)
        };
        let span = |min: f64, max: f64, origin: f64, len: f64, cells: usize| {
            let lo = scale(min, origin, len, cells).min(cells - 1);
            (lo, scale(max, origin, len, cells).max(lo + 1))
        };
        let (x0, x1) = span(frame.min().x, frame.max().x, s.min().x, s.size.width, cols);
        let (y0, y1) = span(frame.min().y, frame.max().y, s.min().y, s.size.height, rows);
        Some([x0, y0, x1, y1])
    }

    fn back_to_front(&self) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.windows.len()).collect();
        if let Some(front_to_back) = &self.front_to_back {
            let depth = |i: &usize| {
                let pos = front_to_back.iter().position(|l| *l == self.windows[*i].0);
                std::cmp::Reverse(pos.unwrap_or(usize::MAX))
            };
            order.sort_by_key(|i| (depth(i), *i));
        }
        order
    }

    fn depth(&self, label: &str) -> Option<usize> {
        self.front_to_back.as_ref()?.iter().position(|l| l == label)
    }
}

/// Add the outline of `[x0, y0, x1, y1]` to a grid of line masks.
fn stroke(lines: &mut [u8], width: usize, [x0, y0, x1, y1]: [usize; 4]) {
    for y in [y0, y1] {
        for x in x0..x1 {
            lines[y * width + x] |= RIGHT;
            lines[y * width + x + 1] |= LEFT;
        }
    }
    for x in [x0, x1] {
        for y in y0..y1 {
            lines[y * width + x] |= DOWN;
            lines[(y + 1) * width + x] |= UP;
        }
    }
}

fn fmt_rect(r: CGRect) -> String {
    format!(
        "({}, {}) {}×{}",
        r.origin.x, r.origin.y, r.size.width, r.size.height
    )
}

impl fmt::Display for Scene {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.draw())?;
        writeln!(f, "screen: {}", fmt_rect(self.screen))?;
        for (label, frame) in &self.windows {
            writeln!(f, "{label}: {}", fmt_rect(*frame))?;
        }
        Ok(())
    }
}

impl fmt::Debug for Scene {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// A window in a [`Scene`] that relations are checked against.
///
/// Each check panics with the scene drawing if it fails, and otherwise returns
/// the subject so checks can be chained. Edges and sizes match if they are
/// within a pixel of each other.
pub struct Subject<'a> {
    scene: &'a Scene,
    label: String,
    frame: CGRect,
}

impl Subject<'_> {
    #[track_caller]
    fn check(&self, ok: bool, relation: fmt::Arguments) -> &Self {
        if !ok {
            panic!("expected {} to be {relation}\n\n{}", self.label, self.scene);
        }
        self
    }

    #[track_caller]
    fn other(&self, other: impl Label) -> (String, CGRect) {
        let other = other.label();
        let frame = self.scene.frame(&*other);
        (other, frame)
    }

    /// Check that the window is entirely left of `other`.
    #[track_caller]
    pub fn left_of(&self, other: impl Label) -> &Self {
        let (o, of) = self.other(other);
        self.check(
            self.frame.max().x <= of.min().x + TOLERANCE,
            format_args!("left of {o}"),
        )
    }

    /// Check that the window is entirely right of `other`.
    #[track_caller]
    pub fn right_of(&self, other: impl Label) -> &Self {
        let (o, of) = self.other(other);
        self.check(
            of.max().x <= self.frame.min().x + TOLERANCE,
            format_args!("right of {o}"),
        )
    }

    /// Check that the window is entirely above `other`.
    #[track_caller]
    pub fn above(&self, other: impl Label) -> &Self {
        let (o, of) = self.other(other);
        self.check(
            self.frame.max().y <= of.min().y + TOLERANCE,
            format_args!("above {o}"),
        )
    }

    /// Check that the window is entirely below `other`.
    #[track_caller]
    pub fn below(&self, other: impl Label) -> &Self {
        let (o, of) = self.other(other);
        self.check(
            of.max().y <= self.frame.min().y + TOLERANCE,
            format_args!("below {o}"),
        )
    }

    /// Check that the window is as wide as `other`.
    #[track_caller]
    pub fn same_width_as(&self, other: impl Label) -> &Self {
        self.width_ratio(other, 1.0)
    }

    /// Check that the window is as tall as `other`.
    #[track_caller]
    pub fn same_height_as(&self, other: impl Label) -> &Self {
        self.height_ratio(other, 1.0)
    }

    /// Check that the window is the same size as `other`.
    #[track_caller]
    pub fn same_size_as(&self, other: impl Label) -> &Self {
        let other = other.label();
        self.same_width_as(&*other).same_height_as(&*other)
    }

    /// Check that the window is `ratio` times as wide as `other`.
    #[track_caller]
    pub fn width_ratio(&self, other: impl Label, ratio: f64) -> &Self {
        let (o, of) = self.other(other);
        self.check(
            (self.frame.size.width - ratio * of.size.width).abs() <= TOLERANCE,
            format_args!("{ratio} times as wide as {o}"),
        )
    }

    /// Check that the window is `ratio` times as tall as `other`.
    #[track_caller]
    pub fn height_ratio(&self, other: impl Label, ratio: f64) -> &Self {
        let (o, of) = self.other(other);
        self.check(
            (self.frame.size.height - ratio * of.size.height).abs() <= TOLERANCE,
            format_args!("{ratio} times as tall as {o}"),
        )
    }

    /// Check that the window covers the whole screen.
    #[track_caller]
    pub fn fills_screen(&self) -> &Self {
        let (f, s) = (self.frame, self.scene.screen);
        let close = |a: f64, b: f64| (a - b).abs() <= TOLERANCE;
        self.check(
            close(f.min().x, s.min().x)
                && close(f.min().y, s.min().y)
                && close(f.max().x, s.max().x)
                && close(f.max().y, s.max().y),
            format_args!("filling the screen"),
        )
    }

    /// Check that the window is in front of `other` in the scene's
    /// front-to-back order.
    #[track_caller]
    pub fn in_front_of(&self, other: impl Label) -> &Self {
        let (o, _) = self.other(other);
        let (depth, other_depth) = (self.scene.depth(&self.label), self.scene.depth(&o));
        self.check(
            matches!((depth, other_depth), (Some(a), Some(b)) if a < b),
            format_args!("in front of {o}"),
        )
    }
}

#[cfg(test)]
mod tests {
    use expect_test::expect;
    use objc2_core_foundation::{CGPoint, CGSize};

    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> CGRect {
        CGRect::new(
            CGPoint::new(f64::from(x), f64::from(y)),
            CGSize::new(f64::from(w), f64::from(h)),
        )
    }

    fn columns() -> Scene {
        Scene::new(rect(0, 0, 1000, 1000))
            .window("a1", rect(0, 0, 500, 1000))
            .window("a2", rect(500, 0, 500, 500))
            .window("a3", rect(500, 500, 500, 500))
    }

    #[test]
    fn draws_shared_borders() {
        expect![[r#"
            ┌───────────┬───────────┐
            │ a1        │ a2        │
            │           │           │
            │           │           │
            │           │           │
            │           │           │
            │           ├───────────┤
            │           │ a3        │
            │           │           │
            │           │           │
            │           │           │
            │           │           │
            └───────────┴───────────┘
        "#]]
        .assert_eq(&columns().draw());
    }

    #[test]
    fn draws_a_portrait_screen_narrower() {
        let scene = Scene::new(rect(0, 0, 100, 300))
            .window("a1", rect(0, 0, 100, 150))
            .window("a2", rect(0, 150, 100, 150));
        expect![[r#"
            ┌───────┐
            │ a1    │
            │       │
            │       │
            │       │
            │       │
            ├───────┤
            │ a2    │
            │       │
            │       │
            │       │
            │       │
            └───────┘
        "#]]
        .assert_eq(&scene.draw());
    }

    #[test]
    fn draws_front_windows_over_back_windows() {
        let scene = Scene::new(rect(0, 0, 1000, 500))
            .window("a1", rect(0, 0, 500, 500))
            .window("a2", rect(500, 0, 500, 500))
            .window("b1", rect(250, 125, 500, 250))
            .window("b2", rect(0, 0, 200, 200))
            .front_to_back(["b1", "a1", "a2", "b2"]);
        expect![[r#"
            ┌───────────┬───────────┐
            │ a1        │ a2        │
            │     ┌─────┴─────┐     │
            │     │ b1        │     │
            │     │           │     │
            │     └─────┬─────┘     │
            └───────────┴───────────┘
            front to back: b1, a1, a2, b2
            hidden: b2
        "#]]
        .assert_eq(&scene.draw());
    }

    #[test]
    fn lists_windows_it_cannot_draw() {
        let scene = Scene::new(rect(0, 0, 1000, 500))
            .window("a1", rect(100, 0, 900, 500))
            .window("a2", rect(0, -2000, 1000, 500))
            .window("a3", rect(900, 400, 10, 10))
            .window("a4", rect(500, 250, 0, 0))
            .window("a5", rect(-500, 0, 600, 500));
        expect![[r#"
            ┌─┬─────────────────────┐
            │ │ a1                  │
            │ │                     │
            │ │         ┌┐          │
            │ │         └┘          │
            │ │                   ┌┐│
            └─┴───────────────────┴┴┘
            off screen: a2
            too small to label: a3, a4, a5
        "#]]
        .assert_eq(&scene.draw());
    }

    #[test]
    fn does_not_panic_on_an_empty_screen() {
        let scene = Scene::new(CGRect::ZERO).window("a1", rect(0, 0, 10, 10));
        let _ = scene.draw();
    }

    #[test]
    fn labels_window_ids_by_app() {
        assert_eq!(WindowId::new(1, 2).label(), "a2");
        assert_eq!(WindowId::new(3, 1).label(), "c1");
        assert_eq!(WindowId::new(30, 4).label(), "p30.4");
        let scene = Scene::of(rect(0, 0, 10, 10), [(WindowId::new(2, 3), rect(0, 0, 5, 5))]);
        assert_eq!(scene.frame("b3"), rect(0, 0, 5, 5));
    }

    #[test]
    fn checks_relations() {
        let scene = columns();
        scene.assert("a1").left_of("a2").left_of("a3").height_ratio("a2", 2.0);
        scene.assert("a2").above("a3").right_of("a1").same_size_as("a3");
        scene.assert("a3").below("a2").width_ratio("a1", 1.0);
        Scene::new(rect(0, 0, 10, 10))
            .window("a1", rect(0, 0, 10, 10))
            .assert("a1")
            .fills_screen();
        Scene::new(rect(0, 0, 10, 10))
            .window("a1", rect(0, 0, 5, 5))
            .window("a2", rect(2, 2, 5, 5))
            .front_to_back(["a2", "a1"])
            .assert("a2")
            .in_front_of("a1");
    }

    #[test]
    fn failure_shows_the_drawing_and_frames() {
        let err = std::panic::catch_unwind(|| {
            columns().assert("a2").left_of("a1");
        })
        .unwrap_err();
        let msg = err.downcast_ref::<String>().unwrap();
        assert!(msg.starts_with("expected a2 to be left of a1\n\n┌"), "{msg}");
        assert!(msg.contains("a2: (500, 0) 500×500\n"), "{msg}");
    }
}
