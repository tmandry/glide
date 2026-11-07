// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Group indicator UI component for displaying window group information.
//!
//! This module provides a segmented bar for visualizing groups:
//! - Horizontal groups (tabbed): horizontal bar at top
//! - Vertical groups (stacked): vertical bar on the side
//! - Each segment represents one child in the group
//! - Selected segment is highlighted

use std::cell::RefCell;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{AnyThread, DeclaredClass, MainThreadOnly, msg_send};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSColor, NSEvent, NSFont, NSGlassEffectView, NSImage,
    NSTrackingArea, NSTrackingAreaOptions, NSView, NSVisualEffectBlendingMode,
    NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView, NSWindowOrderingMode,
};
use objc2_core_foundation::{CFType, CGAffineTransform, CGPoint, CGRect, CGSize};
use objc2_foundation::{MainThreadMarker, NSString};
use objc2_quartz_core::{
    CALayer, CATextLayer, CATransaction, kCAAlignmentCenter, kCAGravityResizeAspect,
    kCATruncationEnd,
};
use redact::Secret;

use crate::config::{Color, GroupBarStyle, GroupBars};
use crate::sys::geometry::CGRectExt;

pub trait ColorExt {
    fn to_nscolor(&self) -> Retained<NSColor>;
}

impl ColorExt for Color {
    fn to_nscolor(&self) -> Retained<NSColor> {
        let c = |v: u8| f64::from(v) / 255.0;
        NSColor::colorWithRed_green_blue_alpha(c(self.r), c(self.g), c(self.b), c(self.a))
    }
}

/// Group orientation for determining bar placement
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GroupKind {
    Horizontal, // Tabbed groups - bar at top
    Vertical,   // Stacked groups - bar on right
}

/// High-level data about a group for display
#[derive(Debug, Clone)]
pub struct GroupDisplayData {
    pub group_kind: GroupKind,
    pub total_count: usize,
    pub selected_index: usize,
    pub frame: CGRect,
    pub is_selected: bool,
    /// Labels for each segment. May be shorter than `total_count` if some are
    /// unknown.
    pub tabs: Vec<TabLabel>,
}

#[derive(Debug, Clone, Default)]
pub struct TabLabel {
    pub title: Secret<String>,
    pub icon: Option<Retained<NSImage>>,
}

pub type SegmentClickCallback = Rc<dyn Fn(usize)>;
/// Called with the old and new index of a tab dragged to a new position.
pub type TabMoveCallback = Rc<dyn Fn(usize, usize)>;

/// How far the mouse must move before a press on a tab becomes a drag.
const DRAG_THRESHOLD: f64 = 8.0;

/// A tab being pressed or dragged.
struct TabDrag {
    index: usize,
    start_x: f64,
    /// How far the tab has been dragged, once past the threshold.
    offset: Option<f64>,
}

const LABEL_PADDING: f64 = 8.0;
const LABEL_SPACING: f64 = 5.0;

/// Sizes of the label elements for a bar of the given bounds.
struct LabelMetrics {
    font_size: f64,
    /// Side of the square icon and close button.
    side: f64,
    line_height: f64,
}

impl LabelMetrics {
    fn new(bounds: CGRect) -> Self {
        let font_size = (bounds.size.height * 0.5).clamp(8.0, 14.0);
        Self {
            font_size,
            side: (font_size + 3.0).min(bounds.size.height),
            line_height: (font_size * 1.3).min(bounds.size.height),
        }
    }
}

fn shows_labels(config: &GroupBars, group_data: &GroupDisplayData) -> bool {
    (config.show_titles || config.show_icons) && group_data.group_kind == GroupKind::Horizontal
}

/// Sets the font of a text layer.
fn set_font(layer: &CATextLayer, font: &NSFont) {
    // SAFETY: NSFont is toll-free bridged with CTFont, which this property
    // accepts.
    unsafe { layer.setFont(Some(&*(font as *const NSFont as *const CFType))) };
}

/// Inner state for the indicator view
pub struct IndicatorState {
    config: GroupBars,
    backdrop: Option<GlassBackdrop>,
    group_data: Option<GroupDisplayData>,
    background_layer: Option<Retained<CALayer>>,
    separator_layers: Vec<Retained<CALayer>>,
    selected_layer: Option<Retained<CALayer>>,
    label_layers: Vec<LabelLayers>,
    close_button: Option<CloseButton>,
    hover_layer: Option<Retained<CALayer>>,
    /// The segment under the mouse.
    hovered_index: Option<usize>,
    drag: Option<TabDrag>,
    click_callback: Option<SegmentClickCallback>,
    close_callback: Option<SegmentClickCallback>,
    move_callback: Option<TabMoveCallback>,
}

