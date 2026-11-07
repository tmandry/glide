// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Manages visual indicators for window groups.
//!
//! The layout system calculates indicator frames and state. This actor is
//! responsible for managing the UI components themselves, and forwarding events
//! between them and the reactor.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSFloatingWindowLevel, NSNormalWindowLevel, NSRunningApplication,
    NSWindow, NSWindowStyleMask,
};
use objc2_core_foundation::CGRect;
use objc2_foundation::NSZeroRect;
use redact::Secret;
use tracing::debug;

use crate::actor::{self, reactor};
use crate::collections::HashMap;
use crate::config::Config;
use crate::model::{ContainerKind, GroupBarInfo, NodeId};
use crate::sys::app::NSRunningApplicationExt;
use crate::sys::screen::{CoordinateConverter, SpaceId};
use crate::sys::window_server::WindowId;
use crate::ui::group_bar::{
    GroupDisplayData, GroupIndicatorNSView, GroupKind, SegmentClickCallback, TabLabel,
};

#[derive(Debug)]
pub enum Event {
    /// Groups have been updated for a space - full replacement
    GroupsUpdated {
        space_id: SpaceId,
        groups: Vec<GroupBarInfo>,
        /// Titles of the windows in `groups`.
        titles: HashMap<WindowId, Secret<String>>,
    },
    WindowTitleChanged(WindowId, Secret<String>),
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
}

pub struct GroupBars {
    config: Arc<Config>,
    rx: Receiver,
    events_tx: reactor::Sender,
    mtm: MainThreadMarker,
    indicators: HashMap<SpaceId, HashMap<NodeId, Indicator>>,
    coordinate_converter: CoordinateConverter,
    active_spaces: Vec<Option<SpaceId>>,
}

struct Indicator {
    view: GroupIndicatorNSView,
    window: Retained<NSWindow>,
    /// The window shown by each tab, shared with the click callback.
    windows: Rc<RefCell<Vec<WindowId>>>,
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
    pub fn new(
        config: Arc<Config>,
        rx: Receiver,
        events_tx: reactor::Sender,
        mtm: MainThreadMarker,
    ) -> Self {
        Self {
            config,
            rx,
            events_tx,
            mtm,
            indicators: HashMap::default(),
            coordinate_converter: CoordinateConverter::default(),
            active_spaces: Vec::new(),
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
            Event::GroupsUpdated { space_id, groups, titles } => {
                self.handle_groups_updated(space_id, groups, &titles);
            }
            Event::WindowTitleChanged(wid, title) => {
                self.handle_title_changed(wid, title);
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
        }
    }

    fn handle_groups_updated(
        &mut self,
        space_id: SpaceId,
        groups: Vec<GroupBarInfo>,
        titles: &HashMap<WindowId, Secret<String>>,
    ) {
        let group_nodes: crate::collections::HashSet<NodeId> =
            groups.iter().map(|g| g.node_id).collect();
        let space_indicators = self.indicators.entry(space_id).or_default();
        space_indicators.retain(|&node_id, _| group_nodes.contains(&node_id));

        for group in groups {
            self.update_or_create_indicator(space_id, group, titles);
        }
    }

    fn handle_title_changed(&mut self, wid: WindowId, title: Secret<String>) {
        for indicator in self.indicators.values_mut().flat_map(|i| i.values_mut()) {
            let Some(index) = indicator.windows.borrow().iter().position(|&w| w == wid) else {
                continue;
            };
            let Some(mut group_data) = indicator.view.group_data() else {
                continue;
            };
            if let Some(tab) = group_data.tabs.get_mut(index) {
                tab.title = title.clone();
                indicator.view.update(group_data);
            }
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

    fn update_or_create_indicator(
        &mut self,
        space_id: SpaceId,
        group: GroupBarInfo,
        titles: &HashMap<WindowId, Secret<String>>,
    ) {
        let group_kind = match group.container_kind {
            ContainerKind::Tabbed => GroupKind::Horizontal,
            ContainerKind::Stacked => GroupKind::Vertical,
            _ => {
                tracing::warn!(?group.container_kind, "Unexpected container kind for group");
                return;
            }
        };

        let tabs = group
            .windows
            .iter()
            .map(|wid| TabLabel {
                title: titles.get(wid).cloned().unwrap_or_default(),
                icon: NSRunningApplication::with_process_id(wid.pid).and_then(|app| app.icon()),
            })
            .collect();

        let space_indicators = self.indicators.entry(space_id).or_default();
        let indicator = space_indicators.entry(group.node_id).or_insert_with(|| {
            let mut view =
                GroupIndicatorNSView::new(CGRect::ZERO, &self.config.settings.group_bars, self.mtm);
            let windows = Rc::new(RefCell::new(Vec::new()));
            let node_id = group.node_id;
            // Sends `event` with the window of the clicked tab.
            let tab_callback = |event: fn(WindowId) -> reactor::Event| -> SegmentClickCallback {
                let windows = windows.clone();
                let events_tx = self.events_tx.clone();
                Rc::new(move |segment_index| {
                    let Some(&wid) = windows.borrow().get(segment_index) else {
                        return;
                    };
                    let event = event(wid);
                    debug!(?node_id, segment_index, ?event, "Group bar tab clicked");
                    events_tx.send(event);
                })
            };
            view.set_click_callback(tab_callback(reactor::Event::GroupBarTabClicked));
            view.set_close_callback(tab_callback(reactor::Event::GroupBarTabCloseClicked));
            let events_tx = self.events_tx.clone();
            view.set_move_callback(Rc::new(move |from, to| {
                debug!(?node_id, from, to, "Group bar tab moved");
                events_tx.send(reactor::Event::GroupBarTabMoved { group: node_id, from, to });
            }));
            let window = make_indicator_window(self.mtm);
            window.setContentView(Some(view.view()));
            Indicator { view, window, windows }
        });
        if let Some(frame) = self.coordinate_converter.convert_rect(group.indicator_frame) {
            indicator.window.setFrame_display(frame, false);
        }
        indicator.view.update(GroupDisplayData {
            group_kind,
            total_count: group.windows.len(),
            selected_index: group.selected_index,
            frame: group.indicator_frame,
            is_selected: group.is_selected,
            tabs,
        });
        *indicator.windows.borrow_mut() = group.windows;
        indicator.window.setIsVisible(group.is_visible);
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

    window
}
