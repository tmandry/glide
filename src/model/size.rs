// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

use core::fmt::Debug;
use std::mem;

use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use serde::{Deserialize, Serialize};

use super::layout_tree::TreeEvent;
use super::selection::Selection;
use super::tree::{NodeId, NodeMap};
use crate::actor::app::WindowId;
use crate::config::Config;
use crate::sys::geometry::{CGRectExt, Round};

#[derive(Default, Serialize, Deserialize)]
pub struct Size {
    info: slotmap::SecondaryMap<NodeId, LayoutInfo>,
}

#[allow(unused)]
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerKind {
    #[default]
    Horizontal,
    Vertical,
    Tabbed,
    Stacked,
}

impl ContainerKind {
    pub fn from(orientation: Orientation) -> Self {
        match orientation {
            Orientation::Horizontal => ContainerKind::Horizontal,
            Orientation::Vertical => ContainerKind::Vertical,
        }
    }

    pub fn group(orientation: Orientation) -> Self {
        match orientation {
            Orientation::Horizontal => ContainerKind::Tabbed,
            Orientation::Vertical => ContainerKind::Stacked,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Orientation {
    Horizontal,
    Vertical,
}

/// The orientation a new layout's root container starts out with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootOrientation {
    #[default]
    Horizontal,
    Vertical,
    /// Split the screen's longer axis, so a portrait screen stacks windows
    /// and a landscape one puts them side by side.
    Auto,
}

impl RootOrientation {
    pub fn resolve(self, screen: CGSize) -> Orientation {
        match self {
            RootOrientation::Horizontal => Orientation::Horizontal,
            RootOrientation::Vertical => Orientation::Vertical,
            // A square screen has no longer axis; treat it as landscape,
            // which is what every other orientation defaults to.
            RootOrientation::Auto if screen.height > screen.width => Orientation::Vertical,
            RootOrientation::Auto => Orientation::Horizontal,
        }
    }
}

impl ContainerKind {
    pub fn orientation(self) -> Orientation {
        use ContainerKind::*;
        match self {
            Horizontal | Tabbed => Orientation::Horizontal,
            Vertical | Stacked => Orientation::Vertical,
        }
    }

    pub fn is_group(self) -> bool {
        use ContainerKind::*;
        match self {
            Stacked | Tabbed => true,
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    pub(super) fn orientation(self) -> Orientation {
        use Direction::*;
        match self {
            Left | Right => Orientation::Horizontal,
            Up | Down => Orientation::Vertical,
        }
    }
}

#[derive(Debug, Clone)]
pub struct GroupBarInfo {
    pub node_id: NodeId,
    pub container_kind: ContainerKind,
    /// Total number of windows in the group
    pub total_count: usize,
    /// Index of the currently selected window
    pub selected_index: usize,
    /// Whether this group should be visible
    pub is_visible: bool,
    /// Whether this group is in the selection path
    pub is_selected: bool,
    /// Frame reserved for the indicator bar.
    pub indicator_frame: CGRect,
    /// Whether this bar is on top of other windows in the layout.
    ///
    /// Always set to true by us; higher layers set to false if there are
    /// floating windows.
    pub is_on_top: bool,
}

// TODO:
//
// It'd be much easier to only move specific edges if we keep the min edge
// of each child (relative to the parent, from 0 to 1). Then we just need
// to adjust this edge, and preserve the invariant that no edge is greater
// than the following edge.
//
// Calculating the size of a single node is easy and just needs to look at the
// next sibling.
//
// Proportional changes would no longer happen by default, but should still be
// relatively easy. Just keep a count of children, and we can adjust each child's
// size in a single scan.
//
// This seems *way* simpler than trying to fix up a proportionate representation
// to create a single edge change.
//
// Actually, on second thought, this would still create proportional resizes of
// children. To prevent that we would need the edges to be absolute (relative
// to the root) and traverse *recursively* when one is modified, fixing up any
// edges that violate our invariant.
//
// This might still be overall simpler than the resize logic would need to be
// for the proportionate case, but it feels more like we are distributing the
// complexity rather than reducing it.

#[derive(Default, Debug, Serialize, Deserialize, Clone)]
struct LayoutInfo {
    /// The share of the parent's size taken up by this node; 1.0 by default.
    size: f32,
    /// The total size of all children.
    total: f32,
    /// The orientation of this node. Not used for leaf nodes.
    kind: ContainerKind,
    /// The last ungrouped layout of this node.
    last_ungrouped_kind: ContainerKind,
    /// Whether the node is fullscreen.
    #[serde(default)]
    is_fullscreen: bool,
}

impl Size {
    pub(super) fn handle_event(&mut self, map: &NodeMap, event: TreeEvent) {
        match event {
            TreeEvent::AddedToForest(node) => {
                self.info.insert(node, LayoutInfo::default());
            }
            TreeEvent::AddedToParent(node) => {
                let parent = node.parent(map).unwrap();
                self.info[node].size = 1.0;
                self.info[parent].total += 1.0;
            }
            TreeEvent::Copied { src, dest } => {
                self.info.insert(dest, self.info[src].clone());
            }
            TreeEvent::RemovingFromParent(node) => {
                self.info[node.parent(map).unwrap()].total -= self.info[node].size;
            }
            TreeEvent::RemovedFromForest(node) => {
                self.info.remove(node);
            }
        }
    }

    pub(super) fn assume_size_of(&mut self, new: NodeId, old: NodeId, map: &NodeMap) {
        assert_eq!(new.parent(map), old.parent(map));
        let parent = new.parent(map).unwrap();
        self.info[parent].total -= self.info[new].size;
        self.info[new].size = mem::replace(&mut self.info[old].size, 0.0);
    }

    pub(super) fn set_kind(&mut self, node: NodeId, kind: ContainerKind) {
        self.info[node].kind = kind;
        if !kind.is_group() {
            self.info[node].last_ungrouped_kind = kind;
        }
    }

    pub(super) fn kind(&self, node: NodeId) -> ContainerKind {
        self.info[node].kind
    }

    pub(super) fn last_ungrouped_kind(&self, node: NodeId) -> ContainerKind {
        self.info[node].last_ungrouped_kind
    }

    pub(super) fn proportion(&self, map: &NodeMap, node: NodeId) -> Option<f64> {
        let Some(parent) = node.parent(map) else { return None };
        Some(f64::from(self.info[node].size) / f64::from(self.info[parent].total))
    }

    pub(super) fn total(&self, node: NodeId) -> f64 {
        f64::from(self.info[node].total)
    }

    pub(super) fn weight(&self, node: NodeId) -> f32 {
        self.info[node].size
    }

    pub(super) fn take_share(&mut self, map: &NodeMap, node: NodeId, from: NodeId, share: f32) {
        assert_eq!(node.parent(map), from.parent(map));
        let share = share.min(self.info[from].size);
        let share = share.max(-self.info[node].size);
        self.info[from].size -= share;
        self.info[node].size += share;
    }

    pub(super) fn set_weight(&mut self, node: NodeId, weight: f32, map: &NodeMap) {
        let old = self.info[node].size;
        self.info[node].size = weight;
        if let Some(parent) = node.parent(map) {
            self.info[parent].total += weight - old;
        }
    }

    pub(super) fn set_fullscreen(&mut self, node: NodeId, is_fullscreen: bool) {
        self.info[node].is_fullscreen = is_fullscreen;
    }

    pub(super) fn is_fullscreen(&mut self, node: NodeId) -> bool {
        self.info[node].is_fullscreen
    }

    pub(super) fn debug(&self, node: NodeId, is_container: bool) -> String {
        let info = &self.info[node];
        let fullscreen = if info.is_fullscreen {
            "; fullscreen"
        } else {
            ""
        };
        if is_container {
            format!(
                "{:?} [size {} total={}{fullscreen}]",
                info.kind, info.size, info.total
            )
        } else {
            format!("[size {}{fullscreen}]", info.size)
        }
    }

    pub(super) fn get_sizes(
        &self,
        map: &NodeMap,
        window: &super::window::Window,
        selection: &Selection,
        config: &Config,
        root: NodeId,
        screen: CGRect,
        is_scroll: bool,
    ) -> Vec<(WindowId, CGRect)> {
        let mut sizes = vec![];
        Visitor {
            map,
            size: self,
            window,
            selection,
            fullscreen_nodes: &[],
            config,
            screen,
            is_scroll,
            sizes: &mut sizes,
            groups: None,
            target: None,
            target_rect: None,
        }
        .visit(root, screen);
        sizes
    }

    /// Returns the rect the layout assigns to `node`.
    ///
    /// `node` must be reachable from `root`.
    pub(super) fn get_node_rect(
        &self,
        map: &NodeMap,
        window: &super::window::Window,
        selection: &Selection,
        config: &Config,
        root: NodeId,
        screen: CGRect,
        is_scroll: bool,
        node: NodeId,
    ) -> Option<CGRect> {
        let mut sizes = vec![];
        Visitor {
            map,
            size: self,
            window,
            selection,
            fullscreen_nodes: &[],
            config,
            screen,
            is_scroll,
            sizes: &mut sizes,
            groups: None,
            target: Some(node),
            target_rect: None,
        }
        .visit(root, screen)
    }

    /// The extent, along `parent`'s orientation, that one unit of `parent`'s
    /// total weight is worth in its children's frames.
    ///
    /// The inner gaps between children are subtracted first, since they are not
    /// distributed by weight.
    pub(super) fn pixels_per_weight(
        &self,
        map: &NodeMap,
        parent: NodeId,
        parent_rect: CGRect,
        config: &Config,
        is_scroll_root: bool,
    ) -> f64 {
        let extent = match self.kind(parent).orientation() {
            Orientation::Horizontal => parent_rect.size.width,
            Orientation::Vertical => parent_rect.size.height,
        };
        let total = self.total(parent);
        // Mirrors the inputs `Visitor` gives to `solve_sizes`.
        let available = if is_scroll_root {
            total * extent
        } else {
            extent
        };
        let count = parent.children(map).count() as f64;
        let usable = available - config.settings.inner_gap * (count - 1.0).max(0.0);
        usable / total
    }

    pub(super) fn get_sizes_and_groups(
        &self,
        map: &NodeMap,
        window: &super::window::Window,
        selection: &Selection,
        config: &Config,
        root: NodeId,
        screen: CGRect,
        is_scroll: bool,
    ) -> (Vec<(WindowId, CGRect)>, Vec<GroupBarInfo>) {
        let mut sizes = vec![];
        let mut groups = vec![];
        let fullscreen_nodes = &root
            .traverse_postorder(map)
            .filter(|&node| self.info.get(node).map(|i| i.is_fullscreen).unwrap_or(false))
            .collect::<Vec<_>>();
        Visitor {
            map,
            size: self,
            window,
            selection,
            fullscreen_nodes,
            config,
            screen,
            is_scroll,
            sizes: &mut sizes,
            groups: Some(&mut groups),
            target: None,
            target_rect: None,
        }
        .visit(root, screen);
        (sizes, groups)
    }
}

struct Visitor<'a, 'out> {
    map: &'a NodeMap,
    size: &'a Size,
    window: &'a super::window::Window,
    selection: &'a Selection,
    fullscreen_nodes: &'a [NodeId],
    config: &'a Config,
    screen: CGRect,
    is_scroll: bool,
    sizes: &'out mut Vec<(WindowId, CGRect)>,
    groups: Option<&'out mut Vec<GroupBarInfo>>,
    /// If set, the rect assigned to this node is recorded in `target_rect`.
    target: Option<NodeId>,
    target_rect: Option<CGRect>,
}

impl<'a, 'out> Visitor<'a, 'out> {
    fn visit(mut self, root: NodeId, rect: CGRect) -> Option<CGRect> {
        // Usually this should be false, except in the uncommon case where root
        // is fullscreen.
        let parent_visible = self.fullscreen_nodes.contains(&root);
        let rect = rect.inset(self.config.settings.outer_gap);
        self.visit_node(root, rect, true, parent_visible, true);
        self.target_rect
    }

    fn visit_node(
        &mut self,
        node: NodeId,
        rect: CGRect,
        is_in_visibility_path: bool,
        is_parent_visible: bool,
        is_selected: bool,
    ) {
        let info = &self.size.info[node];
        let rect = if info.is_fullscreen {
            self.screen.inset(self.config.settings.outer_gap)
        } else {
            rect
        };

        if self.target == Some(node) {
            self.target_rect = Some(rect);
        }

        if let Some(wid) = self.window.at(node) {
            debug_assert!(
                node.children(self.map).next().is_none(),
                "non-leaf node with window id"
            );
            self.sizes.push((wid, rect));
            return;
        }

        use ContainerKind::*;
        match info.kind {
            Tabbed | Stacked => {
                let (group_frame, indicator_frame) = if self.config.settings.group_bars.enable {
                    size_with_group_indicator(rect, info.kind, &self.config.settings.group_bars)
                } else {
                    (rect, CGRect::ZERO)
                };

                // Slightly janky visibility computation: If a node is fullscreen
                // only its descendants can be considered visible. If multiple
                // nodes are fullscreen we don't attempt to handle it well.
                let is_visible = is_in_visibility_path
                    && (is_parent_visible
                        || self.fullscreen_nodes.is_empty()
                        || self.fullscreen_nodes.contains(&node));

                let selected_child = self.selection.last_selection(self.map, node);
                let mut selected_index = 0;
                let mut num_children = 0;
                for (index, child) in node.children(self.map).enumerate() {
                    let selected = selected_child == Some(child);
                    if selected {
                        selected_index = index;
                    }
                    num_children += 1;

                    self.visit_node(
                        child,
                        group_frame,
                        is_in_visibility_path && selected,
                        is_visible,
                        is_selected && selected,
                    );
                }

                if let Some(groups) = self.groups.as_deref_mut() {
                    groups.push(GroupBarInfo {
                        node_id: node,
                        container_kind: info.kind,
                        indicator_frame,
                        total_count: num_children,
                        selected_index,
                        is_visible,
                        is_selected,
                        is_on_top: true,
                    });
                }
            }
            Horizontal => {
                let inner_gap = self.config.settings.inner_gap;
                let local_selection = self.selection.local_selection(self.map, node);
                let children: Vec<NodeId> = node.children(self.map).collect();

                let aspect_max_width = if children.len() == 1 {
                    self.config
                        .settings
                        .experimental
                        .scroll
                        .aspect_ratio()
                        .map(|ar| rect.size.height * (ar.width / ar.height))
                } else {
                    None
                };

                let inputs: Vec<super::scroll_constraints::WindowInput> = children
                    .iter()
                    .map(|&child| super::scroll_constraints::WindowInput {
                        weight: f64::from(self.size.info[child].size),
                        min_size: if self.is_scroll {
                            super::scroll_constraints::MIN_WINDOW_SIZE
                        } else {
                            1.0
                        },
                        max_size: aspect_max_width,
                        fixed_size: None,
                    })
                    .collect();

                let total_weight: f64 = inputs.iter().map(|i| i.weight).sum();
                let virtual_width = if self.is_scroll {
                    total_weight * rect.size.width
                } else {
                    rect.size.width
                };
                let outputs =
                    super::scroll_constraints::solve_sizes(&inputs, virtual_width, inner_gap);

                let mut x = rect.origin.x;
                for (&child, output) in children.iter().zip(outputs.iter()) {
                    let rect = CGRect {
                        origin: CGPoint { x, y: rect.origin.y },
                        size: CGSize {
                            width: output.size,
                            height: rect.size.height,
                        },
                    }
                    .round();
                    self.visit_node(
                        child,
                        rect,
                        is_in_visibility_path,
                        is_parent_visible,
                        is_selected && local_selection == Some(child),
                    );
                    x = rect.max().x + inner_gap;
                }
            }
            Vertical => {
                let inner_gap = self.config.settings.inner_gap;
                let local_selection = self.selection.local_selection(self.map, node);
                let children: Vec<NodeId> = node.children(self.map).collect();

                let inputs: Vec<super::scroll_constraints::WindowInput> = children
                    .iter()
                    .map(|&child| super::scroll_constraints::WindowInput {
                        weight: f64::from(self.size.info[child].size),
                        min_size: if self.is_scroll {
                            super::scroll_constraints::MIN_WINDOW_SIZE
                        } else {
                            1.0
                        },
                        max_size: None,
                        fixed_size: None,
                    })
                    .collect();

                let outputs =
                    super::scroll_constraints::solve_sizes(&inputs, rect.size.height, inner_gap);

                let mut y = rect.origin.y;
                for (&child, output) in children.iter().zip(outputs.iter()) {
                    let rect = CGRect {
                        origin: CGPoint { x: rect.origin.x, y },
                        size: CGSize {
                            width: rect.size.width,
                            height: output.size,
                        },
                    }
                    .round();
                    self.visit_node(
                        child,
                        rect,
                        is_in_visibility_path,
                        is_parent_visible,
                        is_selected && local_selection == Some(child),
                    );
                    y = rect.max().y + inner_gap;
                }
            }
        }
    }
}

/// Calculate frames for group and indicator, reserving space for the indicator
fn size_with_group_indicator(
    rect: CGRect,
    container_kind: ContainerKind,
    config: &crate::config::GroupBars,
) -> (CGRect, CGRect) {
    use crate::config::{HorizontalPlacement, VerticalPlacement};

    let thickness = config.thickness;

    match container_kind {
        ContainerKind::Tabbed => {
            // Horizontal indicator
            match config.horizontal_placement {
                HorizontalPlacement::Top => {
                    let group_frame = CGRect {
                        origin: CGPoint {
                            x: rect.origin.x,
                            y: rect.origin.y + thickness,
                        },
                        size: CGSize {
                            width: rect.size.width,
                            height: rect.size.height - thickness,
                        },
                    };
                    let indicator_frame = CGRect {
                        origin: rect.origin,
                        size: CGSize {
                            width: rect.size.width,
                            height: thickness,
                        },
                    };
                    (group_frame, indicator_frame)
                }
                HorizontalPlacement::Bottom => {
                    let group_frame = CGRect {
                        origin: rect.origin,
                        size: CGSize {
                            width: rect.size.width,
                            height: rect.size.height - thickness,
                        },
                    };
                    let indicator_frame = CGRect {
                        origin: CGPoint {
                            x: rect.origin.x,
                            y: rect.origin.y + group_frame.size.height,
                        },
                        size: CGSize {
                            width: rect.size.width,
                            height: thickness,
                        },
                    };
                    (group_frame, indicator_frame)
                }
            }
        }
        ContainerKind::Stacked => {
            // Vertical indicator
            match config.vertical_placement {
                VerticalPlacement::Left => {
                    let group_frame = CGRect {
                        origin: CGPoint {
                            x: rect.origin.x + thickness,
                            y: rect.origin.y,
                        },
                        size: CGSize {
                            width: rect.size.width - thickness,
                            height: rect.size.height,
                        },
                    };
                    let indicator_frame = CGRect {
                        origin: rect.origin,
                        size: CGSize {
                            width: thickness,
                            height: rect.size.height,
                        },
                    };
                    (group_frame, indicator_frame)
                }
                VerticalPlacement::Right => {
                    let group_frame = CGRect {
                        origin: rect.origin,
                        size: CGSize {
                            width: rect.size.width - thickness,
                            height: rect.size.height,
                        },
                    };
                    let indicator_frame = CGRect {
                        origin: CGPoint {
                            x: rect.origin.x + group_frame.size.width,
                            y: rect.origin.y,
                        },
                        size: CGSize {
                            width: thickness,
                            height: rect.size.height,
                        },
                    };
                    (group_frame, indicator_frame)
                }
            }
        }
        _ => (rect, CGRect::ZERO),
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::model::LayoutTree;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> CGRect {
        CGRect::new(
            CGPoint::new(f64::from(x), f64::from(y)),
            CGSize::new(f64::from(w), f64::from(h)),
        )
    }

    #[test]
    fn it_lays_out_windows_proportionally() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);
        let _a1 = tree.add_window_under(layout, root, WindowId::new(1, 1));
        let a2 = tree.add_container(root, ContainerKind::Vertical);
        let _b1 = tree.add_window_under(layout, a2, WindowId::new(1, 2));
        let _b2 = tree.add_window_under(layout, a2, WindowId::new(1, 3));
        let _a3 = tree.add_window_under(layout, root, WindowId::new(1, 4));

        let screen = rect(0, 0, 3000, 1000);
        let (mut frames, groups) =
            tree.calculate_layout_and_groups(layout, screen, &Config::default());
        frames.sort_by_key(|&(wid, _)| wid);
        assert_eq!(
            frames,
            vec![
                (WindowId::new(1, 1), rect(0, 0, 1000, 1000)),
                (WindowId::new(1, 2), rect(1000, 0, 1000, 500)),
                (WindowId::new(1, 3), rect(1000, 500, 1000, 500)),
                (WindowId::new(1, 4), rect(2000, 0, 1000, 1000)),
            ]
        );
        assert_eq!(groups.len(), 0);
    }

    #[test]
    fn it_collects_group_information_for_tabbed_containers() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);
        let _a1 = tree.add_window_under(layout, root, WindowId::new(1, 1));

        // Create a tabbed group with 3 windows
        let tabbed_group = tree.add_container(root, ContainerKind::Tabbed);
        let _tab1 = tree.add_window_under(layout, tabbed_group, WindowId::new(2, 1));
        let _tab2 = tree.add_window_under(layout, tabbed_group, WindowId::new(2, 2));
        let _tab3 = tree.add_window_under(layout, tabbed_group, WindowId::new(2, 3));

        let _a3 = tree.add_window_under(layout, root, WindowId::new(3, 1));

        let screen = rect(0, 0, 3000, 1000);
        let config = Config::default();
        let (frames, groups) = tree.calculate_layout_and_groups(layout, screen, &config);

        assert_eq!(frames.len(), 5);
        assert_eq!(groups.len(), 1);

        let group = &groups[0];
        assert_eq!(group.node_id, tabbed_group);
        assert_eq!(group.container_kind, ContainerKind::Tabbed);
        assert_eq!(group.total_count, 3);
        assert_eq!(group.selected_index, 0); // First child selected by default
        assert_eq!(group.is_visible, true); // Root level group is visible
    }

    #[test]
    fn it_collects_group_information_for_stacked_containers() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);