impl IndicatorState {
    /// How far the tab at `index` is displaced by a drag.
    fn drag_offset(&self, index: usize) -> f64 {
        match &self.drag {
            Some(TabDrag {
                index: i, offset: Some(offset), ..
            }) if *i == index => *offset,
            _ => 0.0,
        }
    }

    fn is_dragging(&self) -> bool {
        self.drag.as_ref().is_some_and(|drag| drag.offset.is_some())
    }
}

/// A square with an ✕ drawn from two crossed strokes.
struct CloseButton {
    square: Retained<CALayer>,
    strokes: [Retained<CALayer>; 2],
}

impl CloseButton {
    fn new(parent_layer: &CALayer) -> Self {
        let square = CALayer::layer();
        let strokes = [CALayer::layer(), CALayer::layer()];
        for (stroke, angle) in strokes.iter().zip([1.0, -1.0f64]) {
            let (sin, cos) = (angle * std::f64::consts::FRAC_PI_4).sin_cos();
            stroke.setAffineTransform(CGAffineTransform {
                a: cos,
                b: sin,
                c: -sin,
                d: cos,
                tx: 0.0,
                ty: 0.0,
            });
            square.addSublayer(stroke);
        }
        parent_layer.addSublayer(&square);
        Self { square, strokes }
    }

    fn update(&self, frame: CGRect, config: &GroupBars) {
        let side = frame.size.width;
        self.square.setFrame(frame);
        self.square.setCornerRadius(side * 0.25);
        self.square
            .setBackgroundColor(Some(&config.close_background_color.to_nscolor().CGColor()));
        let stroke_color = config.close_color;
        const STROKE_WIDTH: f64 = 1.5;
        for stroke in &self.strokes {
            stroke.setBounds(CGRect::new(CGPoint::ZERO, CGSize::new(side * 0.5, STROKE_WIDTH)));
            stroke.setPosition(CGPoint::new(side / 2.0, side / 2.0));
            stroke.setCornerRadius(STROKE_WIDTH / 2.0);
            stroke.setBackgroundColor(Some(&stroke_color.to_nscolor().CGColor()));
        }
        self.square.setHidden(false);
    }
}

struct LabelLayers {
    icon: Retained<CALayer>,
    title: Retained<CATextLayer>,
}

impl LabelLayers {
    fn new(parent_layer: &CALayer) -> Self {
        let icon = CALayer::layer();
        // SAFETY: The gravity constant is a valid value for this property.
        icon.setContentsGravity(unsafe { kCAGravityResizeAspect });
        let title = CATextLayer::layer();
        title.setTruncationMode(unsafe { kCATruncationEnd });
        title.setAlignmentMode(unsafe { kCAAlignmentCenter });
        parent_layer.addSublayer(&icon);
        parent_layer.addSublayer(&title);
        Self { icon, title }
    }

    fn remove(&self) {
        self.icon.removeFromSuperlayer();
        self.title.removeFromSuperlayer();
    }
}

