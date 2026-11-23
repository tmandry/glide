// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Menu bar icon for displaying the current space ID.

use std::cell::RefCell;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::Bool;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAlert, NSImage, NSImageSymbolConfiguration, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem,
    NSVariableStatusItemLength,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{NSObject, NSString, ns_string};
use tracing::{Span, debug, error, warn};

use crate::actor::updater;
use crate::actor::wm_controller::{self, WmCmd, WmCommand, WmEvent};
use crate::config;
use crate::ui::{alert, icon};

const SAVE_AND_QUIT_TAG: i64 = 1;
const TOGGLE_GLOBAL_TAG: i64 = 2;
const TOGGLE_SPACE_TAG: i64 = 3;
const SHOW_DOCS_TAG: i64 = 4;
const INSTALL_UPDATE_TAG: i64 = 5;
const SECURE_INPUT_TAG: i64 = 6;

pub struct StatusIcon {
    status_item: Retained<NSStatusItem>,
    mtm: MainThreadMarker,
    menu_handler: Retained<MenuHandler>,
    icon: Option<Retained<NSImage>>,
    /// The icon to show while Secure Input is blocking hotkeys.
    warning_icon: Option<Retained<NSImage>>,
    toggle_item: Retained<NSMenuItem>,
    space_toggle_item: Retained<NSMenuItem>,
    update_item: Retained<NSMenuItem>,
    secure_input_item: Retained<NSMenuItem>,
    secure_input_separator: Retained<NSMenuItem>,
}

impl StatusIcon {
    /// Creates a new menu bar manager.
    pub fn new(
        config: &config::StatusIconExperimental,
        mtm: MainThreadMarker,
        wm_tx: wm_controller::Sender,
        updater_tx: updater::Sender,
    ) -> Self {
        let status_bar = NSStatusBar::systemStatusBar();
        let status_item = status_bar.statusItemWithLength(NSVariableStatusItemLength);

        let icon = create_parachute_icon(config);
        let warning_icon = match &icon {
            Some(icon) => with_warning_badge(icon, config.color),
            None => warning_symbol(),
        };
        if let Some(button) = status_item.button(mtm)
            && let Some(icon) = &icon
        {
            button.setImage(Some(icon));
        }

        let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("Glide"));

        // This is needed to be able to manually set the menu item state
        menu.setAutoenablesItems(false);

        let menu_handler = MenuHandler::new(mtm, wm_tx, updater_tx);