        // Create a stacked group with 2 windows
        let stacked_group = tree.add_container(root, ContainerKind::Stacked);
        let _child1 = tree.add_window_under(layout, stacked_group, WindowId::new(1, 1));
        let _child2 = tree.add_window_under(layout, stacked_group, WindowId::new(1, 2));
        tree.select(_child2);

        let screen = rect(0, 0, 1000, 1000);
        let config = Config::default();
        let (frames, groups) = tree.calculate_layout_and_groups(layout, screen, &config);

        assert_eq!(frames.len(), 2);
        assert_eq!(groups.len(), 1);

        let group = &groups[0];
        assert_eq!(group.node_id, stacked_group);
        assert_eq!(group.container_kind, ContainerKind::Stacked);
        assert_eq!(group.total_count, 2);
        assert_eq!(group.selected_index, 1);
        assert_eq!(group.is_visible, true);
    }

    #[test]
    fn it_tracks_visibility_for_nested_groups() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);

        // Create outer tabbed group
        let outer_group = tree.add_container(root, ContainerKind::Tabbed);
        let _outer_tab1 = tree.add_window_under(layout, outer_group, WindowId::new(1, 1));

        // Create inner stacked group as second tab (not selected)
        let inner_group = tree.add_container(outer_group, ContainerKind::Stacked);
        let _inner_stack1 = tree.add_window_under(layout, inner_group, WindowId::new(2, 1));
        let _inner_stack2 = tree.add_window_under(layout, inner_group, WindowId::new(2, 2));

        let screen = rect(0, 0, 1000, 1000);
        let config = Config::default();
        let (frames, groups) = tree.calculate_layout_and_groups(layout, screen, &config);

        assert_eq!(frames.len(), 3);
        assert_eq!(groups.len(), 2);

        // Find groups by kind
        let outer = groups.iter().find(|g| g.container_kind == ContainerKind::Tabbed).unwrap();
        let inner = groups.iter().find(|g| g.container_kind == ContainerKind::Stacked).unwrap();

        // Outer group should be visible
        assert_eq!(outer.is_visible, true);
        assert_eq!(outer.total_count, 2); // window + inner group
        assert_eq!(outer.selected_index, 0); // First tab selected

        // Inner group should not be visible (not the selected tab)
        assert_eq!(inner.is_visible, false);
        assert_eq!(inner.total_count, 2);
    }

    #[test]
    fn it_does_not_show_groups_obscured_by_fullscreen_nodes() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);

        // Create outer tabbed group
        let outer_group = tree.add_container(root, ContainerKind::Tabbed);
        let _outer_tab1 = tree.add_window_under(layout, outer_group, WindowId::new(1, 1));

        // Create inner stacked group as second tab (selected)
        let inner_group = tree.add_container(outer_group, ContainerKind::Stacked);
        let inner_stack1 = tree.add_window_under(layout, inner_group, WindowId::new(2, 1));
        let _inner_stack2 = tree.add_window_under(layout, inner_group, WindowId::new(2, 2));
        tree.select(inner_stack1);

        let screen = rect(0, 0, 1000, 1000);
        let config = Config::default();

        // If inner_group is fullscreen, only its indicator should be visible.
        tree.set_fullscreen(inner_group, true);
        let (_frames, groups) = tree.calculate_layout_and_groups(layout, screen, &config);
        let outer = groups.iter().find(|g| g.container_kind == ContainerKind::Tabbed).unwrap();
        let inner = groups.iter().find(|g| g.container_kind == ContainerKind::Stacked).unwrap();
        assert_eq!(outer.is_visible, false);
        assert_eq!(inner.is_visible, true);

        // If a window inside inner_group is fullscreen, no indicators should be visible.
        tree.set_fullscreen(inner_group, false);
        tree.set_fullscreen(inner_stack1, true);
        let (_frames, groups) = tree.calculate_layout_and_groups(layout, screen, &config);
        let outer = groups.iter().find(|g| g.container_kind == ContainerKind::Tabbed).unwrap();
        let inner = groups.iter().find(|g| g.container_kind == ContainerKind::Stacked).unwrap();
        assert_eq!(outer.is_visible, false);
        assert_eq!(inner.is_visible, false);

        // If the root is fullscreen for some reason, it behaves as normal.
        tree.set_fullscreen(inner_stack1, false);
        tree.set_fullscreen(root, true);
        let (_frames, groups) = tree.calculate_layout_and_groups(layout, screen, &config);
        let outer = groups.iter().find(|g| g.container_kind == ContainerKind::Tabbed).unwrap();
        let inner = groups.iter().find(|g| g.container_kind == ContainerKind::Stacked).unwrap();
        assert_eq!(outer.is_visible, true);
        assert_eq!(inner.is_visible, true);
    }

    #[test]
    fn it_handles_regular_containers_without_groups() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);
        let _a1 = tree.add_window_under(layout, root, WindowId::new(1, 1));

        // Create a regular vertical container (not a group)
        let vertical_container = tree.add_container(root, ContainerKind::Vertical);
        let _b1 = tree.add_window_under(layout, vertical_container, WindowId::new(2, 1));
        let _b2 = tree.add_window_under(layout, vertical_container, WindowId::new(2, 2));

        let screen = rect(0, 0, 1000, 1000);
        let config = Config::default();
        let (frames, groups) = tree.calculate_layout_and_groups(layout, screen, &config);

        assert_eq!(frames.len(), 3);
        assert_eq!(groups.len(), 0);
    }

    #[test]
    fn it_reserves_space_for_indicators_when_enabled() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);

        // Create a tabbed group
        let tabbed_group = tree.add_container(root, ContainerKind::Tabbed);
        let _tab1 = tree.add_window_under(layout, tabbed_group, WindowId::new(1, 1));
        let _tab2 = tree.add_window_under(layout, tabbed_group, WindowId::new(1, 2));

        let screen = rect(0, 0, 1000, 1000);

        // Test with indicators disabled
        let mut config_disabled = Config::default();
        config_disabled.settings.group_bars.enable = false;
        let (frames_disabled, groups_disabled) =
            tree.calculate_layout_and_groups(layout, screen, &config_disabled);

        // Test with indicators enabled
        let mut config_enabled = Config::default();
        config_enabled.settings.group_bars.enable = true;
        config_enabled.settings.group_bars.thickness = 20.0;
        let (frames_enabled, groups_enabled) =
            tree.calculate_layout_and_groups(layout, screen, &config_enabled);

        // Both should have same number of frames and groups
        assert_eq!(frames_disabled.len(), frames_enabled.len());
        assert_eq!(groups_disabled.len(), groups_enabled.len());
        assert_eq!(groups_enabled.len(), 1);

        // When disabled, indicator frame should be zero (no indicator to display)
        let group_disabled = &groups_disabled[0];
        assert_eq!(group_disabled.indicator_frame, rect(0, 0, 0, 0));

        // When enabled, indicator frame should be reserved space (top placement by default)
        let group_enabled = &groups_enabled[0];
        assert_eq!(group_enabled.indicator_frame, rect(0, 0, 1000, 20)); // Indicator frame at top

        // Window frames should be smaller when indicators are enabled
        // (accounting for the 20px reserved for indicator)
        let target_wid = WindowId::new(1, 1);
        let window_frame_disabled =
            frames_disabled.iter().find(|(wid, _)| *wid == target_wid).unwrap().1;
        let window_frame_enabled =
            frames_enabled.iter().find(|(wid, _)| *wid == target_wid).unwrap().1;

        assert_eq!(window_frame_disabled, rect(0, 0, 1000, 1000));
        assert_eq!(window_frame_enabled, rect(0, 20, 1000, 980));
    }

    #[test]
    fn it_respects_outer_gap() {
        let mut tree = LayoutTree::new();
        let layout = tree.create_layout();
        let root = tree.root(layout);
        let _a1 = tree.add_window_under(layout, root, WindowId::new(1, 1));
        let _a2 = tree.add_window_under(layout, root, WindowId::new(1, 2));

        let screen = rect(0, 0, 1000, 1000);
        let outer_gap = 10.0;

        // Test with non-fullscreen layout
        let mut config = Config::default();
        config.settings.outer_gap = outer_gap;
        let (frames, _) = tree.calculate_layout_and_groups(layout, screen, &config);

        // Without fullscreen, windows should be split with outer_gap applied at root
        // Screen (0,0,1000,1000) with outer_gap=10 becomes (10,10,980,980)
        // Then split horizontally: each window gets half of 980 = 490
        let window1_frame = frames.iter().find(|(wid, _)| *wid == WindowId::new(1, 1)).unwrap().1;
        let window2_frame = frames.iter().find(|(wid, _)| *wid == WindowId::new(1, 2)).unwrap().1;
        assert_eq!(
            window1_frame,
            rect(10, 10, 490, 980),
            "window1 before fullscreen"
        );
        assert_eq!(
            window2_frame,
            rect(500, 10, 490, 980),
            "window2 before fullscreen"
        );

        // Mark first window as fullscreen
        let node1 = tree.window_node(layout, WindowId::new(1, 1)).unwrap();
        tree.set_fullscreen(node1, true);

        let (frames_fullscreen, _) = tree.calculate_layout_and_groups(layout, screen, &config);

        // Fullscreen window should respect outer_gap (inset on all sides from 0,0,1000,1000)
        let window1_fullscreen_frame =
            frames_fullscreen.iter().find(|(wid, _)| *wid == WindowId::new(1, 1)).unwrap().1;

        // Expected: (10, 10, 980, 980) - fullscreen with outer_gap
        assert_eq!(
            window1_fullscreen_frame,
            rect(10, 10, 980, 980),
            "window1 fullscreen with outer_gap"
        );
    }

    #[test]
    fn root_orientation_auto_splits_the_longer_axis() {
        let portrait = CGSize { width: 100.0, height: 300.0 };
        let landscape = CGSize { width: 300.0, height: 100.0 };
        let square = CGSize { width: 100.0, height: 100.0 };

        assert_eq!(Orientation::Vertical, RootOrientation::Auto.resolve(portrait));
        assert_eq!(Orientation::Horizontal, RootOrientation::Auto.resolve(landscape));
        // A square screen has no longer axis; it falls back to landscape.
        assert_eq!(Orientation::Horizontal, RootOrientation::Auto.resolve(square));

        // The explicit settings ignore the screen entirely.
        assert_eq!(
            Orientation::Horizontal,
            RootOrientation::Horizontal.resolve(portrait)
        );
        assert_eq!(
            Orientation::Vertical,
            RootOrientation::Vertical.resolve(landscape)
        );
    }
}
