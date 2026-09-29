// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Manages visual indicators for window groups.
//!
//! The layout system calculates indicator frames and state. This actor is
//! responsible for managing the UI components themselves, and forwarding events
//! between them and the reactor.

use std::rc::Rc;
use std::sync::Arc;

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAnimatablePropertyContainer, NSAnimationContext, NSBackingStoreType, NSColor,
    NSFloatingWindowLevel, NSNormalWindowLevel, NSWindow, NSWindowCollectionBehavior,
    NSWindowStyleMask,
};
use objc2_core_foundation::CGRect;
use objc2_foundation::NSZeroRect;
use tracing::debug;

use crate::actor;
use crate::collections::HashMap;
use crate::config::Config;
use crate::model::{ContainerKind, GroupBarInfo, NodeId};
use crate::sys::screen::{CoordinateConverter, SpaceId};
use crate::ui::group_bar::{GroupDisplayData, GroupIndicatorNSView, GroupKind};

#[derive(Debug)]
pub enum Event {
    /// Groups have been updated for a space - full replacement
    GroupsUpdated {
        space_id: SpaceId,
        groups: Vec<GroupBarInfo>,
    },
    /// Selection changed within a specific group
    GroupSelectionChanged {
        space_id: SpaceId,
        node_id: NodeId,
        selected_index: usize,
    },
    ScreenParametersChanged(Vec<Option<SpaceId>>, CoordinateConverter),
    SpaceChanged(Vec<Option<SpaceId>>),
    SpaceDisabled(SpaceId),
    GlobalDisabled,
    ConfigChanged(Arc<Config>),
    Hide,
    Show,
}

pub struct GroupBars {
    config: Arc<Config>,
    rx: Receiver,
    mtm: MainThreadMarker,
    indicators: HashMap<SpaceId, HashMap<NodeId, Indicator>>,
    coordinate_converter: CoordinateConverter,
    active_spaces: Vec<Option<SpaceId>>,
    hidden: bool,
}

struct Indicator {
    view: GroupIndicatorNSView,
    window: Retained<NSWindow>,
}

impl Drop for Indicator {
    fn drop(&mut self) {
        self.view.clear();
        self.window.close();
    }
}

pub type Sender = actor::Sender<Event>;
pub type Receiver = actor::Receiver<Event>;

impl GroupBars {
    pub fn new(config: Arc<Config>, rx: Receiver, mtm: MainThreadMarker) -> Self {
        Self {
            config,
            rx,
            mtm,
            indicators: HashMap::default(),
            coordinate_converter: CoordinateConverter::default(),
            active_spaces: Vec::new(),
            hidden: false,
        }
    }

    pub async fn run(mut self) {
        while let Some((span, event)) = self.rx.recv().await {
            let _guard = span.enter();
            self.handle_event(event);
        }
    }

    fn handle_event(&mut self, event: Event) {
        debug!(?event);
        match event {
            Event::GroupsUpdated { space_id, groups } => {
                self.handle_groups_updated(space_id, groups);
            }
            Event::GroupSelectionChanged {
                space_id,
                node_id,
                selected_index,
            } => {
                self.handle_selection_changed(space_id, node_id, selected_index);
            }
            Event::ScreenParametersChanged(spaces, converter) => {
                self.active_spaces = spaces;
                self.coordinate_converter = converter;
            }
            Event::SpaceChanged(spaces) => {
                self.active_spaces = spaces;
            }
            Event::ConfigChanged(config) => {
                if config.settings.group_bars != self.config.settings.group_bars {
                    for indicator in self.indicators.values_mut().flat_map(|i| i.values_mut()) {
                        indicator.view.set_config(&config.settings.group_bars);
                    }
                }
                self.config = config;
            }
            Event::SpaceDisabled(space) => {
                self.indicators.remove(&space);
            }
            Event::GlobalDisabled => {
                self.indicators.clear();
            }
            Event::Hide => self.set_hidden(true),
            Event::Show => self.set_hidden(false),
        }
    }