objc2::define_class!(
    #[unsafe(super(NSView))]
    #[ivars = RefCell<IndicatorState>]
    pub struct ClickableIndicatorView;

    impl ClickableIndicatorView {
        // Take the click that activates the window too, so one click selects a tab.
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let location = self.convertPoint_fromView(event.locationInWindow(), None);

            let state = self.ivars().borrow();
            let Some(group_data) = &state.group_data else { return };
            let bounds = self.bounds();
            let Some(segment_index) = Self::segment_at_point_static(
                location,
                group_data,
                &bounds,
            ) else {
                return
            };
            let on_close_button = shows_labels(&state.config, group_data)
                && Self::close_button_frame(group_data, bounds, segment_index).contains(location);
            let callback = if on_close_button {
                state.close_callback.clone()
            } else {
                state.click_callback.clone()
            };
            let can_drag = !on_close_button && group_data.group_kind == GroupKind::Horizontal;

            // Drop the active borrow before invoking the callback.
            drop(state);
            if can_drag {
                self.ivars().borrow_mut().drag = Some(TabDrag {
                    index: segment_index,
                    start_x: location.x,
                    offset: None,
                });
            }
            if let Some(callback) = callback {
                callback(segment_index);
            }
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            let location = self.convertPoint_fromView(event.locationInWindow(), None);
            {
                let mut state = self.ivars().borrow_mut();
                let Some(drag) = &mut state.drag else { return };
                let offset = location.x - drag.start_x;
                if drag.offset.is_none() && offset.abs() < DRAG_THRESHOLD {
                    return;
                }
                drag.offset = Some(offset);
            }
            // Track the mouse instead of animating toward it.
            CATransaction::begin();
            CATransaction::setDisableActions(true);
            self.update_layers();
            CATransaction::commit();
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            let location = self.convertPoint_fromView(event.locationInWindow(), None);
            let mut state = self.ivars().borrow_mut();
            let Some(drag) = state.drag.take() else { return };
            let (Some(_), Some(group_data)) = (drag.offset, &state.group_data) else { return };
            let count = group_data.total_count;
            let bounds = self.bounds();
            let to = ((location.x / bounds.size.width) * count as f64)
                .floor()
                .clamp(0.0, count.saturating_sub(1) as f64) as usize;
            let callback = state.move_callback.clone();
            drop(state);
            self.update_layers();
            if to != drag.index
                && let Some(callback) = callback
            {
                callback(drag.index, to);
            }
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            let location = self.convertPoint_fromView(event.locationInWindow(), None);
            let hovered_index = {
                let state = self.ivars().borrow();
                state.group_data.as_ref().and_then(|group_data| {
                    Self::segment_at_point_static(location, group_data, &self.bounds())
                })
            };
            self.set_hovered_index(hovered_index);
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            self.set_hovered_index(None);
        }
    }
);

/// NSView wrapper for displaying group indicators using CALayer
pub struct GroupIndicatorNSView {
    /// Holds the backdrop (if any) and the indicator view on top of it.
    root: Retained<NSView>,
    view: Retained<ClickableIndicatorView>,
}

/// The material drawn behind the segments when `style = "glass"`.
enum GlassBackdrop {
    /// Liquid Glass, macOS 26 and later.
    Glass(Retained<NSGlassEffectView>),
    /// A behind-window blur, for earlier versions of macOS.
    Blur(Retained<NSVisualEffectView>),
}

impl GlassBackdrop {
    fn new(frame: CGRect, mtm: MainThreadMarker) -> Self {
        if AnyClass::get(c"NSGlassEffectView").is_some() {
            GlassBackdrop::Glass(NSGlassEffectView::initWithFrame(
                NSGlassEffectView::alloc(mtm),
                frame,
            ))
        } else {
            let view = NSVisualEffectView::initWithFrame(NSVisualEffectView::alloc(mtm), frame);
            view.setMaterial(NSVisualEffectMaterial::HUDWindow);
            view.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
            view.setState(NSVisualEffectState::Active);
            view.setWantsLayer(true);
            GlassBackdrop::Blur(view)
        }
    }

    fn view(&self) -> &NSView {
        match self {
            GlassBackdrop::Glass(v) => v,
            GlassBackdrop::Blur(v) => v,
        }
    }

    /// Glass takes the tint directly. The blur has no tint, so the background
    /// layer paints over it instead; returns whether that is needed.
    #[must_use]
    fn set_appearance(&self, corner_radius: f64, tint: &Color) -> bool {
        match self {
            GlassBackdrop::Glass(v) => {
                v.setCornerRadius(corner_radius);
                v.setTintColor(Some(&tint.to_nscolor()));
                false
            }
            GlassBackdrop::Blur(v) => {
                if let Some(layer) = v.layer() {
                    layer.setCornerRadius(corner_radius);
                    layer.setMasksToBounds(true);
                }
                true
            }
        }
    }
}

fn fill_superview(view: &NSView) {
    view.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
}

/// The configured radius, limited to what the bar's shape allows.
fn corner_radius(config: &GroupBars, bounds: CGRect) -> f64 {
    config
        .corner_radius
        .clamp(0.0, f64::min(bounds.size.width, bounds.size.height) / 2.0)
}

impl ClickableIndicatorView {
    fn set_hovered_index(&self, hovered_index: Option<usize>) {
        let mut state = self.ivars().borrow_mut();
        if state.hovered_index != hovered_index {
            state.hovered_index = hovered_index;
            drop(state);
            self.update_hover_layers();
        }
    }