        let secure_input_item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                ns_string!("Shortcuts Paused by Secure Input…"),
                Some(sel!(handleAction:)),
                ns_string!(""),
            )
        };
        unsafe { secure_input_item.setTarget(Some(&*menu_handler)) };
        secure_input_item.setTag(SECURE_INPUT_TAG as isize);
        secure_input_item.setImage(warning_symbol().as_deref());
        secure_input_item.setHidden(true);
        menu.addItem(&secure_input_item);
        let secure_input_separator = NSMenuItem::separatorItem(mtm);
        secure_input_separator.setHidden(true);
        menu.addItem(&secure_input_separator);

        let space_toggle_ns_title = ns_string!("Enable Space");
        let space_toggle_item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &space_toggle_ns_title,
                Some(sel!(handleAction:)),
                ns_string!(""),
            )
        };
        unsafe { space_toggle_item.setTarget(Some(&*menu_handler)) };
        space_toggle_item.setTag(TOGGLE_SPACE_TAG as isize);
        menu.addItem(&space_toggle_item);

        menu.addItem(&NSMenuItem::separatorItem(mtm));

        let version_item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(&format!("Glide v{}", env!("CARGO_PKG_VERSION"))),
                None,
                ns_string!(""),
            )
        };
        version_item.setEnabled(false);
        menu.addItem(&version_item);

        let update_item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                ns_string!(""),
                Some(sel!(handleAction:)),
                ns_string!(""),
            )
        };
        unsafe { update_item.setTarget(Some(&*menu_handler)) };
        update_item.setTag(INSTALL_UPDATE_TAG as isize);
        update_item.setHidden(true);
        menu.addItem(&update_item);

        let docs_item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                ns_string!("Documentation"),
                Some(sel!(handleAction:)),
                ns_string!(""),
            )
        };
        unsafe { docs_item.setTarget(Some(&*menu_handler)) };
        docs_item.setTag(SHOW_DOCS_TAG as isize);
        menu.addItem(&docs_item);

        menu.addItem(&NSMenuItem::separatorItem(mtm));

        let toggle_ns_title = ns_string!("Enable Glide");
        let toggle_item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &toggle_ns_title,
                Some(sel!(handleAction:)),
                ns_string!(""),
            )
        };
        unsafe { toggle_item.setTarget(Some(&*menu_handler)) };
        toggle_item.setTag(TOGGLE_GLOBAL_TAG as isize);
        menu.addItem(&toggle_item);

        let save_quit_item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                ns_string!("Quit"),
                Some(sel!(handleAction:)),
                ns_string!(""),
            )
        };
        unsafe { save_quit_item.setTarget(Some(&*menu_handler)) };
        save_quit_item.setTag(SAVE_AND_QUIT_TAG as isize);
        menu.addItem(&save_quit_item);

        status_item.setMenu(Some(&menu));

        Self {
            status_item,
            mtm,
            menu_handler,
            icon,
            warning_icon,
            toggle_item,
            space_toggle_item,
            update_item,
            secure_input_item,
            secure_input_separator,
        }
    }

    /// Sets the text next to the icon.
    pub fn set_text(&mut self, text: &str) {
        let ns_title = NSString::from_str(text);
        if let Some(button) = self.status_item.button(self.mtm) {
            button.setTitle(&ns_title);
        } else {
            warn!("Could not get button from status item");
        }
    }

    /// Sets the toggle menu item title.
    pub fn set_toggle_title(&mut self, title: &str) {
        let ns_title = NSString::from_str(title);
        self.toggle_item.setTitle(&ns_title);
    }

    /// Sets the space toggle menu item title.
    pub fn set_space_toggle_title(&mut self, title: &str) {
        let ns_title = NSString::from_str(title);
        self.space_toggle_item.setTitle(&ns_title);
    }

    /// Show the update menu item with `title`, or hide it if `None`.
    pub fn set_update_item(&mut self, title: Option<&str>, enabled: bool) {
        match title {
            Some(title) => {
                self.update_item.setTitle(&NSString::from_str(title));
                self.update_item.setEnabled(enabled);
                self.update_item.setHidden(false);
            }
            None => self.update_item.setHidden(true),
        }
    }

    /// Sets whether the space toggle menu item is enabled.
    pub fn set_space_toggle_enabled(&mut self, enabled: bool) {
        self.space_toggle_item.setEnabled(enabled);
    }

    /// Show that Secure Input is blocking hotkeys, with `details` explaining it
    /// in an alert the menu opens, or go back to normal if `None`.
    pub fn set_secure_input_warning(&mut self, details: Option<&str>) {
        let shown = details.is_some();
        *self.menu_handler.ivars().secure_input_details.borrow_mut() = details.map(str::to_owned);
        self.secure_input_item.setHidden(!shown);
        self.secure_input_separator.setHidden(!shown);
        let image = if shown {
            &self.warning_icon
        } else {
            &self.icon
        };
        if let Some(button) = self.status_item.button(self.mtm) {
            button.setImage(image.as_deref());
        }
    }
}

impl Drop for StatusIcon {
    fn drop(&mut self) {
        debug!("Removing menu bar icon");
        let status_bar = NSStatusBar::systemStatusBar();
        status_bar.removeStatusItem(&self.status_item);
    }
}

