// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Visual example demonstrating the GroupIndicatorView with segmented bar rendering.
//!
//! This example creates a window with rendered group indicators showing
//! different scenarios using the new segmented bar approach with click handling.

use std::cell::RefCell;
use std::rc::Rc;

use glide_wm::config::GroupBarStyle;
use glide_wm::ui::group_bar::ColorExt;
use glide_wm::ui::{GroupDisplayData, GroupIndicatorNSView, GroupKind, TabLabel};
use objc2::MainThreadOnly;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSFont, NSRunningApplication,
    NSTextField, NSView, NSWindow, NSWindowStyleMask,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString};

struct IndicatorDemo {
    indicators: Vec<Rc<RefCell<GroupIndicatorNSView>>>,
}

impl IndicatorDemo {
    fn new() -> Self {
        Self { indicators: Vec::new() }
    }

    fn add_indicator(&mut self, indicator: Rc<RefCell<GroupIndicatorNSView>>) {
        self.indicators.push(indicator);
    }
}

fn main() {
    let mtm = MainThreadMarker::new().unwrap();

    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

    let _demo = create_demo_window(mtm);

    println!("Group Indicators Demo Running");
    println!("Visual features:");
    println!("- Segmented bars with visual separators");
    println!("- Blue highlighted segments for selected items");
    println!("- Different orientations (horizontal/vertical)");
    println!("- Various group sizes");

    app.run();
}

fn create_demo_window(mtm: MainThreadMarker) -> IndicatorDemo {
    let window_rect = NSRect::new(NSPoint::new(100.0, 100.0), NSSize::new(800.0, 1000.0));

    let window = unsafe {
        let window = NSWindow::alloc(mtm);
        NSWindow::initWithContentRect_styleMask_backing_defer(
            window,
            window_rect,
            NSWindowStyleMask(15), // Titled, closable, miniaturizable, resizable
            NSBackingStoreType::Buffered,
            false,
        )
    };

    window.setTitle(&NSString::from_str("Group Indicators Visual Demo"));
    window.makeKeyAndOrderFront(None);

    let (content_view, demo) = create_content_view(mtm);
    window.setContentView(Some(&content_view));

    demo
}

fn create_content_view(mtm: MainThreadMarker) -> (Retained<NSView>, IndicatorDemo) {
    let content_view = NSView::initWithFrame(
        NSView::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(800.0, 1000.0)),
    );

    let scenarios = create_demo_scenarios();
    let mut y_position = 950.0;
    let thickness = 4.0;
    let mut demo = IndicatorDemo::new();

    for (title, group_data) in scenarios {
        // Add title label
        let label = create_label(&title, y_position, mtm);
        content_view.addSubview(&label);
        y_position -= 35.0;

        // Add indicator view with proper dimensions based on orientation
        let (indicator_rect, indicator_frame) = match group_data.group_kind {
            GroupKind::Horizontal => {
                // Horizontal bars: 700px wide x 4px tall
                let rect = NSRect::new(
                    NSPoint::new(50.0, y_position - thickness),
                    NSSize::new(700.0, thickness),
                );
                let frame = CGRect::new(
                    CGPoint::new(rect.origin.x, rect.origin.y),
                    CGSize::new(rect.size.width, rect.size.height),
                );
                (rect, frame)
            }
            GroupKind::Vertical => {
                // Vertical bars: 4px wide x 120px tall
                let rect = NSRect::new(
                    NSPoint::new(50.0, y_position - 120.0),
                    NSSize::new(thickness, 120.0),
                );
                let frame = CGRect::new(
                    CGPoint::new(rect.origin.x, rect.origin.y),
                    CGSize::new(rect.size.width, rect.size.height),
                );
                (rect, frame)
            }
        };

        let mut group_data_with_frame = group_data.clone();
        group_data_with_frame.frame = indicator_frame;

        let config = glide_wm::config::Config::default();
        let mut indicator_view =
            GroupIndicatorNSView::new(indicator_rect, &config.settings.group_bars, mtm);
        indicator_view.update(group_data_with_frame);

        let indicator_rc = Rc::new(RefCell::new(indicator_view));

        // Set up click callback that updates the selection
        let indicator_rc_clone = indicator_rc.clone();
        indicator_rc.borrow_mut().set_click_callback(Rc::new(move |segment_index| {
            println!("✨ Segment {} clicked! Updating selection...", segment_index);
            let mut indicator = indicator_rc_clone.borrow_mut();
            indicator.click_segment(segment_index);
        }));

        content_view.addSubview(indicator_rc.borrow().view());

        demo.add_indicator(indicator_rc);

        // Adjust spacing based on orientation
        match group_data.group_kind {
            GroupKind::Horizontal => y_position -= 40.0,
            GroupKind::Vertical => y_position -= 140.0, // More space for taller vertical bars
        }
    }

    // A labeled glass bar, as configured with `style = "glass"`, `show_titles`
    // and `show_icons`.
    let label = create_label("Glass tab bar with titles and icons", y_position, mtm);
    content_view.addSubview(&label);
    y_position -= 35.0;
    let thickness = 24.0;
    let indicator_rect = NSRect::new(
        NSPoint::new(50.0, y_position - thickness),
        NSSize::new(700.0, thickness),
    );
    let mut config = glide_wm::config::Config::default().settings.group_bars;
    config.style = GroupBarStyle::Glass;
    config.show_titles = true;
    config.show_icons = true;
    // Translucent colors let the glass show, and light text reads on the
    // dark demo window.
    let color = |c: &str| c.parse().unwrap();
    config.background_color = color("#FFFFFF33");
    config.selected_color = color("#0080FF99");
    config.text_color = color("#FFFFFF");
    config.inactive_text_color = color("#FFFFFFB3");
    config.close_color = color("#FFFFFFCC");
    config.close_background_color = color("#FFFFFF26");
    // Colored stripes behind the bar give the glass something to show.
    for (i, hex) in ["#FF5F57", "#FEBC2E", "#28C840", "#0A84FF", "#BF5AF2"].iter().enumerate() {
        let stripe = NSView::initWithFrame(
            NSView::alloc(mtm),
            NSRect::new(
                NSPoint::new(50.0 + i as f64 * 140.0, indicator_rect.origin.y - 8.0),
                NSSize::new(70.0, thickness + 16.0),
            ),
        );
        stripe.setWantsLayer(true);
        let hex: glide_wm::config::Color = color(hex);
        if let Some(layer) = stripe.layer() {
            layer.setBackgroundColor(Some(&hex.to_nscolor().CGColor()));
        }
        content_view.addSubview(&stripe);
    }
    let icon = NSRunningApplication::currentApplication().icon();
    let tabs = [
        "README.md — glide",
        "Liquid Glass - Apple Developer",
        "Terminal",
    ]
    .map(|title| TabLabel {
        title: title.to_string().into(),
        icon: icon.clone(),
    })
    .to_vec();
    let mut indicator_view = GroupIndicatorNSView::new(indicator_rect, &config, mtm);
    indicator_view.update(GroupDisplayData {
        group_kind: GroupKind::Horizontal,
        total_count: tabs.len(),
        selected_index: 0,
        frame: indicator_rect,
        is_selected: true,
        tabs,
    });
    let indicator_rc = Rc::new(RefCell::new(indicator_view));
    let indicator_rc_clone = indicator_rc.clone();
    indicator_rc.borrow_mut().set_click_callback(Rc::new(move |segment_index| {
        indicator_rc_clone.borrow_mut().click_segment(segment_index);
    }));
    indicator_rc.borrow_mut().set_close_callback(Rc::new(move |segment_index| {
        println!("Close button of tab {segment_index} clicked");
    }));
    let indicator_rc_clone = indicator_rc.clone();
    indicator_rc.borrow_mut().set_move_callback(Rc::new(move |from, to| {
        let mut indicator = indicator_rc_clone.borrow_mut();
        let Some(mut group_data) = indicator.group_data() else {
            return;
        };
        let tab = group_data.tabs.remove(from);
        group_data.tabs.insert(to, tab);
        group_data.selected_index = to;
        indicator.update(group_data);
    }));
    content_view.addSubview(indicator_rc.borrow().view());
    demo.add_indicator(indicator_rc);
    y_position -= 60.0;

    // Add instructions at the bottom
    let instructions = create_label(
        "Interactive Demo: Click on segments to change selection and see animation!",
        y_position - 20.0,
        mtm,
    );
    content_view.addSubview(&instructions);

    (content_view, demo)
}