    fn close_button_frame(group_data: &GroupDisplayData, bounds: CGRect, index: usize) -> CGRect {
        let segment = Self::calculate_segment_frame(group_data, bounds, index);
        let side = LabelMetrics::new(bounds).side;
        CGRect::new(
            CGPoint::new(
                segment.max().x - LABEL_PADDING - side,
                segment.mid().y - side / 2.0,
            ),
            CGSize::new(side, side),
        )
    }

    /// Highlights the hovered tab of a labeled bar and shows its close button.
    fn update_hover_layers(&self) {
        let bounds = self.bounds();
        let Some(parent_layer) = self.layer() else { return };
        let mut state = self.ivars().borrow_mut();
        let state = &mut *state;
        let hovered =
            state
                .group_data
                .as_ref()
                .zip(state.hovered_index)
                .filter(|(group_data, index)| {
                    shows_labels(&state.config, group_data)
                        && *index < group_data.total_count
                        && !state.is_dragging()
                });
        // Jump straight to the hovered tab rather than sliding between tabs.
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        match hovered {
            Some((group_data, index)) => {
                let frame = Self::close_button_frame(group_data, bounds, index);
                state
                    .close_button
                    .get_or_insert_with(|| CloseButton::new(&parent_layer))
                    .update(frame, &state.config);

                // The selected tab is already highlighted.
                let highlight = index != group_data.selected_index;
                let hover_layer = state.hover_layer.get_or_insert_with(|| {
                    let layer = CALayer::layer();
                    // Below the labels, above the bar's background.
                    parent_layer.insertSublayer_above(&layer, state.background_layer.as_deref());
                    layer
                });
                hover_layer.setFrame(Self::calculate_segment_frame(group_data, bounds, index));
                hover_layer
                    .setBackgroundColor(Some(&state.config.hover_color.to_nscolor().CGColor()));
                hover_layer.setHidden(!highlight);
            }
            None => {
                if let Some(button) = &state.close_button {
                    button.square.setHidden(true);
                }
                if let Some(layer) = &state.hover_layer {
                    layer.setHidden(true);
                }
            }
        }
        CATransaction::commit();
    }

    fn segment_at_point_static(
        point: CGPoint,
        group_data: &GroupDisplayData,
        bounds: &CGRect,
    ) -> Option<usize> {
        if !bounds.contains(point) {
            return None;
        }

        if group_data.total_count == 0 {
            return None;
        }

        let pct = match group_data.group_kind {
            GroupKind::Horizontal => point.x / bounds.size.width,
            // The first segment is at the top, and y increases upward.
            GroupKind::Vertical => 1.0 - point.y / bounds.size.height,
        };
        let segment_index = (pct * group_data.total_count as f64).floor() as usize;
        Some(segment_index.min(group_data.total_count - 1))
    }
}

impl GroupIndicatorNSView {
    pub fn new(frame: CGRect, config: &GroupBars, mtm: MainThreadMarker) -> Self {
        let state = IndicatorState {
            config: config.clone(),
            backdrop: None,
            group_data: None,
            background_layer: None,
            separator_layers: Vec::new(),
            selected_layer: None,
            label_layers: Vec::new(),
            close_button: None,
            hover_layer: None,
            hovered_index: None,
            click_callback: None,
            close_callback: None,
            drag: None,
            move_callback: None,
        };
        let view = ClickableIndicatorView::alloc(mtm).set_ivars(RefCell::new(state));
        // The view fills `root`, which takes `frame`.
        let bounds = CGRect::new(CGPoint::ZERO, frame.size);
        let view: Retained<_> = unsafe { msg_send![super(view), initWithFrame: bounds] };

        view.setWantsLayer(true);
        let parent_layer = view.makeBackingLayer();
        view.setLayer(Some(&parent_layer));

        // SAFETY: The mask must not have a superlayer; we use a brand new layer.
        unsafe { parent_layer.setMask(Some(&CALayer::layer())) };

        // Glide is rarely the active app, so track the mouse regardless.
        let tracking_area = unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(
                NSTrackingArea::alloc(),
                CGRect::ZERO,
                NSTrackingAreaOptions::MouseEnteredAndExited
                    | NSTrackingAreaOptions::MouseMoved
                    | NSTrackingAreaOptions::ActiveAlways
                    | NSTrackingAreaOptions::InVisibleRect,
                Some(&view),
                None,
            )
        };
        view.addTrackingArea(&tracking_area);

        let root = NSView::initWithFrame(NSView::alloc(mtm), frame);
        fill_superview(&view);
        root.addSubview(&view);