    fn set_hidden(&mut self, hidden: bool) {
        let config = &self.config.settings.group_bars;
        self.hidden = hidden && config.fade;
        let alpha = if self.hidden { 0.0 } else { 1.0 };
        NSAnimationContext::beginGrouping();
        NSAnimationContext::currentContext().setDuration(config.fade_duration);
        for indicator in self.indicators.values().flat_map(|i| i.values()) {
            indicator.window.animator().setAlphaValue(alpha);
        }
        NSAnimationContext::endGrouping();
    }

    fn handle_groups_updated(&mut self, space_id: SpaceId, groups: Vec<GroupBarInfo>) {
        let group_nodes: crate::collections::HashSet<NodeId> = groups
            .iter()
            // If indicators are disabled, we will get group info but the frames
            // will be empty.
            .filter(|g| !g.indicator_frame.is_empty())
            .map(|g| g.node_id)
            .collect();
        let space_indicators = self.indicators.entry(space_id).or_default();
        space_indicators.retain(|&node_id, _| group_nodes.contains(&node_id));

        for group in groups {
            self.update_or_create_indicator(space_id, group);
        }
    }

    fn handle_selection_changed(
        &mut self,
        space_id: SpaceId,
        node_id: NodeId,
        selected_index: usize,
    ) {
        if let Some(indicator) = self.indicators.entry(space_id).or_default().get_mut(&node_id) {
            if let Some(mut group_data) = indicator.view.group_data() {
                group_data.selected_index = selected_index;
                indicator.view.update(group_data);
            }
        }
    }

    fn handle_indicator_clicked(node_id: NodeId, segment_index: usize) {
        tracing::debug!(?node_id, segment_index, "Group indicator clicked");
    }

    fn update_or_create_indicator(&mut self, space_id: SpaceId, group: GroupBarInfo) {
        let group_kind = match group.container_kind {
            ContainerKind::Tabbed => GroupKind::Horizontal,
            ContainerKind::Stacked => GroupKind::Vertical,
            _ => {
                tracing::warn!(?group.container_kind, "Unexpected container kind for group");
                return;
            }
        };

        let space_indicators = self.indicators.entry(space_id).or_default();
        let indicator = space_indicators.entry(group.node_id).or_insert_with(|| {
            let mut view =
                GroupIndicatorNSView::new(CGRect::ZERO, &self.config.settings.group_bars, self.mtm);
            view.set_click_callback(Rc::new(move |segment_index| {
                Self::handle_indicator_clicked(group.node_id, segment_index);
            }));
            let window = make_indicator_window(self.mtm);
            window.setContentView(Some(view.view()));
            Indicator { view, window }
        });
        if let Some(frame) = self.coordinate_converter.convert_rect(group.indicator_frame) {
            indicator.window.setFrame_display(frame, false);
        }
        indicator.view.update(GroupDisplayData {
            group_kind,
            total_count: group.total_count,
            selected_index: group.selected_index,
            frame: group.indicator_frame,
            is_selected: group.is_selected,
        });
        indicator.window.setIsVisible(group.is_visible);
        if self.hidden {
            indicator.window.setAlphaValue(0.0);
        }
        indicator.window.setLevel(if group.is_on_top {
            NSFloatingWindowLevel
        } else {
            NSNormalWindowLevel
        });
        if group.is_visible && self.active_spaces.contains(&Some(space_id)) {
            // TODO: There's a risk that we're no longer on the space we think
            // we're on and this will cause the indicator to be assigned to the
            // wrong space (potentially multiple spaces because it is floating).
            indicator.window.makeKeyAndOrderFront(None);
        }
    }
}

fn make_indicator_window(mtm: MainThreadMarker) -> Retained<NSWindow> {
    // TODO: This shoould probably happen at the UI layer instead of the actor.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            NSZeroRect,
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            true,
        )
    };
    // SAFETY: This actually prevents a segfault (double release) when calling
    // window.close().
    unsafe { window.setReleasedWhenClosed(false) };

    // Configure as overlay window
    window.setLevel(NSFloatingWindowLevel);
    window.setBackgroundColor(Some(&NSColor::clearColor()));
    window.setOpaque(false);
    window.setIgnoresMouseEvents(true);
    // Don't animate the indicator along with windows in Exposé.
    window.setCollectionBehavior(NSWindowCollectionBehavior::Stationary);

    window
}
