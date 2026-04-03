// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! This actor manages the global notification queue, which tells us when an
//! application is launched or focused or the screen state changes.

use std::{future, mem};

use objc2::rc::{Allocated, Retained};
use objc2::{AnyThread, ClassType, DeclaredClass, Encode, Encoding, define_class, msg_send, sel};
use objc2_app_kit::{
    self, NSApplication, NSRunningApplication, NSWorkspace, NSWorkspaceApplicationKey,
};
use objc2_foundation::{MainThreadMarker, NSNotification, NSNotificationCenter, NSObject};
use tracing::{Span, info_span, trace, warn};

use super::window_server;
use super::wm_controller::{self, WmEvent};
use crate::actor::app::AppInfo;
use crate::sys::app::NSRunningApplicationExt;
use crate::sys::screen;

#[repr(C)]
struct Instance {
    wm_tx: wm_controller::Sender,
    ws_tx: window_server::Sender,
}

unsafe impl Encode for Instance {
    const ENCODING: Encoding = Encoding::Object;
}

define_class! {
    // SAFETY:
    // - The superclass NSObject does not have any subclassing requirements.
    // - `NotificationHandler` does not implement `Drop`.
    #[unsafe(super(NSObject))]
    #[ivars = Box<Instance>]
    struct NotificationCenterInner;

    // SAFETY: Each of these method signatures must match their invocations.
    impl NotificationCenterInner {
        #[unsafe(method_id(initWith:))]
        fn init(this: Allocated<Self>, instance: Instance) -> Option<Retained<Self>> {
            let this = this.set_ivars(Box::new(instance));
            unsafe { msg_send![super(this), init] }
        }

        #[unsafe(method(recvScreenChangedEvent:))]
        fn recv_screen_changed_event(&self, notif: &NSNotification) {
            trace!("{notif:#?}");
            self.handle_screen_changed_event(notif);
        }

        #[unsafe(method(recvAppEvent:))]
        fn recv_app_event(&self, notif: &NSNotification) {
            trace!("{notif:#?}");
            self.handle_app_event(notif);
        }
    }
}

impl NotificationCenterInner {
    fn new(wm_tx: wm_controller::Sender, ws_tx: window_server::Sender) -> Retained<Self> {
        let instance = Instance { wm_tx, ws_tx };
        unsafe { msg_send![Self::alloc(), initWith: instance] }
    }

    fn handle_screen_changed_event(&self, notif: &NSNotification) {
        use objc2_app_kit::*;
        let name = &*notif.name();
        let span = info_span!("notification_center::handle_screen_changed_event", ?name);
        let _s = span.enter();
        if unsafe { NSWorkspaceActiveSpaceDidChangeNotification } == name {
            self.send_space_changed();
        } else if unsafe { NSApplicationDidChangeScreenParametersNotification } == name {
            self.send_screen_parameters();
        } else {
            panic!("Unexpected screen changed event: {notif:?}");
        }
    }

    fn send_screen_parameters(&self) {
        let ns_screens = screen::get_ns_screens(MainThreadMarker::new().unwrap());
        self.send_ws_request(window_server::Event::ScreenParametersChanged(ns_screens));
    }

    fn send_space_changed(&self) {
        self.send_ws_request(window_server::Event::SpaceChanged);
    }

    fn handle_app_event(&self, notif: &NSNotification) {
        use objc2_app_kit::*;
        let Some(app) = self.running_application(notif) else {
            return;
        };
        let pid = app.pid();
        let name = &*notif.name();
        let span = info_span!("notification_center::handle_app_event", ?name);
        let _guard = span.enter();
        if unsafe { NSWorkspaceDidLaunchApplicationNotification } == name {
            self.send_wm_event(WmEvent::AppLaunch(pid, AppInfo::from(&*app)));
        } else if unsafe { NSWorkspaceDidActivateApplicationNotification } == name {
            self.send_wm_event(WmEvent::AppGloballyActivated(pid));
        } else if unsafe { NSWorkspaceDidDeactivateApplicationNotification } == name {
            self.send_wm_event(WmEvent::AppGloballyDeactivated(pid));
        } else if unsafe { NSWorkspaceDidTerminateApplicationNotification } == name {
            self.send_wm_event(WmEvent::AppTerminated(pid));
        } else if unsafe { NSWorkspaceActiveSpaceDidChangeNotification } == name {
            self.send_space_changed();
        } else {
            panic!("Unexpected application event: {notif:?}");
        }
    }