        view.update_backdrop();
        Self { root, view }
    }

    pub fn view(&self) -> &NSView {
        &self.root
    }

    pub fn update(&mut self, group_data: GroupDisplayData) {
        let old_selected = {
            let state = self.view.ivars().borrow();
            state.group_data.as_ref().map(|d| d.selected_index)
        };

        self.view.ivars().borrow_mut().group_data = Some(group_data.clone());
        self.view.update_layers();

        // Animate if selection changed
        if let Some(old_index) = old_selected {
            if old_index != group_data.selected_index {
                self.view.animate_selection_change(old_index, group_data.selected_index);
            }
        }
    }

    pub fn set_config(&mut self, config: &GroupBars) {
        self.view.ivars().borrow_mut().config = config.clone();
        self.view.update_backdrop();
        self.view.update_layers();
    }

    pub fn clear(&mut self) {
        self.view.ivars().borrow_mut().group_data = None;
        self.view.clear_layers();
    }

    pub fn set_click_callback(&mut self, callback: SegmentClickCallback) {
        self.view.ivars().borrow_mut().click_callback = Some(callback);
    }

    /// Called when the close button of a tab is clicked.
    pub fn set_close_callback(&mut self, callback: SegmentClickCallback) {
        self.view.ivars().borrow_mut().close_callback = Some(callback);
    }

    /// Called when a tab is dragged to a new position.
    pub fn set_move_callback(&mut self, callback: TabMoveCallback) {
        self.view.ivars().borrow_mut().move_callback = Some(callback);
    }

    pub fn group_data(&self) -> Option<GroupDisplayData> {
        self.view.ivars().borrow().group_data.clone()
    }

    /// Handle a click at the given segment index (for demo purposes)
    pub fn click_segment(&mut self, segment_index: usize) {
        if let Some(group_data) = self.group_data() {
            if segment_index < group_data.total_count {
                let mut updated_data = group_data;
                updated_data.selected_index = segment_index;
                self.update(updated_data);
            }
        }
    }

    /// Handle mouse down events for segment clicking
    pub fn handle_mouse_down(&self, event: &NSEvent) {
        let state = self.view.ivars().borrow();
        let Some(group_data) = &state.group_data else {
            return;
        };

        let Some(callback) = &state.click_callback else {
            return;
        };

        // Convert event location to view coordinates
        let location = self.view.convertPoint_fromView(event.locationInWindow(), None);

        // Determine which segment was clicked
        if let Some(segment_index) = self.segment_at_point(location, group_data) {
            callback(segment_index);
        }
    }

    /// Check if a point is inside this view and return the segment index if clicked
    pub fn check_click(&self, window_point: CGPoint) -> Option<usize> {
        let state = self.view.ivars().borrow();
        let Some(group_data) = &state.group_data else {
            return None;
        };

        // Convert window point to view coordinates
        let view_point = self.view.convertPoint_fromView(window_point, None);

        self.segment_at_point(view_point, group_data)
    }

    /// Determine which segment contains the given point
    pub fn segment_at_point(&self, point: CGPoint, group_data: &GroupDisplayData) -> Option<usize> {
        let bounds = self.view.bounds();
        ClickableIndicatorView::segment_at_point_static(point, group_data, &bounds)
    }
}

impl ClickableIndicatorView {
    /// Adds or removes the backdrop to match the configured style.
    fn update_backdrop(&self) {
        // SAFETY: The view's superview is the root view, an ordinary NSView.
        let Some(root) = (unsafe { self.superview() }) else {
            return;
        };
        let mut state = self.ivars().borrow_mut();
        match (state.config.style, &state.backdrop) {
            (GroupBarStyle::Glass, None) => {
                let backdrop = GlassBackdrop::new(root.bounds(), MainThreadMarker::from(self));
                fill_superview(backdrop.view());
                root.addSubview_positioned_relativeTo(
                    backdrop.view(),
                    NSWindowOrderingMode::Below,
                    Some(self),
                );
                state.backdrop = Some(backdrop);
            }
            (GroupBarStyle::Solid, Some(backdrop)) => {
                backdrop.view().removeFromSuperview();
                state.backdrop = None;
            }
            _ => {}
        }
    }

