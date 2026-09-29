// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Visual example demonstrating the GroupIndicatorView with segmented bar rendering.
//!
//! This example creates a window with rendered group indicators showing
//! different scenarios using the new segmented bar approach with click handling.

use std::cell::RefCell;
use std::rc::Rc;

use glide_wm::ui::{GroupDisplayData, GroupIndicatorNSView, GroupKind};
use objc2::MainThreadOnly;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSFont, NSTextField, NSView,
    NSWindow, NSWindowStyleMask,
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
            },
        ),
    ]
}
