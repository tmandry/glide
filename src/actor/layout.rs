// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Defines the [`LayoutManager`] actor.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use redact::Secret;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, warn};

use crate::actor::app::{WindowId, pid_t};
use crate::collections::{BTreeExt, BTreeSet, HashMap, HashSet};
use crate::config::{Config, NewWindowPlacement, ScrollConfig, WindowRule, WindowRuleConditions};
use crate::model::scroll_viewport::ViewportState;
use crate::model::{
    ContainerKind, Direction, LayoutId, LayoutKind, LayoutTree, NodeId, Orientation,
    SpaceLayoutMapping,
};
use crate::sys::geometry::{CGRectDef, CGRectExt, CGSizeExt};
use crate::sys::screen::SpaceId;

#[allow(dead_code)]
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub enum LayoutCommand {
    NextLayout,
    PrevLayout,
    MoveFocus(#[serde(rename = "direction")] Direction),
    Ascend,
    Descend,
    MoveNode(Direction),
    Split(Orientation),
    ToggleOrientation,
    Group(Orientation),
    Ungroup,
    ToggleFocusFloating,
    ToggleWindowFloating,
    ToggleFullscreen,
    Resize {
        #[serde(rename = "direction")]
        direction: Direction,
        #[serde(default = "default_resize_percent")]
        percent: f64,
    },
    CycleColumnWidth,
    ChangeLayoutKind,
    ToggleColumnTabbed,
    FocusNext,
    FocusPrev,
}

fn default_resize_percent() -> f64 {
    5.0
}

#[derive(Debug, Clone, PartialEq)]
pub enum LayoutEvent {
    /// Used during restoration to make sure we don't retain windows for
    /// terminated apps.
    AppsRunningUpdated(HashSet<pid_t>),
    AppClosed(pid_t),
    /// Updates the set of windows for a given app and space.
    WindowsOnScreenUpdated(SpaceId, pid_t, Vec<(WindowId, LayoutWindowInfo)>),
    WindowAdded(SpaceId, WindowId, LayoutWindowInfo),
    WindowRemoved(WindowId),
    WindowSpaceChanged {
        wid: WindowId,
        added: Option<SpaceId>,
        removed: Option<SpaceId>,
        info: LayoutWindowInfo,
    },
    WindowFocused(Vec<SpaceId>, WindowId),
    WindowResized {
        wid: WindowId,
        old_frame: CGRect,
        new_frame: CGRect,
        screens: Vec<(SpaceId, CGRect)>,
    },
    /// A user- or app-originated frame change. Floating windows retain this
    /// frame for their next tiled-to-floating transition.
    WindowFrameChanged {
        wid: WindowId,
        frame: CGRect,
    },
    SpaceExposed(SpaceId, CGSize),
    MouseMovedOverWindow {
        over: (SpaceId, WindowId),
        current_main: Option<(SpaceId, WindowId)>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct LayoutWindowInfo {
    /// Frame reported before Glide first tiles this window.
    pub frame: CGRect,
    pub bundle_id: Option<String>,
    pub app_name: Option<String>,
    pub title: Option<Secret<String>>,
    pub layer: Option<i32>,
    pub is_standard: bool,
    pub is_resizable: bool,
    pub ax_role: String,
    pub ax_subrole: Option<String>,
}

#[must_use]
#[derive(Debug, Clone, Default)]
pub struct EventResponse {
    /// One-shot frame targets for transitions such as restoring a floating
    /// window. The reactor merges these into its next animation.
    pub frame_overrides: Vec<(WindowId, CGRect)>,
    /// Windows to raise quietly. No WindowFocused events will be created for
    /// these.
    pub raise_windows: Vec<WindowId>,
    /// Window to focus. This window will be raised after the windows in
    /// raise_windows and a WindowFocused event will be generated.
    pub focus_window: Option<WindowId>,
}

impl EventResponse {
    pub fn coalesce(mut self, other: Self) -> Self {
        self.frame_overrides.extend(other.frame_overrides);
        self.raise_windows.extend(other.raise_windows);
        match (self.focus_window, other.focus_window) {
            (Some(focus_window), Some(other_focus)) => {
                self.focus_window = Some(focus_window);
                self.raise_windows.push(other_focus);
            }
            (None, Some(other_focus)) => {
                self.focus_window = Some(other_focus);
            }
            _ => {}
        }
        self
    }
}

impl LayoutCommand {
    fn modifies_layout(&self) -> bool {
        use LayoutCommand::*;
        match self {
            MoveNode(_)
            | ToggleOrientation
            | Group(_)
            | Ungroup
            | Resize { .. }
            | CycleColumnWidth
            | ToggleColumnTabbed => true,

            NextLayout | PrevLayout | MoveFocus(_) | Ascend | Descend | Split(_)
            | ToggleFocusFloating | ToggleWindowFloating | ToggleFullscreen | ChangeLayoutKind
            | FocusNext | FocusPrev => false,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ResizeEdge(u8);

impl ResizeEdge {
    const LEFT: u8 = 0b0001;
    const RIGHT: u8 = 0b0010;
    const TOP: u8 = 0b0100;
    const BOTTOM: u8 = 0b1000;

    fn has_horizontal(self) -> bool {
        self.0 & (Self::LEFT | Self::RIGHT) != 0
    }

    fn has_vertical(self) -> bool {
        self.0 & (Self::TOP | Self::BOTTOM) != 0
    }

    fn is_empty(self) -> bool {
        self.0 == 0
    }
}

struct InteractiveScrollResize {
    column_node: NodeId,
    window_node: NodeId,
    edges: ResizeEdge,
    last_mouse: CGPoint,
}

struct InteractiveScrollMove {
    layout_id: LayoutId,
    window_id: WindowId,
    window_node: NodeId,
    start_mouse: CGPoint,
    drag_active: bool,
}

const RESIZE_EDGE_THRESHOLD: f64 = 8.0;
const MOVE_DRAG_THRESHOLD: f64 = 10.0;

/// Actor that manages the layouts for each space.
///
/// The LayoutManager is the event-driven layer that sits between the Reactor
/// and the LayoutTree model. This actor receives commands and (cleaned up)
/// events from the Reactor, converts them into LayoutTree operations, and
/// calculates the desired position and size of each window. It also manages
/// floating windows.
///
/// LayoutManager keeps a different layout for each screen size a space is used
/// on. See [`SpaceLayoutInfo`] for more details.
//
// TODO: LayoutManager has too many roles. Consider splitting into a few layers:
//
// * Restoration and new/removed windows/apps.
//   * Convert WindowsOnScreenUpdated events into adds/removes.
// * (Virtual workspaces could go around here.)
// * Floating/tiling split.
// * Tiling layout selection (manual and automatic based on size).
// * Tiling layout-specific commands.
//
// If glide core had a true public API I'd expect it to go after the restoration
// or virtual workspaces layer.
#[derive(Serialize, Deserialize)]
pub struct LayoutManager {
    tree: LayoutTree,
    layout_mapping: HashMap<SpaceId, SpaceLayoutMapping>,
    floating_windows: BTreeSet<WindowId>,
    /// The last user-controlled frame for each floating window. This is kept
    /// after a window is tiled so toggling it back to floating can restore it.
    #[serde(default)]
    floating_restore_frames: HashMap<WindowId, FloatingRestoreFrame>,
    #[serde(skip)]
    active_floating_windows: ActiveFloatingWindows,
    #[serde(skip)]
    focused_window: Option<WindowId>,
    /// Last window focused in floating mode.
    #[serde(skip)]
    // TODO: We should keep a stack for each space.
    last_floating_focus: Option<WindowId>,
    #[serde(skip)]
    viewports: HashMap<LayoutId, ViewportState>,
    #[serde(skip)]
    default_layout_kind: LayoutKind,
    #[serde(skip)]
    scroll_cfg: ScrollConfig,
    #[serde(skip)]
    scroll_enabled: bool,
    #[serde(skip)]
    window_rules: Vec<WindowRule>,
    #[serde(skip)]
    interactive_resize: Option<InteractiveScrollResize>,
    #[serde(skip)]
    interactive_move: Option<InteractiveScrollMove>,
    #[serde(skip, default = "default_config")]
    config: Arc<Config>,
}

#[derive(Serialize, Deserialize)]
struct FloatingRestoreFrame {
    #[serde(with = "CGRectDef")]
    frame: CGRect,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum WindowClass {
    Untracked,
    FloatByDefault,
    Regular,
}

/// Returns true if every specified condition of `rule` matches the window. A
/// rule with no conditions matches every window.
fn window_rule_matches(conditions: &WindowRuleConditions, info: &LayoutWindowInfo) -> bool {
    let title = info.title.as_ref().map(|t| t.expose_secret().as_str());
    let eq = |pattern: Option<&str>, value: Option<&str>| {
        pattern.map_or(true, |p| value.is_some_and(|v| v.eq_ignore_ascii_case(p)))
    };
    let contains = |needle: Option<&str>, haystack: Option<&str>| {
        needle.map_or(true, |n| {
            haystack.is_some_and(|v| v.to_lowercase().contains(&n.to_lowercase()))
        })
    };
    let matches = |re: Option<&regex::Regex>, value: Option<&str>| {
        re.map_or(true, |re| value.is_some_and(|v| re.is_match(v)))
    };
    eq(conditions.app_id.as_deref(), info.bundle_id.as_deref())
        && contains(conditions.app_name.as_deref(), info.app_name.as_deref())
        && matches(conditions.title_regex.as_deref(), title)
        && contains(conditions.title_substring.as_deref(), title)
        && eq(conditions.ax_role.as_deref(), Some(info.ax_role.as_str()))
        && eq(conditions.ax_subrole.as_deref(), info.ax_subrole.as_deref())
}

fn classify_window(rules: &[WindowRule], info: &LayoutWindowInfo) -> WindowClass {
    use LayoutWindowInfo as Info;

    // Phantom/non-window cases are handled first and can't be overridden by app
    // rules, since floating or tiling a window that doesn't really exist makes
    // no sense.
    match info {
        &Info { layer: Some(layer), .. } if layer != 0 => return WindowClass::Untracked,

        // Finder reports a nonstandard window that doesn't actually "exist".
        // In general windows with no layer info are suspect, since it means
        // we couldn't find a corresponding window server window, but we try
        // not to lean on this too much since it depends on a private API.
        Info {
            layer: None,
            is_standard: false,
            bundle_id: Some(bundle_id),
            ..
        } if bundle_id == "com.apple.finder" => return WindowClass::Untracked,

        // Firefox picture-in-picture windows sometimes get observed at layer 0
        // after they are created, even though the layer is later changed to 3.
        // We don't have an event source for the layer change so special case
        // them here. #154
        Info {
            title: Some(title),
            bundle_id: Some(bundle_id),
            ..
        } if bundle_id == "org.mozilla.firefox"
            && title.expose_secret() == "Picture-in-Picture" =>
        {
            return WindowClass::Untracked;
        }

        _ => {}
    }

    // The first matching user rule overrides the built-in heuristics below.
    if let Some(rule) = rules.iter().find(|rule| window_rule_matches(&rule.conditions, info)) {
        return if rule.float {
            WindowClass::FloatByDefault
        } else {
            WindowClass::Regular
        };
    }

    match info {
        Info { is_standard: false, .. } => WindowClass::FloatByDefault,
        Info { is_resizable: false, .. } => WindowClass::FloatByDefault,

        // Float system preferences windows, since they don't resize horiztonally.
        Info { bundle_id: Some(bundle_id), .. } if bundle_id == "com.apple.systempreferences" => {
            WindowClass::FloatByDefault
        }

        _ => WindowClass::Regular,
    }
}

fn default_config() -> Arc<Config> {
    Arc::new(Config::default())
}

impl LayoutManager {
    pub fn new(config: Arc<Config>) -> Self {
        LayoutManager {
            tree: LayoutTree::new(),
            layout_mapping: Default::default(),
            floating_windows: Default::default(),
            floating_restore_frames: Default::default(),
            active_floating_windows: Default::default(),
            focused_window: None,
            last_floating_focus: None,
            viewports: Default::default(),
            default_layout_kind: LayoutKind::default(),
            scroll_cfg: Config::default().settings.experimental.scroll.validated(),
            scroll_enabled: false,
            window_rules: Vec::new(),
            interactive_resize: None,
            interactive_move: None,
            config,
        }
    }

    pub fn set_config(&mut self, config: &Arc<Config>) {
        // TODO: read these through self.config instead of cloning them out
        self.config = config.clone();
        self.scroll_cfg = config.settings.experimental.scroll.clone().validated();
        self.scroll_enabled = self.scroll_cfg.enable;
        self.window_rules = config.window_rules.clone();
        self.default_layout_kind = match (self.scroll_enabled, config.settings.default_layout_kind)
        {
            (false, LayoutKind::Scroll) => {
                warn!(
                    "Ignoring default_layout_kind=scroll because experimental.scroll.enable=false"
                );
                LayoutKind::Tree
            }
            (_, kind) => kind,
        };
        if !self.scroll_enabled {
            self.convert_active_scroll_layouts_to_tree();
        }
    }

    fn convert_active_scroll_layouts_to_tree(&mut self) {
        for space in self.layout_mapping.keys().copied().collect::<Vec<_>>() {
            self.ensure_layout_kind_allowed_for_space(space);
        }
    }

    fn ensure_layout_kind_allowed_for_space(&mut self, space: SpaceId) {
        if self.scroll_enabled {
            return;
        }
        let Some(layout) = self.try_layout(space) else { return };
        if !self.tree.is_scroll_layout(layout) {
            return;
        }
        debug!(
            ?space,
            "Converting scroll layout to tree because scroll gate is disabled"
        );
        let new_layout = Self::convert_layout_kind(
            &mut self.tree,
            &self.scroll_cfg,
            self.focused_window,
            layout,
            LayoutKind::Tree,
        );
        if let Some(mapping) = self.layout_mapping.get_mut(&space)
            && mapping.active_layout() == layout
        {
            mapping.replace_active_layout(new_layout);
        }
        self.viewports.remove(&layout);
    }

    fn convert_layout_kind(
        tree: &mut LayoutTree,
        scroll_cfg: &ScrollConfig,
        focused_window: Option<WindowId>,
        layout: LayoutId,
        new_kind: LayoutKind,
    ) -> LayoutId {
        if tree.layout_kind(layout) == new_kind {
            return layout;
        }

        let selected_window = tree.window_at(tree.selection(layout));
        let windows: Vec<WindowId> = tree
            .root(layout)
            .traverse_postorder(tree.map())
            .filter_map(|n| tree.window_at(n))
            .collect();

        let new_layout = match new_kind {
            LayoutKind::Tree => tree.create_layout(),
            LayoutKind::Scroll => tree.create_scroll_layout(),
        };

        let visible_columns = scroll_cfg.visible_columns;
        for wid in windows {
            tree.remove_window(wid);
            if new_kind == LayoutKind::Scroll {
                tree.add_window_to_scroll_column_with_visible(
                    new_layout,
                    wid,
                    true,
                    visible_columns,
                );
            } else {
                let sel = tree.selection(new_layout);
                tree.add_window_after(new_layout, sel, wid);
            }
        }

        if let Some(wid) = focused_window.or(selected_window)
            && let Some(node) = tree.window_node(new_layout, wid)
        {
            tree.select(node);
        }

        new_layout
    }

    fn change_layout_index_filtered(
        mapping: &mut SpaceLayoutMapping,
        tree: &LayoutTree,
        offset: i16,
        allow_scroll: bool,
    ) -> LayoutId {
        let layouts: Vec<_> = mapping.layouts().collect();
        let len = layouts.len();
        if len <= 1 {
            return mapping.active_layout();
        }
        let cur_idx = mapping.active_layout_index() as i16;
        for step in 1..=len {
            let idx = (cur_idx + offset * step as i16).rem_euclid(len as i16) as usize;
            let candidate = layouts[idx];
            if allow_scroll || !tree.is_scroll_layout(candidate) {
                mapping.select_layout(candidate);
                return candidate;
            }
        }
        mapping.active_layout()
    }

    pub fn debug_tree(&self, space: SpaceId) {
        self.debug_tree_desc(space, "", false);
    }

    pub fn debug_tree_desc(&self, space: SpaceId, desc: &'static str, print: bool) {
        macro_rules! log {
            ($print:expr, $($args:tt)*) => {
                if $print {
                    tracing::warn!($($args)*);
                } else {
                    tracing::debug!($($args)*);
                }
            };
        }
        if let Some(layout) = self.try_layout(space) {
            log!(print, "Tree {desc}\n{}", self.tree.draw_tree(layout).trim());
        } else {
            log!(print, "No layout for space {space:?}");
        }
        let floating = self.active_floating_windows.in_space(space).collect::<Vec<_>>();
        if !floating.is_empty() {
            log!(print, "Floating {floating:?}");
        }
    }

    pub fn handle_event(&mut self, event: LayoutEvent) -> EventResponse {
        debug!(?event);
        match event {
            LayoutEvent::SpaceExposed(space, size) => {
                self.debug_tree(space);
                let kind = self.default_layout_kind;
                {
                    let mapping = self
                        .layout_mapping
                        .entry(space)
                        .or_insert_with(|| SpaceLayoutMapping::new(size, &mut self.tree, kind));
                    mapping.activate_size(size, &mut self.tree);
                }
                self.ensure_layout_kind_allowed_for_space(space);
                return EventResponse {
                    frame_overrides: vec![],
                    raise_windows: self.top_layer_windows(space),
                    focus_window: None,
                };
            }
            LayoutEvent::WindowsOnScreenUpdated(space, pid, windows) => {
                self.debug_tree(space);
                // The windows may already be in the layout if we restored a saved state, so
                // make sure not to duplicate or erase them here.
                for (wid, info) in &windows {
                    self.floating_restore_frames
                        .entry(*wid)
                        .or_insert(FloatingRestoreFrame { frame: info.frame });
                }
                let window_map = windows.iter().cloned().collect::<HashMap<_, _>>();
                self.last_floating_focus
                    .take_if(|f| f.pid == pid && !window_map.contains_key(f));
                let layout = self.layout(space);
                let floating_active = self.active_floating_windows.reset_app(space, pid);
                let mut add_floating = Vec::new();
                let mut new_windows = Vec::new();
                let tree_windows = windows
                    .iter()
                    .map(|(wid, _info)| *wid)
                    .filter(|wid| {
                        let floating = self.floating_windows.contains(wid);
                        if floating {
                            floating_active.insert(*wid);
                            return false;
                        }
                        if self.tree.window_node(layout, *wid).is_some() {
                            return true;
                        }
                        match classify_window(&self.window_rules, window_map.get(wid).unwrap()) {
                            WindowClass::Untracked => false,
                            WindowClass::FloatByDefault => {
                                add_floating.push(*wid);
                                false
                            }
                            WindowClass::Regular => {
                                if self.tree.is_scroll_layout(layout) {
                                    new_windows.push(*wid);
                                    false
                                } else {
                                    true
                                }
                            }
                        }
                    })
                    .collect();
                self.tree.set_windows_for_app(self.layout(space), pid, tree_windows);
                for wid in new_windows {
                    self.add_scroll_window(layout, wid);
                }
                for wid in add_floating {
                    self.add_floating_window(wid, Some(space));
                }
            }
            LayoutEvent::AppsRunningUpdated(hash_set) => {
                self.tree.retain_apps(|pid| hash_set.contains(&pid));
                self.floating_restore_frames.retain(|wid, _| hash_set.contains(&wid.pid));
            }
            LayoutEvent::AppClosed(pid) => {
                self.tree.remove_windows_for_app(pid);
                self.floating_windows.remove_all_for_pid(pid);
                self.floating_restore_frames.retain(|wid, _| wid.pid != pid);
            }
            LayoutEvent::WindowAdded(space, wid, info) => {
                self.debug_tree(space);
                self.floating_restore_frames
                    .entry(wid)
                    .or_insert(FloatingRestoreFrame { frame: info.frame });
                match classify_window(&self.window_rules, &info) {
                    WindowClass::FloatByDefault => self.add_floating_window(wid, Some(space)),
                    WindowClass::Regular => {
                        let layout = self.layout(space);
                        if self.tree.is_scroll_layout(layout) {
                            self.add_scroll_window(layout, wid);
                        } else {
                            self.tree.add_window_after(layout, self.tree.selection(layout), wid);
                        }
                    }
                    WindowClass::Untracked => (),
                }
            }
            LayoutEvent::WindowRemoved(wid) => {
                self.tree.remove_window(wid);
                self.floating_windows.remove(&wid);
                self.floating_restore_frames.remove(&wid);
            }
            LayoutEvent::WindowFrameChanged { wid, frame } => {
                if self.floating_windows.contains(&wid) {
                    self.floating_restore_frames.insert(wid, FloatingRestoreFrame { frame });
                }
            }
            LayoutEvent::WindowSpaceChanged { wid, added, removed, info } => {
                if self.floating_windows.contains(&wid) {
                    // Floating windows live outside the tree, tracked per space
                    // in active_floating_windows.
                    if let Some(added) = added {
                        self.add_floating_window(wid, Some(added));
                    }
                    if let Some(removed) = removed {
                        self.active_floating_windows.remove(removed, wid);
                    }
                } else {
                    if let Some(added) = added {
                        let class = if self.tree.has_window(wid) {
                            // A window we already tile keeps its placement.
                            WindowClass::Regular
                        } else {
                            // We may not have seen this window before, which can
                            // happen if it was off screen before.
                            classify_window(&self.window_rules, &info)
                        };
                        match class {
                            WindowClass::Untracked => (),
                            WindowClass::FloatByDefault => {
                                self.add_floating_window(wid, Some(added))
                            }
                            WindowClass::Regular => {
                                let layout = self.layout(added);
                                if self.tree.is_scroll_layout(layout) {
                                    self.add_scroll_window(layout, wid);
                                } else {
                                    self.tree.add_window_after(
                                        layout,
                                        self.tree.selection(layout),
                                        wid,
                                    );
                                }
                            }
                        }
                    }
                    if let Some(removed) = removed {
                        self.tree.remove_window_from(self.layout(removed), wid);
                    }
                }
            }
            LayoutEvent::WindowFocused(spaces, wid) => {
                self.focused_window = Some(wid);
                if self.floating_windows.contains(&wid) {
                    self.last_floating_focus = Some(wid);
                } else {
                    for space in &spaces {
                        self.clear_user_scrolling(*space);
                    }
                    for space in spaces {
                        let layout = self.layout(space);
                        if let Some(node) = self.tree.window_node(layout, wid) {
                            self.tree.select(node);
                        }
                    }
                }
            }
            LayoutEvent::WindowResized {
                wid,
                old_frame,
                new_frame,
                screens,
            } => {
                for (space, screen) in screens {
                    let layout = self.layout(space);
                    let Some(node) = self.tree.window_node(layout, wid) else {
                        continue;
                    };
                    if !screen.size.contains(old_frame.size)
                        || !screen.size.contains(new_frame.size)
                    {
                        // Ignore resizes involving sizes outside the normal
                        // screen bounds. This can happen for instance if the
                        // window becomes fullscreen at the system level.
                        debug!("Ignoring out-of-bounds resize");
                        continue;
                    }
                    if new_frame == screen {
                        // Usually this happens because the user double-clicked
                        // the title bar.
                        self.tree.set_fullscreen(node, true);
                    } else if self.tree.is_fullscreen(node) {
                        // Either the user double-clicked the window to restore
                        // it from full-size, or they are in an interactive
                        // resize. In both cases we should ignore the old_frame
                        // because it does not reflect the layout size in the
                        // tree (fullscreen overrides that). In the interactive
                        // case clearing the fullscreen bit will cause us to
                        // resize the window to our expected restore size, and
                        // the next resize event we see from the user will
                        // correctly use that as the old_frame.
                        self.tree.set_fullscreen(node, false);
                    } else {
                        // n.b.: old_frame should reflect the current size in
                        // the layout tree so it can be accurately updated.
                        self.tree.set_frame_from_resize(
                            node,
                            old_frame,
                            new_frame,
                            screen,
                            &self.config,
                        );
                    }
                }
            }
            LayoutEvent::MouseMovedOverWindow {
                over: (new_space, new_wid),
                current_main,
            } => {
                if let Some((cur_space, cur_wid)) = current_main
                    // If either window isn't in the layout at all, ignore. Only
                    // follow the mouse between tiled windows.
                    && let Some(_) = self.tree.window_node(self.layout(cur_space), cur_wid)
                    && let Some(new_node) =
                        self.tree.window_node(self.layout(new_space), new_wid)
                    // Don't follow the mouse to windows that aren't visible
                    // according to the layout. This can happen if there are gaps
                    // between windows or the occluding windows have different
                    // border shapes.
                    && self.tree.is_visible(new_node)
                {
                    return EventResponse {
                        frame_overrides: vec![],
                        raise_windows: vec![],
                        focus_window: Some(new_wid),
                    };
                }
            }
        }
        EventResponse::default()
    }

    pub fn handle_command(
        &mut self,
        space: Option<SpaceId>,
        visible_spaces: &[SpaceId],
        command: LayoutCommand,
    ) -> EventResponse {
        if let Some(space) = space {
            let layout = self.layout(space);
            debug!("Tree:\n{}", self.tree.draw_tree(layout).trim());
            debug!(selection = ?self.tree.selection(layout));
        }
        let is_floating = self.is_floating();
        debug!(?self.floating_windows);
        debug!(?self.focused_window, ?self.last_floating_focus, ?is_floating);

        if !self.scroll_enabled
            && matches!(
                command,
                LayoutCommand::CycleColumnWidth
                    | LayoutCommand::ToggleColumnTabbed
                    | LayoutCommand::ChangeLayoutKind
            )
        {
            warn!("Ignoring {command:?} because scroll layout is disabled");
            return EventResponse::default();
        }

        // ToggleWindowFloating is the only command that works when the space is
        // disabled.
        if let LayoutCommand::ToggleWindowFloating = &command {
            let Some(wid) = self.focused_window else {
                return EventResponse::default();
            };
            if is_floating {
                self.remove_floating_window(wid, space);
                self.last_floating_focus = None;
            } else {
                self.add_floating_window(wid, space);
                self.tree.remove_window(wid);
                self.last_floating_focus = Some(wid);
                return EventResponse {
                    frame_overrides: self
                        .floating_restore_frames
                        .get(&wid)
                        .map(|restore| vec![(wid, restore.frame)])
                        .unwrap_or_default(),
                    ..Default::default()
                };
            }
            return EventResponse::default();
        }

        let Some(space) = space else {
            return EventResponse::default();
        };
        let Some(mapping) = self.layout_mapping.get_mut(&space) else {
            error!(
                ?command, ?self.layout_mapping,
                "Could not find layout mapping for current space");
            return EventResponse::default();
        };
        if command.modifies_layout() {
            mapping.prepare_modify(&mut self.tree);
        }
        let layout = mapping.active_layout();

        if let LayoutCommand::ToggleFocusFloating = &command {
            if is_floating {
                let selection = self.tree.window_at(self.tree.selection(layout));
                let mut raise_windows = self.tree.visible_windows_under(self.tree.root(layout));
                // We need to focus some window to transition into floating
                // mode. If there is no selection, pick a window.
                let focus_window = selection.or_else(|| raise_windows.pop());
                return EventResponse {
                    frame_overrides: vec![],
                    raise_windows,
                    focus_window,
                };
            } else {
                let mut raise_windows: Vec<_> = self
                    .active_floating_windows
                    .in_space(space)
                    .filter(|&wid| Some(wid) != self.last_floating_focus)
                    .collect();
                // We need to focus some window to transition into floating
                // mode. If there is no last floating window, pick one.
                let focus_window = self.last_floating_focus.or_else(|| raise_windows.pop());
                return EventResponse {
                    frame_overrides: vec![],
                    raise_windows,
                    focus_window,
                };
            }
        }

        // Remaining commands only work for tiling layout.
        if is_floating {
            return EventResponse::default();
        }

        let next_space = |direction| {
            // Pick another space based on the order in visible_spaces.
            if visible_spaces.len() <= 1 {
                return None;
            }
            let idx = visible_spaces.iter().enumerate().find(|(_, s)| **s == space)?.0;
            let idx = match direction {
                Direction::Left | Direction::Up => idx as i32 - 1,
                Direction::Right | Direction::Down => idx as i32 + 1,
            };
            let idx = idx.rem_euclid(visible_spaces.len() as i32);
            Some(visible_spaces[idx as usize])
        };

        match command {
            // Handled above.
            LayoutCommand::ToggleWindowFloating => unreachable!(),
            LayoutCommand::ToggleFocusFloating => unreachable!(),

            LayoutCommand::NextLayout => {
                // FIXME: Update windows in the new layout.
                let layout =
                    Self::change_layout_index_filtered(mapping, &self.tree, 1, self.scroll_enabled);
                if let Some(wid) = self.focused_window
                    && let Some(node) = self.tree.window_node(layout, wid)
                {
                    self.tree.select(node);
                }
                EventResponse::default()
            }
            LayoutCommand::PrevLayout => {
                // FIXME: Update windows in the new layout.
                let layout = Self::change_layout_index_filtered(
                    mapping,
                    &self.tree,
                    -1,
                    self.scroll_enabled,
                );
                if let Some(wid) = self.focused_window
                    && let Some(node) = self.tree.window_node(layout, wid)
                {
                    self.tree.select(node);
                }
                EventResponse::default()
            }
            LayoutCommand::MoveFocus(direction) => {
                let is_scroll = self.tree.is_scroll_layout(layout);
                let use_wrapping = self.scroll_enabled
                    && is_scroll
                    && self.scroll_config().infinite_loop
                    && matches!(direction, Direction::Left | Direction::Right);
                let new_focus = if use_wrapping {
                    self.tree.traverse_scroll_wrapping(
                        layout,
                        self.tree.selection(layout),
                        direction,
                    )
                } else {
                    self.tree.traverse(self.tree.selection(layout), direction)
                }
                .or_else(|| {
                    let layout = self.layout(next_space(direction)?);
                    Some(self.tree.selection(layout))
                });
                if new_focus.is_some() && is_scroll {
                    self.clear_user_scrolling(space);
                }
                let focus_window = new_focus.and_then(|new| self.tree.window_at(new));
                let raise_windows = new_focus
                    .map(|new| self.tree.select_returning_surfaced_windows(new))
                    .unwrap_or_default();
                EventResponse {
                    frame_overrides: vec![],
                    focus_window,
                    raise_windows,
                }
            }
            LayoutCommand::FocusNext => {
                let new_focus = self.tree.focus_next(layout, self.tree.selection(layout));
                let focus_window = new_focus.and_then(|new| self.tree.window_at(new));
                let raise_windows = new_focus
                    .map(|new| self.tree.select_returning_surfaced_windows(new))
                    .unwrap_or_default();
                EventResponse {
                    frame_overrides: vec![],
                    focus_window,
                    raise_windows,
                }
            }
            LayoutCommand::FocusPrev => {
                let new_focus = self.tree.focus_prev(layout, self.tree.selection(layout));
                let focus_window = new_focus.and_then(|new| self.tree.window_at(new));
                let raise_windows = new_focus
                    .map(|new| self.tree.select_returning_surfaced_windows(new))
                    .unwrap_or_default();
                EventResponse {
                    frame_overrides: vec![],
                    focus_window,
                    raise_windows,
                }
            }
            LayoutCommand::Ascend => {
                self.tree.ascend_selection(layout);
                EventResponse::default()
            }
            LayoutCommand::Descend => {
                self.tree.descend_selection(layout);
                EventResponse::default()
            }
            LayoutCommand::MoveNode(direction) => {
                let selection = self.tree.selection(layout);
                if !self.tree.move_node(layout, selection, direction) {
                    if let Some(new_space) = next_space(direction) {
                        let new_layout = self.layout(new_space);
                        self.tree.move_node_after(self.tree.selection(new_layout), selection);
                    }
                }
                EventResponse::default()
            }
            LayoutCommand::Split(orientation) => {
                // Don't mark as written yet, since merely splitting doesn't
                // usually have a visible effect.
                let selection = self.tree.selection(layout);
                self.tree.nest_in_container(layout, selection, ContainerKind::from(orientation));
                EventResponse::default()
            }
            LayoutCommand::ToggleOrientation => {
                if let Some(parent) = self.tree.selection(layout).parent(self.tree.map()) {
                    let kind = self.tree.container_kind(parent);
                    self.tree.set_container_kind(parent, kind.flip());
                }
                EventResponse::default()
            }
            LayoutCommand::Group(orientation) => {
                if let Some(parent) = self.tree.selection(layout).parent(self.tree.map()) {
                    self.tree.set_container_kind(parent, ContainerKind::group(orientation));
                }
                EventResponse::default()
            }
            LayoutCommand::Ungroup => {
                if let Some(parent) = self.tree.selection(layout).parent(self.tree.map()) {
                    if self.tree.container_kind(parent).is_group() {
                        self.tree.set_container_kind(
                            parent,
                            self.tree.last_ungrouped_container_kind(parent),
                        )
                    }
                }
                EventResponse::default()
            }
            LayoutCommand::ToggleFullscreen => {
                // We don't consider this a structural change so don't save the
                // layout.
                let node = self.tree.selection(layout);
                if self.tree.toggle_fullscreen(node) {
                    // If we have multiple windows in the newly fullscreen node,
                    // make sure they are on top.
                    let node_windows = node
                        .traverse_preorder(self.tree.map())
                        .flat_map(|n| self.tree.window_at(n))
                        .collect();
                    EventResponse {
                        frame_overrides: vec![],
                        raise_windows: node_windows,
                        focus_window: None,
                    }
                } else {
                    EventResponse::default()
                }
            }
            LayoutCommand::Resize { direction, percent } => {
                let percent = percent.clamp(-100.0, 100.0);
                let node = self.tree.selection(layout);
                self.tree.resize(node, percent / 100.0, direction);
                EventResponse::default()
            }
            LayoutCommand::CycleColumnWidth => {
                if !self.tree.is_scroll_layout(layout) {
                    return EventResponse::default();
                }
                let presets = &self.scroll_config().column_width_presets;
                if presets.is_empty() {
                    return EventResponse::default();
                }
                let selection = self.tree.selection(layout);
                if let Some(col) = self.tree.column_of(layout, selection) {
                    let current_proportion = self.tree.proportion(col).unwrap_or(1.0);
                    let next = presets
                        .iter()
                        .find(|&&p| p > current_proportion + 0.01)
                        .or(presets.first())
                        .copied()
                        .unwrap_or(current_proportion);
                    let delta = next - current_proportion;
                    if delta.abs() > 0.001 {
                        self.tree.resize(col, delta, Direction::Right);
                    }
                }
                EventResponse::default()
            }
            LayoutCommand::ToggleColumnTabbed => {
                if !self.tree.is_scroll_layout(layout) {
                    return EventResponse::default();
                }
                let selection = self.tree.selection(layout);
                if let Some(col) = self.tree.column_of(layout, selection) {
                    let new_kind = match self.tree.container_kind(col) {
                        ContainerKind::Vertical => ContainerKind::Tabbed,
                        ContainerKind::Tabbed => ContainerKind::Vertical,
                        other => other,
                    };
                    self.tree.set_container_kind(col, new_kind);
                }
                EventResponse::default()
            }
            LayoutCommand::ChangeLayoutKind => {
                let old_kind = self.tree.layout_kind(layout);
                let new_kind = match old_kind {
                    LayoutKind::Tree => LayoutKind::Scroll,
                    LayoutKind::Scroll => LayoutKind::Tree,
                };
                let new_layout = Self::convert_layout_kind(
                    &mut self.tree,
                    &self.scroll_cfg,
                    self.focused_window,
                    layout,
                    new_kind,
                );
                mapping.replace_active_layout(new_layout);
                self.viewports.remove(&layout);
                EventResponse::default()
            }
        }
    }
}

impl LayoutManager {
    fn is_floating(&self) -> bool {
        if let Some(focus) = self.focused_window {
            self.floating_windows.contains(&focus)
        } else {
            false
        }
    }

    fn add_floating_window(&mut self, wid: WindowId, space: Option<SpaceId>) {
        if let Some(space) = space {
            self.active_floating_windows.insert(space, wid);
        }
        self.floating_windows.insert(wid);
    }

    fn remove_floating_window(&mut self, wid: WindowId, space: Option<SpaceId>) {
        if let Some(space) = space {
            let layout = self.layout(space);
            let selection = self.tree.selection(layout);
            let node = self.tree.add_window_after(layout, selection, wid);
            self.tree.select(node);
            self.active_floating_windows.remove(space, wid);
        }
        self.floating_windows.remove(&wid);
    }
}

/// Tracks which floating windows are present on each space.
#[derive(Default)]
struct ActiveFloatingWindows {
    by_space: HashMap<SpaceId, HashMap<pid_t, HashSet<WindowId>>>,
}

impl ActiveFloatingWindows {
    fn insert(&mut self, space: SpaceId, wid: WindowId) {
        self.by_space.entry(space).or_default().entry(wid.pid).or_default().insert(wid);
    }

    fn remove(&mut self, space: SpaceId, wid: WindowId) {
        if let Some(by_pid) = self.by_space.get_mut(&space)
            && let Some(wids) = by_pid.get_mut(&wid.pid)
        {
            wids.remove(&wid);
        }
    }

    /// Clears the set of floating windows for one app on one space and returns a
    /// handle for inserting that app's current floating windows.
    fn reset_app(&mut self, space: SpaceId, pid: pid_t) -> &mut HashSet<WindowId> {
        let set = self.by_space.entry(space).or_default().entry(pid).or_default();
        set.clear();
        set
    }

    fn in_space(&self, space: SpaceId) -> impl Iterator<Item = WindowId> + '_ {
        self.by_space
            .get(&space)
            .into_iter()
            .flat_map(|by_pid| by_pid.values().flatten().copied())
    }
}

impl LayoutManager {
    pub fn calculate_layout(
        &self,
        space: SpaceId,
        screen: CGRect,
        config: &Config,
    ) -> Vec<(WindowId, CGRect)> {
        let layout = self.layout(space);
        //debug!("{}", self.tree.draw_tree(space));
        let frames = self.tree.calculate_layout(layout, screen, config);
        if self.scroll_enabled && self.tree.is_scroll_layout(layout) {
            if let Some(vp) = self.viewports.get(&layout) {
                return vp.apply_viewport_to_frames(screen, frames, Instant::now());
            }
        }
        frames
    }

    pub fn calculate_layout_and_groups(
        &self,
        space: SpaceId,
        screen: CGRect,
        config: &Config,
    ) -> (Vec<(WindowId, CGRect)>, Vec<crate::model::GroupBarInfo>) {
        let layout = self.layout(space);
        let (sizes, mut groups) = self.tree.calculate_layout_and_groups(layout, screen, config);
        if self.is_floating() {
            // Make sure group bars don't cover the floating windows.
            for group in &mut groups {
                group.is_on_top = false;
            }
        }
        if self.scroll_enabled && self.tree.is_scroll_layout(layout) {
            if let Some(vp) = self.viewports.get(&layout) {
                let transformed = vp.apply_viewport_to_frames(screen, sizes, Instant::now());
                for group in &mut groups {
                    group.indicator_frame = vp.offset_rect(group.indicator_frame, Instant::now());
                }
                return (transformed, groups);
            }
        }
        (sizes, groups)
    }
}

impl LayoutManager {
    fn scroll_config(&self) -> &ScrollConfig {
        &self.scroll_cfg
    }

    fn add_scroll_window(&mut self, layout: LayoutId, wid: WindowId) {
        let new_column = self.scroll_config().new_window_in_column == NewWindowPlacement::NewColumn;
        self.tree.add_window_to_scroll_column_with_visible(
            layout,
            wid,
            new_column,
            self.scroll_config().visible_columns,
        );
    }

    pub fn viewport(&self, layout: LayoutId) -> Option<&ViewportState> {
        self.viewports.get(&layout)
    }

    pub fn viewport_mut(&mut self, layout: LayoutId, screen_width: f64) -> &mut ViewportState {
        self.viewports.entry(layout).or_insert_with(|| ViewportState::new(screen_width))
    }

    pub fn clear_user_scrolling(&mut self, space: SpaceId) {
        let layout = self.layout(space);
        if let Some(vp) = self.viewports.get_mut(&layout) {
            vp.user_scrolling = false;
        }
    }

    pub fn update_viewport_for_focus(&mut self, space: SpaceId, screen: CGRect, config: &Config) {
        if !self.scroll_enabled {
            return;
        }
        let layout = self.layout(space);
        if !self.tree.is_scroll_layout(layout) {
            return;
        }

        if self.viewport(layout).map_or(false, |vp| vp.user_scrolling) {
            return;
        }

        let frames = self.tree.calculate_layout(layout, screen, config);
        let selection = self.tree.selection(layout);
        let sel_wid = self.tree.window_at(selection);
        let columns = self.tree.columns(layout);
        let col = self.tree.column_of(layout, selection);
        let center_mode = config.settings.experimental.scroll.center_focused_column;
        let gap = config.settings.inner_gap;

        let vp = self.viewport_mut(layout, screen.size.width);
        vp.set_screen_width(screen.size.width);

        if let Some(wid) = sel_wid {
            if let Some((_, frame)) = frames.iter().find(|(w, _)| *w == wid) {
                if let Some(c) = col {
                    let col_idx = columns.iter().position(|&n| n == c).unwrap_or(0);
                    vp.ensure_column_visible(
                        col_idx,
                        frame.origin.x,
                        frame.size.width,
                        center_mode,
                        gap,
                        Instant::now(),
                    );
                }
            }
        }
    }

    pub fn has_active_scroll_animation(&self) -> bool {
        if !self.scroll_enabled {
            return false;
        }
        self.viewports.values().any(|vp| vp.is_animating(Instant::now()))
    }

    pub fn tick_viewports(&mut self) {
        for vp in self.viewports.values_mut() {
            vp.tick(Instant::now());
        }
    }

    pub fn handle_scroll_wheel(
        &mut self,
        space: SpaceId,
        delta_x: f64,
        screen: &CGRect,
        config: &crate::config::ScrollConfig,
    ) -> EventResponse {
        if !self.scroll_enabled {
            return EventResponse::default();
        }
        let layout = self.layout(space);
        if !self.tree.is_scroll_layout(layout) {
            return EventResponse::default();
        }

        let columns = self.tree.columns(layout);
        let col_count = columns.len();
        if col_count == 0 {
            return EventResponse::default();
        }

        let step_threshold = screen.size.width / col_count.min(3) as f64;

        let delta = if config.invert_scroll_direction {
            -delta_x
        } else {
            delta_x
        };
        let scaled_delta = delta * config.scroll_sensitivity;

        let is_discrete = delta_x.abs() < 10.0 && delta_x.fract() == 0.0;
        let (effective_delta, effective_threshold) = if is_discrete {
            (scaled_delta.signum() * step_threshold, step_threshold)
        } else {
            (scaled_delta, step_threshold)
        };

        let vp = self.viewport_mut(layout, screen.size.width);
        vp.set_screen_width(screen.size.width);

        let steps = match vp.accumulate_scroll(effective_delta, effective_threshold) {
            Some(s) => s,
            None => return EventResponse::default(),
        };

        let selection = self.tree.selection(layout);
        let direction = if steps < 0 {
            Direction::Right
        } else {
            Direction::Left
        };
        let abs_steps = steps.unsigned_abs().min(16) as usize;

        let mut current = selection;
        for _ in 0..abs_steps {
            let next = if self.scroll_config().infinite_loop {
                self.tree.traverse_scroll_wrapping(layout, current, direction)
            } else {
                self.tree.traverse(current, direction)
            };
            match next {
                Some(n) => current = n,
                None => break,
            }
        }

        if current == selection {
            return EventResponse::default();
        }

        self.clear_user_scrolling(space);
        let focus_window = self.tree.window_at(current);
        let raise_windows = self.tree.select_returning_surfaced_windows(current);
        EventResponse {
            frame_overrides: vec![],
            focus_window,
            raise_windows,
        }
    }

    pub(crate) fn hit_test_scroll_edges(
        &self,
        space: SpaceId,
        point: CGPoint,
        screen: CGRect,
        config: &Config,
    ) -> Option<(NodeId, NodeId, ResizeEdge)> {
        if !self.scroll_enabled {
            return None;
        }
        let layout = self.try_layout(space)?;
        if !self.tree.is_scroll_layout(layout) {
            return None;
        }
        let frames = self.calculate_layout(space, screen, config);
        for (wid, frame) in &frames {
            let edges = detect_edges(point, *frame);
            if !edges.is_empty() {
                let window_node = self.tree.window_node(layout, *wid)?;
                let column_node = self.tree.column_of(layout, window_node)?;
                return Some((column_node, window_node, edges));
            }
        }
        None
    }

    pub fn hit_test_scroll_window(
        &self,
        space: SpaceId,
        point: CGPoint,
        screen: CGRect,
        config: &Config,
    ) -> Option<(WindowId, NodeId)> {
        if !self.scroll_enabled {
            return None;
        }
        let layout = self.try_layout(space)?;
        if !self.tree.is_scroll_layout(layout) {
            return None;
        }
        let frames = self.calculate_layout(space, screen, config);
        for (wid, frame) in &frames {
            if frame.contains(point) {
                let node = self.tree.window_node(layout, *wid)?;
                return Some((*wid, node));
            }
        }
        None
    }

    pub(crate) fn begin_interactive_resize(
        &mut self,
        column: NodeId,
        window: NodeId,
        edges: ResizeEdge,
        mouse: CGPoint,
    ) -> bool {
        if self.interactive_resize.is_some() {
            return false;
        }
        self.interactive_resize = Some(InteractiveScrollResize {
            column_node: column,
            window_node: window,
            edges,
            last_mouse: mouse,
        });
        true
    }

    pub fn update_interactive_resize(&mut self, mouse: CGPoint, screen: CGRect) -> bool {
        let Some(state) = self.interactive_resize.as_mut() else {
            return false;
        };
        let dx = mouse.x - state.last_mouse.x;
        let dy = mouse.y - state.last_mouse.y;
        state.last_mouse = mouse;

        let mut changed = false;
        if state.edges.has_horizontal() {
            let ratio = dx / screen.size.width;
            let direction = if state.edges.0 & ResizeEdge::LEFT != 0 {
                Direction::Left
            } else {
                Direction::Right
            };
            let col = state.column_node;
            if self.tree.resize(col, ratio, direction) {
                changed = true;
            }
        }
        if state.edges.has_vertical() {
            let ratio = dy / screen.size.height;
            let direction = if state.edges.0 & ResizeEdge::TOP != 0 {
                Direction::Up
            } else {
                Direction::Down
            };
            let win = state.window_node;
            if self.tree.resize(win, ratio, direction) {
                changed = true;
            }
        }
        changed
    }

    pub fn end_interactive_resize(&mut self, space: SpaceId, screen: CGRect, config: &Config) {
        if self.interactive_resize.take().is_some() {
            self.clear_user_scrolling(space);
            self.update_viewport_for_focus(space, screen, config);
        }
    }

    pub fn begin_interactive_move(
        &mut self,
        space: SpaceId,
        wid: WindowId,
        node: NodeId,
        mouse: CGPoint,
    ) -> bool {
        if self.interactive_resize.is_some() || self.interactive_move.is_some() {
            return false;
        }
        let layout_id = self.layout(space);
        self.interactive_move = Some(InteractiveScrollMove {
            layout_id,
            window_id: wid,
            window_node: node,
            start_mouse: mouse,
            drag_active: false,
        });
        true
    }

    pub fn update_interactive_move(
        &mut self,
        mouse: CGPoint,
        screen: CGRect,
        config: &Config,
    ) -> bool {
        let Some(state) = self.interactive_move.as_mut() else {
            return false;
        };
        if !state.drag_active {
            let dx = mouse.x - state.start_mouse.x;
            let dy = mouse.y - state.start_mouse.y;
            if (dx * dx + dy * dy).sqrt() < MOVE_DRAG_THRESHOLD {
                return false;
            }
            state.drag_active = true;
        }
        let source_node = state.window_node;
        let source_wid = state.window_id;
        let layout = state.layout_id;
        let frames = self.tree.calculate_layout(layout, screen, config);
        let vp_opt = self.viewports.get(&layout);

        for (wid, frame) in &frames {
            if *wid == source_wid {
                continue;
            }

            let target_frame;
            if let Some(vp) = vp_opt {
                if !vp.is_visible(*frame, Instant::now()) {
                    continue;
                }
                target_frame = vp.offset_rect(*frame, Instant::now());
            } else {
                target_frame = *frame;
            }

            if target_frame.contains(mouse)
                && let Some(target_node) = self.tree.window_node(layout, *wid)
            {
                self.tree.swap_windows(source_node, target_node);
                if let Some(state) = self.interactive_move.as_mut() {
                    state.window_node = target_node;
                }
                return true;
            }
        }
        false
    }

    pub fn end_interactive_move(&mut self, space: SpaceId, screen: CGRect, config: &Config) {
        if self.interactive_move.take().is_some() {
            self.clear_user_scrolling(space);
            self.update_viewport_for_focus(space, screen, config);
        }
    }

    pub fn cancel_interactive_state(&mut self) {
        self.interactive_resize = None;
        self.interactive_move = None;
    }

    pub fn has_interactive_state(&self) -> bool {
        self.interactive_resize.is_some() || self.interactive_move.is_some()
    }
}

impl LayoutManager {
    fn try_layout(&self, space: SpaceId) -> Option<LayoutId> {
        self.layout_mapping.get(&space)?.active_layout().into()
    }

    fn layout(&self, space: SpaceId) -> LayoutId {
        self.try_layout(space).unwrap()
    }

    fn top_layer_windows(&self, space: SpaceId) -> Vec<WindowId> {
        if self.is_floating() {
            // For now don't mess with window order if a floating window is focused.
            return Vec::new();
        }
        let Some(layout) = self.try_layout(space) else {
            return Vec::new();
        };
        self.tree.visible_windows_under(self.tree.root(layout))
    }

    pub fn load(path: PathBuf, config: Arc<Config>) -> anyhow::Result<Self> {
        let mut buf = String::new();
        File::open(path)?.read_to_string(&mut buf)?;
        let mut manager: Self = ron::from_str(&buf)?;
        // Only the field, not `set_config`: the caller applies the config, and
        // `set_config` also converts layouts the config no longer allows.
        manager.config = config;
        Ok(manager)
    }

    pub fn save(&self, path: PathBuf) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        File::create(path)?.write_all(self.serialize_to_string().as_bytes())?;
        Ok(())
    }

    pub fn serialize_to_string(&self) -> String {
        ron::ser::to_string(&self).unwrap()
    }

    // This seems a bit messy, but it's simpler and more robust to write some
    // reactor tests as integration tests with this actor.
    #[cfg(test)]
    pub(super) fn selected_window(&mut self, space: SpaceId) -> Option<WindowId> {
        let layout = self.layout(space);
        self.tree.window_at(self.tree.selection(layout))
    }

    #[cfg(test)]
    pub(super) fn active_layout_kind(&self, space: SpaceId) -> LayoutKind {
        self.tree.layout_kind(self.layout(space))
    }

    #[cfg(test)]
    pub(super) fn floating_windows_in_space(&self, space: SpaceId) -> BTreeSet<WindowId> {
        self.active_floating_windows.in_space(space).collect()
    }

    #[cfg(test)]
    pub(super) fn floating_restore_frame(&self, wid: WindowId) -> Option<CGRect> {
        self.floating_restore_frames.get(&wid).map(|restore| restore.frame)
    }

    /// Every window present in any layout. Used by the restore snapshot tests to
    /// confirm a deserialized layout retained its window mapping.
    #[cfg(test)]
    pub(crate) fn all_windows(&self) -> std::collections::BTreeSet<WindowId> {
        self.tree
            .layouts()
            .flat_map(|layout| self.tree.visible_windows_under(self.tree.root(layout)))
            .collect()
    }
}

// TODO: detect_edges does not account for screen boundaries.
// A window flush against a screen edge should not offer resize on that edge.
fn detect_edges(point: CGPoint, frame: CGRect) -> ResizeEdge {
    let threshold = RESIZE_EDGE_THRESHOLD;
    let expanded = CGRect::new(
        CGPoint::new(frame.origin.x - threshold, frame.origin.y - threshold),
        CGSize::new(
            frame.size.width + threshold * 2.0,
            frame.size.height + threshold * 2.0,
        ),
    );
    if !expanded.contains(point) {
        return ResizeEdge(0);
    }
    let inner = CGRect::new(
        CGPoint::new(frame.origin.x + threshold, frame.origin.y + threshold),
        CGSize::new(
            (frame.size.width - threshold * 2.0).max(0.0),
            (frame.size.height - threshold * 2.0).max(0.0),
        ),
    );
    if inner.contains(point) {
        return ResizeEdge(0);
    }
    let mut edges = 0u8;
    if point.x < frame.origin.x + threshold {
        edges |= ResizeEdge::LEFT;
    }
    if point.x > frame.origin.x + frame.size.width - threshold {
        edges |= ResizeEdge::RIGHT;
    }
    if point.y < frame.origin.y + threshold {
        edges |= ResizeEdge::TOP;
    }
    if point.y > frame.origin.y + frame.size.height - threshold {
        edges |= ResizeEdge::BOTTOM;
    }
    // If both opposing edges are set (window too small for edge detection),
    // disable that axis to avoid conflicting resize directions.
    if edges & ResizeEdge::LEFT != 0 && edges & ResizeEdge::RIGHT != 0 {
        edges &= !(ResizeEdge::LEFT | ResizeEdge::RIGHT);
    }
    if edges & ResizeEdge::TOP != 0 && edges & ResizeEdge::BOTTOM != 0 {
        edges &= !(ResizeEdge::TOP | ResizeEdge::BOTTOM);
    }
    ResizeEdge(edges)
}

#[cfg(test)]
impl LayoutManager {
    pub(crate) fn new_for_test() -> Self {
        Self::new(default_config())
    }
}

#[cfg(test)]
mod tests {
    use objc2_core_foundation::CGPoint;
    use pretty_assertions::assert_eq;
    use test_log::test;

    use super::*;
    use crate::config::WindowRuleConditions;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> CGRect {
        CGRect::new(CGPoint::new(x as f64, y as f64), CGSize::new(w as f64, h as f64))
    }

    fn make_windows(pid: pid_t, num: u32) -> Vec<(WindowId, LayoutWindowInfo)> {
        (1..=num).map(|idx| (WindowId::new(pid, idx), win_info())).collect()
    }

    fn win_info() -> LayoutWindowInfo {
        LayoutWindowInfo {
            frame: CGRect::ZERO,
            bundle_id: None,
            app_name: None,
            title: None,
            layer: Some(0),
            is_standard: true,
            is_resizable: true,
            ax_role: String::new(),
            ax_subrole: None,
        }
    }

    fn config_with_scroll(enable: bool, default_layout_kind: LayoutKind) -> Arc<Config> {
        let mut config = Config::default();
        config.settings.experimental.scroll.enable = enable;
        config.settings.default_layout_kind = default_layout_kind;
        Arc::new(config)
    }

    #[test]
    fn no_rules_falls_back_to_builtin_heuristics() {
        let mut info = win_info();
        assert_eq!(classify_window(&[], &info), WindowClass::Regular);
        info.is_standard = false;
        assert_eq!(classify_window(&[], &info), WindowClass::FloatByDefault);
    }

    #[test]
    fn app_id_rule_floats_matching_window_only() {
        let rules = [WindowRule {
            conditions: WindowRuleConditions {
                app_id: Some("com.example.X".into()),
                ..Default::default()
            },
            float: true,
        }];

        let mut info = win_info();
        info.bundle_id = Some("com.example.X".into());
        assert_eq!(classify_window(&rules, &info), WindowClass::FloatByDefault);

        info.bundle_id = Some("com.example.Y".into());
        assert_eq!(classify_window(&rules, &info), WindowClass::Regular);
    }

    #[test]
    fn earlier_rule_wins() {
        let rules = [
            WindowRule {
                conditions: WindowRuleConditions {
                    app_id: Some("com.example.X".into()),
                    title_regex: Some("Dialog".parse().unwrap()),
                    ..Default::default()
                },
                float: true,
            },
            WindowRule {
                conditions: WindowRuleConditions {
                    app_id: Some("com.example.X".into()),
                    ..Default::default()
                },
                float: false,
            },
        ];

        let mut info = win_info();
        info.bundle_id = Some("com.example.X".into());
        info.title = Some("My Dialog".to_owned().into());
        assert_eq!(classify_window(&rules, &info), WindowClass::FloatByDefault);

        info.title = Some("Main Window".to_owned().into());
        assert_eq!(classify_window(&rules, &info), WindowClass::Regular);
    }

    #[test]
    fn rule_conditions_combine_with_and() {
        let rules = [WindowRule {
            conditions: WindowRuleConditions {
                app_name: Some("Code".into()),
                title_substring: Some("Settings".into()),
                ax_role: Some("AXWindow".into()),
                ax_subrole: Some("AXDialog".into()),
                ..Default::default()
            },
            float: true,
        }];

        let mut info = win_info();
        info.app_name = Some("Visual Studio Code".into());
        info.title = Some("Settings — main".to_owned().into());
        info.ax_role = "AXWindow".into();
        info.ax_subrole = Some("AXDialog".into());
        assert_eq!(classify_window(&rules, &info), WindowClass::FloatByDefault);

        // One mismatched condition (subrole) means the rule does not apply.
        info.ax_subrole = Some("AXStandardWindow".into());
        assert_eq!(classify_window(&rules, &info), WindowClass::Regular);
    }

    #[test]
    fn rule_conditions_match_case_insensitively() {
        let rules = [WindowRule {
            conditions: WindowRuleConditions {
                app_id: Some("COM.EXAMPLE.x".into()),
                app_name: Some("code".into()),
                title_regex: Some("dialog".parse().unwrap()),
                title_substring: Some("SETTINGS".into()),
                ax_role: Some("axwindow".into()),
                ax_subrole: Some("AXDIALOG".into()),
            },
            float: true,
        }];

        let mut info = win_info();
        info.bundle_id = Some("com.example.X".into());
        info.app_name = Some("Visual Studio Code".into());
        info.title = Some("Settings Dialog".to_owned().into());
        info.ax_role = "AXWindow".into();
        info.ax_subrole = Some("AXDialog".into());
        assert_eq!(classify_window(&rules, &info), WindowClass::FloatByDefault);
    }

    #[test]
    fn condition_does_not_match_when_attribute_is_absent() {
        // A rule that requires an attribute must not match a window that lacks
        // it; the condition should fail rather than be skipped.
        let rules = [WindowRule {
            conditions: WindowRuleConditions {
                app_id: Some("com.example.X".into()),
                ..Default::default()
            },
            float: true,
        }];
        let mut info = win_info();
        info.bundle_id = None;
        assert_eq!(classify_window(&rules, &info), WindowClass::Regular);
    }

    #[test]
    fn rule_overrides_builtin_float_heuristics() {
        // System Preferences floats by default; a rule can force it to tile.
        let rules = [WindowRule {
            conditions: WindowRuleConditions {
                app_id: Some("com.apple.systempreferences".into()),
                ..Default::default()
            },
            float: false,
        }];
        let mut info = win_info();
        info.bundle_id = Some("com.apple.systempreferences".into());
        assert_eq!(classify_window(&rules, &info), WindowClass::Regular);

        // A non-resizable window floats by default; a rule can force it to tile.
        let rules = [WindowRule {
            conditions: WindowRuleConditions {
                app_id: Some("com.example.X".into()),
                ..Default::default()
            },
            float: false,
        }];
        let mut info = win_info();
        info.bundle_id = Some("com.example.X".into());
        info.is_resizable = false;
        assert_eq!(classify_window(&rules, &info), WindowClass::Regular);
    }

    #[test]
    fn rules_cannot_override_untracked_phantom_windows() {
        let rules = [WindowRule {
            conditions: WindowRuleConditions::default(),
            float: true,
        }];
        let mut info = win_info();
        info.layer = Some(3);
        assert_eq!(classify_window(&rules, &info), WindowClass::Untracked);
    }

    impl LayoutManager {
        fn layout_sorted(&self, space: SpaceId, screen: CGRect) -> Vec<(WindowId, CGRect)> {
            let mut layout = self.calculate_layout(
                space,
                screen,
                &Config::default(), // TODO stop being lazy
            );
            layout.sort_by_key(|(wid, _)| *wid);
            layout
        }
    }

    #[test]
    fn it_maintains_separate_layouts_for_each_screen_size() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let pid = 1;
        let windows = make_windows(pid, 3);

        // Set up the starting layout.
        let screen1 = rect(0, 0, 120, 120);
        _ = mgr.handle_event(SpaceExposed(space, screen1.size));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, windows.clone()));
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 1)));
        _ = mgr.handle_command(Some(space), &[space], MoveNode(Direction::Up));
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 120, 60)),
                (WindowId::new(pid, 2), rect(0, 60, 60, 60)),
                (WindowId::new(pid, 3), rect(60, 60, 60, 60)),
            ],
            mgr.layout_sorted(space, screen1),
        );

        // Introduce new screen size.
        let screen2 = rect(0, 0, 1200, 1200);
        _ = mgr.handle_event(SpaceExposed(space, screen2.size));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, windows.clone()));
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 1200, 600)),
                (WindowId::new(pid, 2), rect(0, 600, 600, 600)),
                (WindowId::new(pid, 3), rect(600, 600, 600, 600)),
            ],
            mgr.layout_sorted(space, screen2),
            "layout was not correctly scaled to new screen size"
        );

        // Change the layout for the second screen size.
        _ = mgr.handle_command(Some(space), &[space], MoveNode(Direction::Down));
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 400, 1200)),
                (WindowId::new(pid, 2), rect(400, 0, 400, 1200)),
                (WindowId::new(pid, 3), rect(800, 0, 400, 1200)),
            ],
            mgr.layout_sorted(space, screen2),
        );

        // Switch back to the first size; the layout should be the same as before.
        _ = mgr.handle_event(SpaceExposed(space, screen1.size));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, windows.clone()));
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 120, 60)),
                (WindowId::new(pid, 2), rect(0, 60, 60, 60)),
                (WindowId::new(pid, 3), rect(60, 60, 60, 60)),
            ],
            mgr.layout_sorted(space, screen1),
        );

        // Switch back to the second size.
        _ = mgr.handle_event(SpaceExposed(space, screen2.size));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, windows.clone()));
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 400, 1200)),
                (WindowId::new(pid, 2), rect(400, 0, 400, 1200)),
                (WindowId::new(pid, 3), rect(800, 0, 400, 1200)),
            ],
            mgr.layout_sorted(space, screen2),
        );
    }

    #[test]
    fn it_culls_unmodified_layouts() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let pid = 1;
        let windows = make_windows(pid, 3);

        // Set up the starting layout but do not modify it.
        let screen1 = rect(0, 0, 120, 120);
        _ = mgr.handle_event(SpaceExposed(space, screen1.size));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, windows.clone()));
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 1)));
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 40, 120)),
                (WindowId::new(pid, 2), rect(40, 0, 40, 120)),
                (WindowId::new(pid, 3), rect(80, 0, 40, 120)),
            ],
            mgr.layout_sorted(space, screen1),
        );

        // Introduce new screen size.
        let screen2 = rect(0, 0, 1200, 1200);
        _ = mgr.handle_event(SpaceExposed(space, screen2.size));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, windows.clone()));
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 1)));
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 400, 1200)),
                (WindowId::new(pid, 2), rect(400, 0, 400, 1200)),
                (WindowId::new(pid, 3), rect(800, 0, 400, 1200)),
            ],
            mgr.layout_sorted(space, screen2),
            "layout was not correctly scaled to new screen size"
        );

        // Change the layout for the second screen size.
        _ = mgr.handle_command(Some(space), &[space], MoveNode(Direction::Up));
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 1200, 600)),
                (WindowId::new(pid, 2), rect(0, 600, 600, 600)),
                (WindowId::new(pid, 3), rect(600, 600, 600, 600)),
            ],
            mgr.layout_sorted(space, screen2),
        );

        // Switch back to the first size. We should see a downscaled
        // version of the modified layout.
        _ = mgr.handle_event(SpaceExposed(space, screen1.size));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, windows.clone()));
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 120, 60)),
                (WindowId::new(pid, 2), rect(0, 60, 60, 60)),
                (WindowId::new(pid, 3), rect(60, 60, 60, 60)),
            ],
            mgr.layout_sorted(space, screen1),
        );

        // Switch to a third size. We should see a scaled version of the same.
        let screen3 = rect(0, 0, 12, 12);
        _ = mgr.handle_event(SpaceExposed(space, screen3.size));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, windows.clone()));
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 12, 6)),
                (WindowId::new(pid, 2), rect(0, 6, 6, 6)),
                (WindowId::new(pid, 3), rect(6, 6, 6, 6)),
            ],
            mgr.layout_sorted(space, screen3),
        );

        // Modify the layout.
        _ = mgr.handle_command(Some(space), &[space], MoveNode(Direction::Left));
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 6, 12)),
                (WindowId::new(pid, 2), rect(6, 0, 3, 12)),
                (WindowId::new(pid, 3), rect(9, 0, 3, 12)),
            ],
            mgr.layout_sorted(space, screen3),
        );

        // Switch back to the first size. We should see a scaled
        // version of the newly modified layout.
        _ = mgr.handle_event(SpaceExposed(space, screen1.size));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, windows.clone()));
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 60, 120)),
                (WindowId::new(pid, 2), rect(60, 0, 30, 120)),
                (WindowId::new(pid, 3), rect(90, 0, 30, 120)),
            ],
            mgr.layout_sorted(space, screen1),
        );

        // Modify the layout in the first size.
        _ = mgr.handle_command(Some(space), &[space], MoveNode(Direction::Right));

        // Switch back to the second screen size, then the first, then the
        // second again. Since the layout was modified in the second size, the
        // windows should go back to the way they were laid out then.
        _ = mgr.handle_event(SpaceExposed(space, screen2.size));
        _ = mgr.handle_event(SpaceExposed(space, screen1.size));
        _ = mgr.handle_event(SpaceExposed(space, screen2.size));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, windows.clone()));
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 1200, 600)),
                (WindowId::new(pid, 2), rect(0, 600, 600, 600)),
                (WindowId::new(pid, 3), rect(600, 600, 600, 600)),
            ],
            mgr.layout_sorted(space, screen2),
        );
    }

    #[test]
    fn floating_windows() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let pid = 1;
        let config = &Config::default();

        let screen1 = rect(0, 0, 120, 120);
        _ = mgr.handle_event(SpaceExposed(space, screen1.size));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, make_windows(pid, 3)));

        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 2)));
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 1)));

        // Make the first window float.
        _ = mgr.handle_command(Some(space), &[space], ToggleWindowFloating);
        let sizes: HashMap<_, _> =
            mgr.calculate_layout(space, screen1, config).into_iter().collect();
        assert_eq!(sizes[&WindowId::new(pid, 2)], rect(0, 0, 60, 120));
        assert_eq!(sizes[&WindowId::new(pid, 3)], rect(60, 0, 60, 120));

        // Toggle back to the tiled windows.
        let response = mgr.handle_command(Some(space), &[space], ToggleFocusFloating);
        assert_eq!(
            vec![WindowId::new(pid, 3), WindowId::new(pid, 2)],
            response.raise_windows
        );
        assert_eq!(Some(WindowId::new(pid, 2)), response.focus_window);
        if let Some(focus) = response.focus_window {
            _ = mgr.handle_event(WindowFocused(vec![space], focus));
        }

        // Make the second window float.
        _ = mgr.handle_command(Some(space), &[space], ToggleWindowFloating);
        let sizes: HashMap<_, _> =
            mgr.calculate_layout(space, screen1, config).into_iter().collect();
        assert_eq!(sizes[&WindowId::new(pid, 3)], rect(0, 0, 120, 120));

        // Toggle back to tiled.
        let response = mgr.handle_command(Some(space), &[space], ToggleFocusFloating);
        assert_eq!(vec![WindowId::new(pid, 3)], response.raise_windows);
        assert_eq!(Some(WindowId::new(pid, 3)), response.focus_window);
        if let Some(focus) = response.focus_window {
            _ = mgr.handle_event(WindowFocused(vec![space], focus));
        }

        // Toggle back to floating.
        let response = mgr.handle_command(Some(space), &[space], ToggleFocusFloating);
        assert_eq!(vec![WindowId::new(pid, 1)], response.raise_windows);
        assert_eq!(Some(WindowId::new(pid, 2)), response.focus_window);
        if let Some(focus) = response.focus_window {
            _ = mgr.handle_event(WindowFocused(vec![space], focus));
        }
    }

    #[test]
    fn floating_windows_space_disabled() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let pid = 1;
        let config = &Config::default();

        _ = mgr.handle_event(WindowFocused(vec![], WindowId::new(pid, 1)));

        // Make the first window float.
        _ = mgr.handle_command(None, &[], ToggleWindowFloating);

        // Enable the space.
        let screen1 = rect(0, 0, 120, 120);
        _ = mgr.handle_event(SpaceExposed(space, screen1.size));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, make_windows(pid, 3)));

        let sizes: HashMap<_, _> =
            mgr.calculate_layout(space, screen1, config).into_iter().collect();
        assert_eq!(sizes[&WindowId::new(pid, 2)], rect(0, 0, 60, 120));
        assert_eq!(sizes[&WindowId::new(pid, 3)], rect(60, 0, 60, 120));

        // Toggle back to the tiled windows.
        let response = mgr.handle_command(Some(space), &[space], ToggleFocusFloating);
        let mut raised_windows = response.raise_windows;
        raised_windows.extend(response.focus_window);
        raised_windows.sort();
        assert_eq!(
            vec![WindowId::new(pid, 2), WindowId::new(pid, 3)],
            raised_windows
        );
        // This if let is kind of load bearing for this test: previously we
        // allowed passing None for the window id of this event, except we
        // did that in the test but not in production. This led to an uncaught
        // bug!
        if let Some(focus) = response.focus_window {
            _ = mgr.handle_event(WindowFocused(vec![space], focus));
        }

        // Toggle back to floating.
        let response = mgr.handle_command(Some(space), &[space], ToggleFocusFloating);
        assert!(response.raise_windows.is_empty());
        assert_eq!(Some(WindowId::new(pid, 1)), response.focus_window);
        if let Some(focus) = response.focus_window {
            _ = mgr.handle_event(WindowFocused(vec![space], focus));
        }
    }

    #[test]
    fn space_exposed_does_not_bury_focused_floating_window() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let pid = 1;

        let screen1 = rect(0, 0, 120, 120);
        _ = mgr.handle_event(SpaceExposed(space, screen1.size));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, make_windows(pid, 3)));
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 1)));

        // With a tiled window focused, re-exposing the space (e.g. via mission
        // control) raises the tiled windows on top.
        let response = mgr.handle_event(SpaceExposed(space, screen1.size));
        assert!(!response.raise_windows.is_empty());

        // Float the focused window and re-expose the space. The floating window
        // must not be buried, so no tiled windows are raised over it.
        _ = mgr.handle_command(Some(space), &[space], ToggleWindowFloating);
        let response = mgr.handle_event(SpaceExposed(space, screen1.size));
        assert!(
            response.raise_windows.is_empty()
                || response.raise_windows.contains(&WindowId::new(pid, 1))
        );
    }

    #[test]
    fn it_adds_new_windows_behind_selection() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let pid = 1;
        let windows = make_windows(pid, 5);

        let screen1 = rect(0, 0, 300, 30);
        _ = mgr.handle_event(SpaceExposed(space, screen1.size));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, windows.clone()));
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 5)));
        _ = mgr.handle_command(Some(space), &[space], ToggleWindowFloating);
        _ = mgr.handle_command(Some(space), &[space], ToggleFocusFloating);
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 2)));
        _ = mgr.handle_command(Some(space), &[space], Split(Orientation::Vertical));
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 3)));
        _ = mgr.handle_command(Some(space), &[space], MoveNode(Direction::Left));

        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 100, 30)),
                (WindowId::new(pid, 2), rect(100, 0, 100, 15)),
                (WindowId::new(pid, 3), rect(100, 15, 100, 15)),
                (WindowId::new(pid, 4), rect(200, 0, 100, 30)),
            ],
            mgr.layout_sorted(space, screen1),
        );

        // Add a new window when the left window is selected.
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 1)));
        _ = mgr.handle_event(WindowAdded(space, WindowId::new(pid, 6), win_info()));
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 75, 30)),
                (WindowId::new(pid, 2), rect(150, 0, 75, 15)),
                (WindowId::new(pid, 3), rect(150, 15, 75, 15)),
                (WindowId::new(pid, 4), rect(225, 0, 75, 30)),
                (WindowId::new(pid, 6), rect(75, 0, 75, 30)),
            ],
            mgr.layout_sorted(space, screen1),
        );
        _ = mgr.handle_event(WindowRemoved(WindowId::new(pid, 6)));

        // Add a new window when the top middle is selected.
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 2)));
        _ = mgr.handle_event(WindowAdded(space, WindowId::new(pid, 6), win_info()));
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 100, 30)),
                (WindowId::new(pid, 2), rect(100, 0, 100, 10)),
                (WindowId::new(pid, 3), rect(100, 20, 100, 10)),
                (WindowId::new(pid, 4), rect(200, 0, 100, 30)),
                (WindowId::new(pid, 6), rect(100, 10, 100, 10)),
            ],
            mgr.layout_sorted(space, screen1),
        );
        _ = mgr.handle_event(WindowRemoved(WindowId::new(pid, 6)));

        // Same thing, but unfloat an existing window instead of making a new one.
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 2)));
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 5)));
        _ = mgr.handle_command(Some(space), &[space], ToggleWindowFloating);
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 100, 30)),
                (WindowId::new(pid, 2), rect(100, 0, 100, 10)),
                (WindowId::new(pid, 3), rect(100, 20, 100, 10)),
                (WindowId::new(pid, 4), rect(200, 0, 100, 30)),
                (WindowId::new(pid, 5), rect(100, 10, 100, 10)),
            ],
            mgr.layout_sorted(space, screen1),
        );
        _ = mgr.handle_command(Some(space), &[space], ToggleWindowFloating);

        // Add a new window when the bottom middle is selected.
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 3)));
        _ = mgr.handle_event(WindowAdded(space, WindowId::new(pid, 6), win_info()));
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 100, 30)),
                (WindowId::new(pid, 2), rect(100, 0, 100, 10)),
                (WindowId::new(pid, 3), rect(100, 10, 100, 10)),
                (WindowId::new(pid, 4), rect(200, 0, 100, 30)),
                (WindowId::new(pid, 6), rect(100, 20, 100, 10)),
            ],
            mgr.layout_sorted(space, screen1),
        );
        _ = mgr.handle_event(WindowRemoved(WindowId::new(pid, 6)));

        // Add a new window when the right window is selected.
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 4)));
        _ = mgr.handle_event(WindowAdded(space, WindowId::new(pid, 6), win_info()));
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 75, 30)),
                (WindowId::new(pid, 2), rect(75, 0, 75, 15)),
                (WindowId::new(pid, 3), rect(75, 15, 75, 15)),
                (WindowId::new(pid, 4), rect(150, 0, 75, 30)),
                (WindowId::new(pid, 6), rect(225, 0, 75, 30)),
            ],
            mgr.layout_sorted(space, screen1),
        );
        _ = mgr.handle_event(WindowRemoved(WindowId::new(pid, 6)));
    }

    #[test]
    fn add_remove_add() {
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let pid = 1;

        let screen1 = rect(0, 0, 300, 30);
        _ = mgr.handle_event(SpaceExposed(space, screen1.size));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, vec![]));
        _ = mgr.handle_event(WindowAdded(space, WindowId::new(pid, 1), win_info()));
        _ = mgr.handle_event(WindowAdded(space, WindowId::new(pid, 2), win_info()));
        _ = mgr.handle_event(WindowAdded(space, WindowId::new(pid, 3), win_info()));

        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 100, 30)),
                (WindowId::new(pid, 2), rect(100, 0, 100, 30)),
                (WindowId::new(pid, 3), rect(200, 0, 100, 30)),
            ],
            mgr.layout_sorted(space, screen1),
        );

        _ = mgr.handle_event(WindowRemoved(WindowId::new(pid, 3)));
        _ = mgr.handle_event(WindowRemoved(WindowId::new(pid, 1)));
        _ = mgr.handle_event(WindowRemoved(WindowId::new(pid, 2)));
        _ = mgr.handle_event(WindowAdded(space, WindowId::new(pid, 1), win_info()));
        _ = mgr.handle_event(WindowAdded(space, WindowId::new(pid, 2), win_info()));
        _ = mgr.handle_event(WindowAdded(space, WindowId::new(pid, 3), win_info()));

        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 100, 30)),
                (WindowId::new(pid, 2), rect(100, 0, 100, 30)),
                (WindowId::new(pid, 3), rect(200, 0, 100, 30)),
            ],
            mgr.layout_sorted(space, screen1),
        );
    }

    #[test]
    fn resize_to_full_screen_and_back_preserves_layout() {
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let pid = 1;

        let screen1 = rect(0, 0, 300, 30);
        _ = mgr.handle_event(SpaceExposed(space, screen1.size));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, vec![]));
        _ = mgr.handle_event(WindowAdded(space, WindowId::new(pid, 1), win_info()));
        _ = mgr.handle_event(WindowAdded(space, WindowId::new(pid, 2), win_info()));
        _ = mgr.handle_event(WindowAdded(space, WindowId::new(pid, 3), win_info()));

        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 100, 30)),
                (WindowId::new(pid, 2), rect(100, 0, 100, 30)),
                (WindowId::new(pid, 3), rect(200, 0, 100, 30)),
            ],
            mgr.layout_sorted(space, screen1),
        );

        _ = mgr.handle_event(WindowResized {
            wid: WindowId::new(pid, 2),
            old_frame: rect(100, 0, 100, 30),
            new_frame: screen1,
            screens: vec![(space, screen1)],
        });

        // Check that the other windows aren't resized (especially to zero);
        // otherwise, we will lose the layout state as we receive nonconforming
        // frame changed events.
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 100, 30)),
                (WindowId::new(pid, 2), screen1),
                (WindowId::new(pid, 3), rect(200, 0, 100, 30)),
            ],
            mgr.layout_sorted(space, screen1),
        );

        _ = mgr.handle_event(WindowResized {
            wid: WindowId::new(pid, 2),
            old_frame: screen1,
            new_frame: rect(100, 0, 100, 30),
            screens: vec![(space, screen1)],
        });

        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 100, 30)),
                (WindowId::new(pid, 2), rect(100, 0, 100, 30)),
                (WindowId::new(pid, 3), rect(200, 0, 100, 30)),
            ],
            mgr.layout_sorted(space, screen1),
        );
    }

    #[test]
    fn resize_to_system_full_screen_and_back_preserves_layout() {
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let pid = 1;

        let screen1 = rect(0, 10, 300, 20);
        let screen1_full = rect(0, 0, 300, 30);
        _ = mgr.handle_event(SpaceExposed(space, screen1.size));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, vec![]));
        _ = mgr.handle_event(WindowAdded(space, WindowId::new(pid, 1), win_info()));
        _ = mgr.handle_event(WindowAdded(space, WindowId::new(pid, 2), win_info()));
        _ = mgr.handle_event(WindowAdded(space, WindowId::new(pid, 3), win_info()));

        let orig = vec![
            (WindowId::new(pid, 1), rect(0, 10, 100, 20)),
            (WindowId::new(pid, 2), rect(100, 10, 100, 20)),
            (WindowId::new(pid, 3), rect(200, 10, 100, 20)),
        ];
        assert_eq!(orig, mgr.layout_sorted(space, screen1));

        // Simulate a window going fullscreen on the current space.
        //
        // The leftmost window is better for testing because it passes the
        // "only resize in 2 directions" requirement.
        _ = mgr.handle_event(WindowResized {
            wid: WindowId::new(pid, 1),
            old_frame: rect(0, 10, 100, 20),
            new_frame: screen1_full,
            screens: vec![(space, screen1)],
        });

        _ = mgr.handle_event(WindowResized {
            wid: WindowId::new(pid, 1),
            old_frame: screen1_full,
            new_frame: rect(0, 10, 100, 20),
            screens: vec![(space, screen1)],
        });

        assert_eq!(orig, mgr.layout_sorted(space, screen1));
    }

    #[test]
    fn flip_between_screens() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space1 = SpaceId::new(1);
        let space2 = SpaceId::new(2);
        let pid = 1;

        let screen1 = rect(0, 0, 300, 30);
        let screen2 = rect(300, 0, 300, 30);
        _ = mgr.handle_event(SpaceExposed(space1, screen1.size));
        _ = mgr.handle_event(SpaceExposed(space2, screen2.size));
        _ = mgr.handle_event(WindowsOnScreenUpdated(
            space1,
            pid,
            vec![
                (WindowId::new(pid, 1), win_info()),
                (WindowId::new(pid, 2), win_info()),
            ],
        ));
        _ = mgr.handle_event(WindowsOnScreenUpdated(
            space2,
            pid,
            vec![
                (WindowId::new(pid, 3), win_info()),
                (WindowId::new(pid, 4), win_info()),
            ],
        ));
        _ = mgr.handle_event(WindowFocused(vec![space1, space2], WindowId::new(pid, 3)));
        _ = mgr.handle_event(WindowFocused(vec![space1, space2], WindowId::new(pid, 1)));

        // Test moving focus between screens.
        assert_eq!(
            mgr.handle_command(Some(space1), &[space1, space2], MoveFocus(Direction::Right))
                .focus_window,
            Some(WindowId::new(pid, 2))
        );
        _ = mgr.handle_event(WindowFocused(vec![space1, space2], WindowId::new(pid, 2)));
        assert_eq!(
            mgr.handle_command(Some(space1), &[space1, space2], MoveFocus(Direction::Right))
                .focus_window,
            Some(WindowId::new(pid, 3))
        );
        _ = mgr.handle_event(WindowFocused(vec![space1, space2], WindowId::new(pid, 3)));
        assert_eq!(
            mgr.handle_command(Some(space2), &[space1, space2], MoveFocus(Direction::Left))
                .focus_window,
            Some(WindowId::new(pid, 2))
        );
        _ = mgr.handle_event(WindowFocused(vec![space1, space2], WindowId::new(pid, 3)));

        // Test moving a node between screens.
        _ = mgr.handle_command(Some(space1), &[space1, space2], MoveNode(Direction::Right));
        mgr.debug_tree(space2);
        assert_eq!(
            vec![(WindowId::new(pid, 1), rect(0, 0, 300, 30)),],
            mgr.layout_sorted(space1, screen1),
        );
        assert_eq!(
            vec![
                // Note that 2 is moved to the right of 3.
                (WindowId::new(pid, 2), rect(400, 0, 100, 30)),
                (WindowId::new(pid, 3), rect(300, 0, 100, 30)),
                (WindowId::new(pid, 4), rect(500, 0, 100, 30)),
            ],
            mgr.layout_sorted(space2, screen2),
        );
        assert_eq!(Some(WindowId::new(pid, 2)), mgr.selected_window(space2));

        // Finally, test moving focus after moving the node.
        assert_eq!(
            mgr.handle_command(Some(space2), &[space1, space2], MoveFocus(Direction::Right))
                .focus_window,
            Some(WindowId::new(pid, 4))
        );
    }

    #[test]
    fn move_node_between_spaces_keeps_window_mapping_consistent() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space1 = SpaceId::new(1);
        let space2 = SpaceId::new(2);
        let pid = 1;

        let screen1 = rect(0, 0, 300, 30);
        let screen2 = rect(300, 0, 300, 30);
        _ = mgr.handle_event(SpaceExposed(space1, screen1.size));
        _ = mgr.handle_event(SpaceExposed(space2, screen2.size));
        _ = mgr.handle_event(WindowsOnScreenUpdated(
            space1,
            pid,
            vec![
                (WindowId::new(pid, 1), win_info()),
                (WindowId::new(pid, 2), win_info()),
            ],
        ));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space2, pid, vec![]));

        let moved = WindowId::new(pid, 2);
        _ = mgr.handle_event(WindowFocused(vec![space1, space2], moved));

        // Move the selected window off the right edge of space1, into space2.
        _ = mgr.handle_command(Some(space1), &[space1, space2], MoveNode(Direction::Right));

        let windows_in = |mgr: &LayoutManager, space, screen| {
            mgr.layout_sorted(space, screen)
                .into_iter()
                .map(|(wid, _)| wid)
                .collect::<Vec<_>>()
        };
        assert!(!windows_in(&mgr, space1, screen1).contains(&moved));
        assert!(windows_in(&mgr, space2, screen2).contains(&moved));

        // The window <-> layout mapping must follow the node into space2. The
        // old move_node_after reparented the node but left this mapping pointing
        // at space1, so window_node lookups returned the wrong layout. This is
        // the long-standing bug this assertion guards against.
        assert_eq!(None, mgr.tree.window_node(mgr.layout(space1), moved));
        assert!(mgr.tree.window_node(mgr.layout(space2), moved).is_some());

        // A follow-up that resolves the window through its mapping now works.
        // Focusing it must select it in space2 (WindowFocused uses window_node),
        // and moving it left must carry it back to space1. With a stale mapping
        // the focus would miss and the window would never return to space1.
        _ = mgr.handle_event(WindowFocused(vec![space1, space2], moved));
        _ = mgr.handle_command(Some(space2), &[space1, space2], MoveNode(Direction::Left));
        assert!(windows_in(&mgr, space1, screen1).contains(&moved));
        assert!(!windows_in(&mgr, space2, screen2).contains(&moved));
    }

    #[test]
    fn floating_window_space_change_stays_floating() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space1 = SpaceId::new(1);
        let space2 = SpaceId::new(2);
        let pid = 1;

        let screen1 = rect(0, 0, 300, 30);
        let screen2 = rect(300, 0, 300, 30);
        _ = mgr.handle_event(SpaceExposed(space1, screen1.size));
        _ = mgr.handle_event(SpaceExposed(space2, screen2.size));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space1, pid, make_windows(pid, 2)));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space2, pid, vec![]));

        // Float the first window on space1.
        let floated = WindowId::new(pid, 1);
        _ = mgr.handle_event(WindowFocused(vec![space1], floated));
        _ = mgr.handle_command(Some(space1), &[space1], ToggleWindowFloating);
        assert_eq!(BTreeSet::from([floated]), mgr.floating_windows_in_space(space1));

        // Drag the floating window onto space2.
        _ = mgr.handle_event(WindowSpaceChanged {
            wid: floated,
            added: Some(space2),
            removed: Some(space1),
            info: win_info(),
        });

        // It must remain floating, tracked under space2 and not space1. It must
        // not be tiled into either layout: the old handler called
        // add_window_after unconditionally, which pulled it into space2's tree.
        assert_eq!(BTreeSet::new(), mgr.floating_windows_in_space(space1));
        assert_eq!(BTreeSet::from([floated]), mgr.floating_windows_in_space(space2));
        let tiled = |mgr: &LayoutManager, space, screen| {
            mgr.layout_sorted(space, screen)
                .into_iter()
                .map(|(wid, _)| wid)
                .collect::<Vec<_>>()
        };
        assert!(!tiled(&mgr, space1, screen1).contains(&floated));
        assert!(!tiled(&mgr, space2, screen2).contains(&floated));
    }

    #[test]
    fn window_entering_a_space_for_the_first_time_is_classified() {
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let pid = 1;

        let screen = rect(0, 0, 300, 30);
        _ = mgr.handle_event(SpaceExposed(space, screen.size));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, make_windows(pid, 2)));

        // A window that was off screen when we first saw it never went through
        // classification, so a space change is its first layout event.
        let nonstandard = WindowId::new(pid, 3);
        _ = mgr.handle_event(WindowSpaceChanged {
            wid: nonstandard,
            added: Some(space),
            removed: None,
            info: LayoutWindowInfo {
                is_standard: false,
                ..win_info()
            },
        });

        assert_eq!(
            BTreeSet::from([nonstandard]),
            mgr.floating_windows_in_space(space)
        );
        let tiled = mgr
            .layout_sorted(space, screen)
            .into_iter()
            .map(|(wid, _)| wid)
            .collect::<Vec<_>>();
        assert!(!tiled.contains(&nonstandard));
    }

    #[test]
    fn window_dragged_into_scroll_space_joins_a_column() {
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        mgr.set_config(&config_with_scroll(true, LayoutKind::Scroll));
        let space1 = SpaceId::new(1);
        let space2 = SpaceId::new(2);
        let pid = 1;

        let screen1 = rect(0, 0, 300, 30);
        let screen2 = rect(300, 0, 300, 30);
        _ = mgr.handle_event(SpaceExposed(space1, screen1.size));
        _ = mgr.handle_event(SpaceExposed(space2, screen2.size));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space1, pid, make_windows(pid, 2)));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space2, pid, vec![]));
        assert_eq!(LayoutKind::Scroll, mgr.active_layout_kind(space2));

        // Drag a window from the scroll space1 into the scroll space2.
        let moved = WindowId::new(pid, 1);
        _ = mgr.handle_event(WindowSpaceChanged {
            wid: moved,
            added: Some(space2),
            removed: Some(space1),
            info: win_info(),
        });

        // In a scroll layout windows live inside column containers, never as a
        // direct child of the root. The old handler called add_window_after,
        // which dropped a bare window node directly under the root.
        let layout2 = mgr.layout(space2);
        let node = mgr.tree.window_node(layout2, moved).expect("window should be in space2");
        assert_ne!(
            Some(mgr.tree.root(layout2)),
            node.parent(mgr.tree.map()),
            "scroll-space window should be nested in a column, not under the root"
        );
    }

    #[test]
    fn focus_next_prev() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let pid = 1;

        let screen = rect(0, 0, 1000, 1000);
        _ = mgr.handle_event(SpaceExposed(space, screen.size));
        _ = mgr.handle_event(WindowsOnScreenUpdated(
            space,
            pid,
            vec![
                (WindowId::new(pid, 1), win_info()),
                (WindowId::new(pid, 2), win_info()),
                (WindowId::new(pid, 3), win_info()),
            ],
        ));
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 1)));

        // Test FocusNext
        assert_eq!(
            mgr.handle_command(Some(space), &[space], FocusNext).focus_window,
            Some(WindowId::new(pid, 2))
        );
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 2)));

        assert_eq!(
            mgr.handle_command(Some(space), &[space], FocusNext).focus_window,
            Some(WindowId::new(pid, 3))
        );
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 3)));

        assert_eq!(
            mgr.handle_command(Some(space), &[space], FocusNext).focus_window,
            Some(WindowId::new(pid, 1))
        ); // wraparound
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 1)));

        // Test FocusPrev
        assert_eq!(
            mgr.handle_command(Some(space), &[space], FocusPrev).focus_window,
            Some(WindowId::new(pid, 3))
        ); // wraparound
    }

    #[test]
    fn it_resizes_windows_with_resize_command() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let pid = 1;
        let windows = make_windows(pid, 2);

        let screen = rect(0, 0, 100, 100);
        _ = mgr.handle_event(SpaceExposed(space, screen.size));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, windows));
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 1)));

        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 50, 100)),
                (WindowId::new(pid, 2), rect(50, 0, 50, 100)),
            ],
            mgr.layout_sorted(space, screen),
        );

        _ = mgr.handle_command(
            Some(space),
            &[space],
            Resize {
                direction: Direction::Right,
                percent: 10.0,
            },
        );
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 60, 100)),
                (WindowId::new(pid, 2), rect(60, 0, 40, 100)),
            ],
            mgr.layout_sorted(space, screen),
        );

        _ = mgr.handle_command(Some(space), &[space], MoveFocus(Direction::Right));
        _ = mgr.handle_command(
            Some(space),
            &[space],
            Resize {
                direction: Direction::Left,
                percent: 10.0,
            },
        );
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 50, 100)),
                (WindowId::new(pid, 2), rect(50, 0, 50, 100)),
            ],
            mgr.layout_sorted(space, screen),
        );
    }

    #[test]
    fn it_flips_the_container_with_toggle_orientation() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let pid = 1;
        let windows = make_windows(pid, 2);

        let screen = rect(0, 0, 100, 100);
        _ = mgr.handle_event(SpaceExposed(space, screen.size));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, windows));
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 1)));

        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 50, 100)),
                (WindowId::new(pid, 2), rect(50, 0, 50, 100)),
            ],
            mgr.layout_sorted(space, screen),
        );

        // Side by side becomes stacked top to bottom.
        _ = mgr.handle_command(Some(space), &[space], ToggleOrientation);
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 100, 50)),
                (WindowId::new(pid, 2), rect(0, 50, 100, 50)),
            ],
            mgr.layout_sorted(space, screen),
        );

        // Toggling again returns to the original layout.
        _ = mgr.handle_command(Some(space), &[space], ToggleOrientation);
        assert_eq!(
            vec![
                (WindowId::new(pid, 1), rect(0, 0, 50, 100)),
                (WindowId::new(pid, 2), rect(50, 0, 50, 100)),
            ],
            mgr.layout_sorted(space, screen),
        );
    }

    #[test]
    fn toggle_orientation_keeps_a_group_a_group() {
        use LayoutCommand::*;
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let space = SpaceId::new(1);
        let pid = 1;
        let windows = make_windows(pid, 2);

        let screen = rect(0, 0, 100, 100);
        _ = mgr.handle_event(SpaceExposed(space, screen.size));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, windows));
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 1)));

        _ = mgr.handle_command(Some(space), &[space], Group(Orientation::Horizontal));
        _ = mgr.handle_command(Some(space), &[space], ToggleOrientation);

        let layout = mgr.layout(space);
        let parent = mgr.tree.selection(layout).parent(mgr.tree.map()).unwrap();
        assert_eq!(ContainerKind::Stacked, mgr.tree.container_kind(parent));
    }

    #[test]
    fn space_exposed_forces_tree_when_scroll_gate_disabled() {
        use LayoutEvent::*;
        let mut mgr = LayoutManager::new_for_test();
        let config = config_with_scroll(false, LayoutKind::Scroll);
        mgr.set_config(&config);

        let space = SpaceId::new(1);
        _ = mgr.handle_event(SpaceExposed(space, rect(0, 0, 300, 200).size));

        assert_eq!(mgr.active_layout_kind(space), LayoutKind::Tree);
    }

    #[test]
    fn change_layout_kind_noops_when_scroll_gate_disabled() {
        use LayoutCommand::*;
        use LayoutEvent::*;

        let mut mgr = LayoutManager::new_for_test();
        let config = config_with_scroll(false, LayoutKind::Tree);
        mgr.set_config(&config);

        let space = SpaceId::new(1);
        let pid = 1;
        _ = mgr.handle_event(SpaceExposed(space, rect(0, 0, 400, 200).size));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, make_windows(pid, 2)));
        _ = mgr.handle_command(Some(space), &[space], ChangeLayoutKind);

        assert_eq!(mgr.active_layout_kind(space), LayoutKind::Tree);
    }

    #[test]
    fn active_scroll_layout_converts_to_tree_when_gate_disabled() {
        use LayoutEvent::*;

        let mut mgr = LayoutManager::new_for_test();
        let config_on = config_with_scroll(true, LayoutKind::Scroll);
        mgr.set_config(&config_on);

        let space = SpaceId::new(1);
        let pid = 1;
        _ = mgr.handle_event(SpaceExposed(space, rect(0, 0, 500, 300).size));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, make_windows(pid, 3)));
        assert_eq!(mgr.active_layout_kind(space), LayoutKind::Scroll);

        let config_off = config_with_scroll(false, LayoutKind::Tree);
        mgr.set_config(&config_off);

        assert_eq!(mgr.active_layout_kind(space), LayoutKind::Tree);
    }

    #[test]
    fn next_layout_skips_scroll_when_gate_disabled() {
        use LayoutCommand::*;
        use LayoutEvent::*;

        let mut mgr = LayoutManager::new_for_test();
        let config_on = config_with_scroll(true, LayoutKind::Scroll);
        mgr.set_config(&config_on);

        let space = SpaceId::new(1);
        let pid = 1;
        _ = mgr.handle_event(SpaceExposed(space, rect(0, 0, 500, 300).size));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, make_windows(pid, 3)));
        assert_eq!(mgr.active_layout_kind(space), LayoutKind::Scroll);

        _ = mgr.handle_command(Some(space), &[space], ChangeLayoutKind);
        assert_eq!(mgr.active_layout_kind(space), LayoutKind::Tree);

        let config_off = config_with_scroll(false, LayoutKind::Tree);
        mgr.set_config(&config_off);
        _ = mgr.handle_command(Some(space), &[space], NextLayout);

        assert_eq!(mgr.active_layout_kind(space), LayoutKind::Tree);
    }

    #[test]
    fn scroll_wheel_is_ignored_when_scroll_gate_disabled() {
        use LayoutEvent::*;

        let mut mgr = LayoutManager::new_for_test();
        let config_on = config_with_scroll(true, LayoutKind::Scroll);
        mgr.set_config(&config_on);

        let space = SpaceId::new(1);
        let screen = rect(0, 0, 900, 600);
        let pid = 1;
        _ = mgr.handle_event(SpaceExposed(space, screen.size));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, make_windows(pid, 3)));
        _ = mgr.handle_event(WindowFocused(vec![space], WindowId::new(pid, 1)));
        assert_eq!(mgr.active_layout_kind(space), LayoutKind::Scroll);

        let config_off = config_with_scroll(false, LayoutKind::Tree);
        mgr.set_config(&config_off);
        let response =
            mgr.handle_scroll_wheel(space, -1.0, &screen, &config_off.settings.experimental.scroll);
        assert!(response.raise_windows.is_empty());
        assert!(response.focus_window.is_none());
    }

    #[test]
    fn scroll_only_commands_noop_when_gate_disabled() {
        use LayoutCommand::*;
        use LayoutEvent::*;

        let mut mgr = LayoutManager::new_for_test();
        let config_on = config_with_scroll(true, LayoutKind::Scroll);
        mgr.set_config(&config_on);

        let space = SpaceId::new(1);
        let screen = rect(0, 0, 1000, 600);
        let pid = 1;
        _ = mgr.handle_event(SpaceExposed(space, screen.size));
        _ = mgr.handle_event(WindowsOnScreenUpdated(space, pid, make_windows(pid, 3)));
        assert_eq!(mgr.active_layout_kind(space), LayoutKind::Scroll);

        let config_off = config_with_scroll(false, LayoutKind::Tree);
        mgr.set_config(&config_off);
        assert_eq!(mgr.active_layout_kind(space), LayoutKind::Tree);

        let before = mgr.layout_sorted(space, screen);
        _ = mgr.handle_command(Some(space), &[space], CycleColumnWidth);
        _ = mgr.handle_command(Some(space), &[space], ToggleColumnTabbed);
        assert_eq!(mgr.active_layout_kind(space), LayoutKind::Tree);
        assert_eq!(mgr.layout_sorted(space, screen), before);
    }

    #[test]
    fn event_response_coalesce_downgrades_extra_focus_to_raise() {
        let response = EventResponse {
            frame_overrides: vec![],
            raise_windows: vec![WindowId::new(1, 1)],
            focus_window: Some(WindowId::new(1, 2)),
        }
        .coalesce(EventResponse {
            frame_overrides: vec![],
            raise_windows: vec![WindowId::new(1, 3)],
            focus_window: Some(WindowId::new(1, 4)),
        });

        assert_eq!(
            response.raise_windows,
            vec![
                WindowId::new(1, 1),
                WindowId::new(1, 3),
                WindowId::new(1, 4)
            ]
        );
        assert_eq!(response.focus_window, Some(WindowId::new(1, 2)));
    }
}