    /// Clear all layers
    fn clear_layers(&self) {
        unsafe {
            if let Some(parent_layer) = self.layer() {
                if let Some(sublayers) = parent_layer.sublayers() {
                    // Convert to vec before iterating to avoid mutation panic.
                    for sublayer in sublayers.to_vec() {
                        sublayer.removeFromSuperlayer();
                    }
                }
            }
        }

        let mut state = self.ivars().borrow_mut();
        state.background_layer = None;
        state.separator_layers.clear();
        state.selected_layer = None;
        state.label_layers.clear();
        state.close_button = None;
        state.hover_layer = None;
    }

    /// Update the layer structure to match current group data
    fn update_layers(&self) {
        let group_data = match self.ivars().borrow().group_data.clone() {
            Some(data) => data,
            None => {
                self.clear_layers();
                return;
            }
        };

        let bounds = self.bounds();

        let parent_layer = match self.layer() {
            Some(layer) => layer,
            None => return,
        };

        // Ensure we have the right number of separator layers
        self.ensure_separator_layers(group_data.total_count);

        self.update_background_layer(&parent_layer, &group_data, bounds);
        self.update_separator_layers(&parent_layer, &group_data, bounds);
        self.update_selected_layer(&parent_layer, &group_data, bounds);
        self.update_label_layers(&parent_layer, &group_data, bounds);
        self.update_hover_layers();
    }

    /// Lays out the icon and title of each tab. Only tabbed groups are labeled.
    fn update_label_layers(
        &self,
        parent_layer: &CALayer,
        group_data: &GroupDisplayData,
        bounds: CGRect,
    ) {
        let scale = self.window().map_or(2.0, |w| w.backingScaleFactor());
        let mut state = self.ivars().borrow_mut();
        let show_titles = state.config.show_titles;
        let show_icons = state.config.show_icons;
        let wanted = if shows_labels(&state.config, group_data) {
            group_data.tabs.len().min(group_data.total_count)
        } else {
            0
        };
        while state.label_layers.len() > wanted {
            state.label_layers.pop().unwrap().remove();
        }
        while state.label_layers.len() < wanted {
            state.label_layers.push(LabelLayers::new(parent_layer));
        }

        let LabelMetrics { font_size, side, line_height } = LabelMetrics::new(bounds);
        let regular = NSFont::systemFontOfSize(font_size);
        let bold = NSFont::boldSystemFontOfSize(font_size);
        let text_color = state.config.text_color;
        let inactive_text_color = state.config.inactive_text_color;

        for (index, (layers, tab)) in state.label_layers.iter().zip(&group_data.tabs).enumerate() {
            let is_active = index == group_data.selected_index;
            let mut segment = Self::calculate_segment_frame(group_data, bounds, index);
            let offset = state.drag_offset(index);
            segment.origin.x += offset;
            let z_position = if offset == 0.0 { 0.0 } else { 1.0 };
            layers.icon.setZPosition(z_position);
            layers.title.setZPosition(z_position);
            let min_x = segment.origin.x + LABEL_PADDING;
            let max_x = segment.max().x - LABEL_PADDING;

            let icon = tab.icon.as_ref().filter(|_| show_icons);
            layers.icon.setHidden(icon.is_none());
            if let Some(icon) = icon {
                layers.icon.setFrame(CGRect::new(
                    CGPoint::new(min_x, segment.mid().y - side / 2.0),
                    CGSize::new(side, side),
                ));
                // SAFETY: CALayer accepts an NSImage as its contents.
                unsafe { layers.icon.setContents(Some(icon.as_ref() as &AnyObject)) };
                layers.icon.setOpacity(if is_active { 1.0 } else { 0.7 });
            }

            layers.title.setHidden(!show_titles);
            if show_titles {
                // The icon sits to the left of the title and the close button
                // to its right; reserving the same room on both sides keeps the
                // title centered in the segment.
                let reserved = side + LABEL_SPACING;
                let width = (max_x - min_x - 2.0 * reserved).max(0.0);
                layers.title.setFrame(CGRect::new(
                    CGPoint::new(min_x + reserved, segment.mid().y - line_height / 2.0),
                    CGSize::new(width, line_height),
                ));
                let title = NSString::from_str(tab.title.expose_secret());
                // SAFETY: NSString is toll-free bridged with CFString, which
                // this property accepts.
                unsafe { layers.title.setString(Some(&title)) };
                set_font(&layers.title, if is_active { &bold } else { &regular });
                layers.title.setFontSize(font_size);
                let color = if is_active {
                    text_color
                } else {
                    inactive_text_color
                };
                layers.title.setForegroundColor(Some(&color.to_nscolor().CGColor()));
                layers.title.setContentsScale(scale);
            }
        }
    }