struct MenuHandlerIvars {
    wm_tx: wm_controller::Sender,
    updater_tx: updater::Sender,
    secure_input_details: RefCell<Option<String>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = MenuHandlerIvars]
    struct MenuHandler;

    impl MenuHandler {
        #[unsafe(method(handleAction:))]
        fn handle_action(&self, sender: &NSObject) {
            let tag = unsafe {
                let tag: i64 = msg_send![sender, tag];
                tag
            };
            debug!("Menu action received with tag: {}", tag);
            let wm_tx = &self.ivars().wm_tx;
            match tag {
                SAVE_AND_QUIT_TAG => {
                    debug!("Sending SaveAndExit command");
                    let _ = wm_tx.send((
                        Span::current(),
                        WmEvent::Command(WmCommand::Wm(WmCmd::SaveAndExit)),
                    ));
                }
                TOGGLE_GLOBAL_TAG => {
                    debug!("Sending ToggleGlobalEnabled command");
                    let _ = wm_tx.send((
                        Span::current(),
                        WmEvent::Command(WmCommand::Wm(WmCmd::ToggleGlobalEnabled)),
                    ));
                }
                TOGGLE_SPACE_TAG => {
                    debug!("Sending ToggleSpaceActivated command");
                    let _ = wm_tx.send((
                        Span::current(),
                        WmEvent::Command(WmCommand::Wm(WmCmd::ToggleSpaceActivated)),
                    ));
                }
                INSTALL_UPDATE_TAG => {
                    debug!("Requesting update install");
                    if self.ivars().updater_tx.send(updater::Request::Install).is_err() {
                        error!("Updater is not running");
                    }
                }
                SECURE_INPUT_TAG => {
                    let details = self.ivars().secure_input_details.borrow().clone();
                    let Some(details) = details else { return };
                    let alert = NSAlert::new(MainThreadMarker::from(self));
                    alert.setMessageText(ns_string!("Keyboard Shortcuts Are Paused"));
                    alert.setInformativeText(&NSString::from_str(&details));
                    alert::run_modal(&alert);
                }
                SHOW_DOCS_TAG => {
                    debug!("Opening docs in browser");
                    if let Err(e) = std::process::Command::new("/usr/bin/open")
                        .arg("https://glidewm.org/reference/config")
                        .spawn()
                    {
                        error!("Failed to open documentation: {e}");
                    }
                }
                _ => {
                    warn!("Unknown tag: {}", tag);
                }
            }
        }
    }
);

impl MenuHandler {
    /// Creates the parachute icon from the SVG file
    pub fn new(
        mtm: MainThreadMarker,
        wm_tx: wm_controller::Sender,
        updater_tx: updater::Sender,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm);
        let this = this.set_ivars(MenuHandlerIvars {
            wm_tx,
            updater_tx,
            secure_input_details: RefCell::new(None),
        });
        unsafe { msg_send![super(this), init] }
    }
}

fn warning_symbol() -> Option<Retained<NSImage>> {
    NSImage::imageWithSystemSymbolName_accessibilityDescription(
        ns_string!("exclamationmark.triangle"),
        Some(ns_string!("Keyboard shortcuts are paused")),
    )
}

/// Return `icon` with a warning triangle drawn to its right.
///
/// With `color`, draw the triangle in its multicolor form and leave `icon` as
/// it is. Otherwise, return a template image, so the menu bar tints both.
fn with_warning_badge(icon: &Retained<NSImage>, color: bool) -> Option<Retained<NSImage>> {
    const GAP: f64 = 2.0;

    let triangle = if color {
        NSImage::imageWithSystemSymbolName_accessibilityDescription(
            ns_string!("exclamationmark.triangle.fill"),
            None,
        )?
        .imageWithSymbolConfiguration(
            &NSImageSymbolConfiguration::configurationPreferringMulticolor(),
        )?
    } else {
        NSImage::imageWithSystemSymbolName_accessibilityDescription(
            ns_string!("exclamationmark.triangle"),
            None,
        )?
    };

    let icon_size = icon.size();
    let natural = triangle.size();
    let triangle_height = icon_size.height * 0.875;
    let triangle_size = CGSize {
        width: natural.width * triangle_height / natural.height,
        height: triangle_height,
    };
    let size = CGSize {
        width: icon_size.width + GAP + triangle_size.width,
        height: icon_size.height,
    };
    let triangle_origin = CGPoint {
        x: icon_size.width + GAP,
        y: (icon_size.height - triangle_height) / 2.0,
    };

    let icon = icon.clone();
    let draw = RcBlock::new(move |_rect: CGRect| -> Bool {
        icon.drawInRect(CGRect {
            origin: CGPoint::ZERO,
            size: icon_size,
        });
        triangle.drawInRect(CGRect {
            origin: triangle_origin,
            size: triangle_size,
        });
        Bool::YES
    });
    let image = NSImage::imageWithSize_flipped_drawingHandler(size, false, &draw);
    image.setTemplate(!color);
    image.setAccessibilityDescription(Some(ns_string!("Glide: keyboard shortcuts are paused")));
    Some(image)
}

fn create_parachute_icon(config: &config::StatusIconExperimental) -> Option<Retained<NSImage>> {
    let image = icon::parachute(config.color)?;
    // Size the image for the menu bar.
    image.setSize(CGSize { width: 16.0, height: 16.0 });
    Some(image)
}