fn create_label(text: &str, y_position: f64, mtm: MainThreadMarker) -> Retained<NSTextField> {
    let label_rect = NSRect::new(NSPoint::new(50.0, y_position), NSSize::new(700.0, 25.0));

    let label = NSTextField::initWithFrame(NSTextField::alloc(mtm), label_rect);

    label.setStringValue(&NSString::from_str(text));
    label.setEditable(false);
    label.setSelectable(false);
    label.setBezeled(false);
    label.setDrawsBackground(false);
    label.setFont(Some(&NSFont::systemFontOfSize(14.0)));

    label
}

fn create_demo_scenarios() -> Vec<(&'static str, GroupDisplayData)> {
    // Note: frame will be set later when we know the actual indicator rect
    let frame = CGRect::ZERO;

    vec![
        (
            "Small horizontal group (3 tabs, middle selected) - Click any segment!",
            GroupDisplayData {
                group_kind: GroupKind::Horizontal,
                total_count: 3,
                selected_index: 1,
                frame,
                is_selected: true,
                tabs: vec![],
            },
        ),
        (
            "Small vertical group (4 windows, first selected) - Click any segment!",
            GroupDisplayData {
                group_kind: GroupKind::Vertical,
                total_count: 4,
                selected_index: 0,
                frame,
                is_selected: true,
                tabs: vec![],
            },
        ),
        (
            "Medium horizontal group (8 tabs, segment 4 selected) - Click any segment!",
            GroupDisplayData {
                group_kind: GroupKind::Horizontal,
                total_count: 8,
                selected_index: 4,
                frame,
                is_selected: true,
                tabs: vec![],
            },
        ),
        (
            "Large horizontal group (15 tabs, segment 7 selected) - Click any segment!",
            GroupDisplayData {
                group_kind: GroupKind::Horizontal,
                total_count: 15,
                selected_index: 7,
                frame,
                is_selected: true,
                tabs: vec![],
            },
        ),
        (
            "Large vertical group (12 windows, segment 3 selected) - Click any segment!",
            GroupDisplayData {
                group_kind: GroupKind::Vertical,
                total_count: 12,
                selected_index: 3,
                frame,
                is_selected: true,
                tabs: vec![],
            },
        ),
        (
            "Single item group (no separators) - Nothing to click!",
            GroupDisplayData {
                group_kind: GroupKind::Horizontal,
                total_count: 1,
                selected_index: 0,
                frame,
                is_selected: true,
                tabs: vec![],
            },
        ),
        (
            "Two item vertical group (one separator) - Click either segment!",
            GroupDisplayData {
                group_kind: GroupKind::Vertical,
                total_count: 2,
                selected_index: 1,
                frame,
                is_selected: true,
                tabs: vec![],
            },
        ),
    ]
}