    /// Ensure we have the correct number of separator layers
    fn ensure_separator_layers(&self, total_count: usize) {
        // We need (total_count - 1) separators between segments
        let needed_count = if total_count > 1 { total_count - 1 } else { 0 };

        let mut state = self.ivars().borrow_mut();

        // Remove excess layers
        while state.separator_layers.len() > needed_count {
            if let Some(layer) = state.separator_layers.pop() {
                layer.removeFromSuperlayer();
            }
        }

        // Add missing layers
        while state.separator_layers.len() < needed_count {
            let layer = CALayer::layer();
            state.separator_layers.push(layer);
        }
    }

    fn update_background_layer(
        &self,
        parent_layer: &CALayer,
        group_data: &GroupDisplayData,
        bounds: CGRect,
    ) {
        let mut state = self.ivars().borrow_mut();

        let background_layer = if let Some(existing) = &state.background_layer {
            existing.clone()
        } else {
            let layer = CALayer::layer();
            parent_layer.addSublayer(&layer);
            state.background_layer = Some(layer.clone());
            layer
        };

        // Use full view bounds for the background layer
        background_layer.setFrame(bounds);

        // Update appearance
        let bg_color = if group_data.is_selected {
            state.config.background_color
        } else {
            state.config.inactive_background_color
        };
        let paint_background = match &state.backdrop {
            Some(backdrop) => {
                backdrop.set_appearance(corner_radius(&state.config, bounds), &bg_color)
            }
            None => true,
        };
        let border_color = state.config.border_color.to_nscolor();
        background_layer.setBackgroundColor(
            paint_background.then(|| bg_color.to_nscolor().CGColor()).as_deref(),
        );
        background_layer.setBorderColor(Some(&border_color.CGColor()));
        background_layer.setBorderWidth(state.config.border_width);

        // Also update the mask here.
        if let Some(mask) = parent_layer.mask() {
            mask.setFrame(bounds);
            mask.setCornerRadius(corner_radius(&state.config, bounds));
            mask.setBackgroundColor(Some(&NSColor::whiteColor().CGColor()));
        }
    }

    fn update_separator_layers(
        &self,
        parent_layer: &CALayer,
        group_data: &GroupDisplayData,
        bounds: CGRect,
    ) {
        if group_data.total_count <= 1 {
            return;
        }

        let segment_length = match group_data.group_kind {
            GroupKind::Horizontal => bounds.size.width / group_data.total_count as f64,
            GroupKind::Vertical => bounds.size.height / group_data.total_count as f64,
        };

        let state = self.ivars().borrow();
        let separator_color = state.config.border_color.to_nscolor().CGColor();
        for (index, layer) in state.separator_layers.iter().enumerate() {
            // Calculate separator position (between segments)
            let separator_pos = (index + 1) as f64 * segment_length;

            let (sep_x, sep_y, sep_width, sep_height) = match group_data.group_kind {
                GroupKind::Horizontal => {
                    // Vertical line separators
                    (separator_pos - 0.5, 0.0, 1.0, bounds.size.height)
                }
                GroupKind::Vertical => {
                    // Horizontal line separators
                    (0.0, separator_pos - 0.5, bounds.size.width, 1.0)
                }
            };

            layer.setFrame(CGRect::new(
                CGPoint::new(sep_x, sep_y),
                CGSize::new(sep_width, sep_height),
            ));

            layer.setBackgroundColor(Some(&separator_color));

            // Ensure layer is added to parent
            if layer.superlayer().is_none() {
                parent_layer.addSublayer(layer);
            }
        }
    }

    fn update_selected_layer(
        &self,
        parent_layer: &CALayer,
        group_data: &GroupDisplayData,
        bounds: CGRect,
    ) {
        if group_data.total_count == 0 {
            return;
        }

        let selected_layer = {
            let mut state = self.ivars().borrow_mut();

            if let Some(existing) = &state.selected_layer {
                existing.clone()
            } else {
                let layer = CALayer::layer();
                parent_layer.addSublayer(&layer);
                state.selected_layer = Some(layer.clone());
                layer
            }
        };

        let mut segment_frame =
            Self::calculate_segment_frame(group_data, bounds, group_data.selected_index);
        segment_frame.origin.x += self.ivars().borrow().drag_offset(group_data.selected_index);

        selected_layer.setFrame(segment_frame);

        let selected_color = {
            let state = self.ivars().borrow();
            if group_data.is_selected {
                state.config.selected_color.to_nscolor()
            } else {
                state.config.inactive_selected_color.to_nscolor()
            }
        };
        selected_layer.setBackgroundColor(Some(&selected_color.CGColor()));
    }