    fn send_wm_event(&self, event: WmEvent) {
        // Errors only happen during shutdown, so we can ignore them.
        _ = self.ivars().wm_tx.send((Span::current().clone(), event));
    }

    fn send_ws_request(&self, request: window_server::Event) {
        self.ivars().ws_tx.send(request);
    }

    fn running_application(
        &self,
        notif: &NSNotification,
    ) -> Option<Retained<NSRunningApplication>> {
        let info = notif.userInfo();
        let Some(info) = info else {
            warn!("Got app notification without user info: {notif:?}");
            return None;
        };
        let app = unsafe { info.valueForKey(NSWorkspaceApplicationKey) };
        let Some(app) = app else {
            warn!("Got app notification without app object: {notif:?}");
            return None;
        };
        assert!(app.class() == NSRunningApplication::class());
        let app: Retained<NSRunningApplication> = unsafe { mem::transmute(app) };
        Some(app)
    }
}

pub struct NotificationCenter {
    #[allow(dead_code)]
    inner: Retained<NotificationCenterInner>,
}

impl NotificationCenter {
    pub fn new(wm_tx: wm_controller::Sender, ws_tx: window_server::Sender) -> Self {
        let handler = NotificationCenterInner::new(wm_tx, ws_tx);

        // SAFETY: Selector must have signature fn(&self, &NSNotification)
        let register_unsafe =
            |selector, notif_name, center: &Retained<NSNotificationCenter>, object| unsafe {
                center.addObserver_selector_name_object(
                    &handler,
                    selector,
                    Some(notif_name),
                    Some(object),
                );
            };

        let workspace = &NSWorkspace::sharedWorkspace();
        let workspace_center = &workspace.notificationCenter();
        let default_center = &NSNotificationCenter::defaultCenter();
        let shared_app = &NSApplication::sharedApplication(MainThreadMarker::new().unwrap());
        unsafe {
            use objc2_app_kit::*;
            register_unsafe(
                sel!(recvScreenChangedEvent:),
                NSApplicationDidChangeScreenParametersNotification,
                default_center,
                shared_app,
            );
            register_unsafe(
                sel!(recvScreenChangedEvent:),
                NSWorkspaceActiveSpaceDidChangeNotification,
                workspace_center,
                workspace,
            );
            register_unsafe(
                sel!(recvAppEvent:),
                NSWorkspaceDidLaunchApplicationNotification,
                workspace_center,
                workspace,
            );
            register_unsafe(
                sel!(recvAppEvent:),
                NSWorkspaceDidActivateApplicationNotification,
                workspace_center,
                workspace,
            );
            register_unsafe(
                sel!(recvAppEvent:),
                NSWorkspaceDidDeactivateApplicationNotification,
                workspace_center,
                workspace,
            );
            register_unsafe(
                sel!(recvAppEvent:),
                NSWorkspaceDidTerminateApplicationNotification,
                workspace_center,
                workspace,
            );
        };

        NotificationCenter { inner: handler }
    }

    pub async fn watch_for_notifications(self) {
        let workspace = &NSWorkspace::sharedWorkspace();

        self.inner.send_screen_parameters();
        self.inner.send_wm_event(WmEvent::AppEventsRegistered);
        if let Some(app) = workspace.frontmostApplication() {
            self.inner.send_wm_event(WmEvent::AppGloballyActivated(app.pid()));
        }

        // All the work is done in callbacks dispatched by the run loop, which
        // we assume is running once this function is awaited.
        future::pending().await
    }
}