    fn calculate_segment_frame(
        group_data: &GroupDisplayData,
        bounds: CGRect,
        segment_index: usize,
    ) -> CGRect {
        let segment_length = match group_data.group_kind {
            GroupKind::Horizontal => bounds.size.width / group_data.total_count as f64,
            GroupKind::Vertical => bounds.size.height / group_data.total_count as f64,
        };

        let (seg_x, seg_y, seg_width, seg_height) = match group_data.group_kind {
            GroupKind::Horizontal => {
                let seg_start = segment_index as f64 * segment_length;
                (seg_start, 0.0, segment_length, bounds.size.height)
            }
            GroupKind::Vertical => {
                let seg_start = segment_index as f64 * segment_length;
                (
                    0.0,
                    bounds.size.height - seg_start - segment_length,
                    bounds.size.width,
                    segment_length,
                )
            }
        };

        CGRect::new(CGPoint::new(seg_x, seg_y), CGSize::new(seg_width, seg_height))
    }

    fn animate_selection_change(&self, _from_index: usize, to_index: usize) {
        let (selected_layer, group_data, bounds) = {
            let state = self.ivars().borrow();
            let Some(selected_layer) = state.selected_layer.clone() else {
                return;
            };
            let Some(group_data) = state.group_data.clone() else {
                return;
            };
            let bounds = self.bounds();
            (selected_layer, group_data, bounds)
        };

        let mut to_frame = Self::calculate_segment_frame(&group_data, bounds, to_index);
        to_frame.origin.x += self.ivars().borrow().drag_offset(to_index);

        selected_layer.setFrame(to_frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_segment_click_detection() {
        let bounds = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(300.0, 100.0));
        let mtm =
            MainThreadMarker::new().unwrap_or_else(|| unsafe { MainThreadMarker::new_unchecked() });
        let config = crate::config::Config::default();
        let view = GroupIndicatorNSView::new(bounds, &config.settings.group_bars, mtm);

        let horizontal_group = GroupDisplayData {
            group_kind: GroupKind::Horizontal,
            total_count: 3,
            selected_index: 1,
            frame: bounds,
            is_selected: true,
            tabs: vec![],
        };

        // Test horizontal segments (each segment is 100px wide)
        // Segment 0: x 0-100, Segment 1: x 100-200, Segment 2: x 200-300
        assert_eq!(
            view.segment_at_point(CGPoint::new(50.0, 98.0), &horizontal_group),
            Some(0)
        );
        assert_eq!(
            view.segment_at_point(CGPoint::new(150.0, 98.0), &horizontal_group),
            Some(1)
        );
        assert_eq!(
            view.segment_at_point(CGPoint::new(250.0, 98.0), &horizontal_group),
            Some(2)
        );

        // Test outside view bounds
        assert_eq!(
            view.segment_at_point(CGPoint::new(50.0, -10.0), &horizontal_group),
            None
        );

        let vertical_group = GroupDisplayData {
            group_kind: GroupKind::Vertical,
            total_count: 4,
            selected_index: 2,
            frame: bounds,
            is_selected: true,
            tabs: vec![],
        };

        // Test vertical segments (each segment is 25px tall), which run from
        // the top of the bar down while y increases upward.
        // Segment 0: y 75-100, Segment 1: y 50-75, etc.
        assert_eq!(
            view.segment_at_point(CGPoint::new(298.0, 87.0), &vertical_group),
            Some(0)
        );
        assert_eq!(
            view.segment_at_point(CGPoint::new(298.0, 62.0), &vertical_group),
            Some(1)
        );
        assert_eq!(
            view.segment_at_point(CGPoint::new(298.0, 37.0), &vertical_group),
            Some(2)
        );
        assert_eq!(
            view.segment_at_point(CGPoint::new(298.0, 12.0), &vertical_group),
            Some(3)
        );
        assert_eq!(
            view.segment_at_point(CGPoint::new(298.0, 0.0), &vertical_group),
            Some(3)
        );

        // Test outside bar area
        // Test outside view bounds
        assert_eq!(
            view.segment_at_point(CGPoint::new(-10.0, 50.0), &vertical_group),
            None
        );
    }
}
