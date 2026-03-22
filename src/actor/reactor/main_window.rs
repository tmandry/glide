// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::Event;
use crate::actor::app::{Quiet, WindowId, pid_t};
use crate::collections::HashMap;

/// Keeps track of the main window.
#[derive(Default)]
pub(crate) struct MainWindowTracker {
    apps: HashMap<pid_t, AppState>,
    global_frontmost: Option<pid_t>,
}

struct AppState {
    is_frontmost: bool,
    frontmost_is_quiet: Quiet,
    main_window: Option<WindowId>,
}

impl MainWindowTracker {
    /// Returns Some(wid) if a WindowFocused layout event should be produced.
    #[must_use]
    pub fn handle_event(&mut self, event: &Event) -> Option<WindowId> {
        // There are two kinds of edges that can transition from one main window
        // state to another. One is an app main window change and the other
        // is a frontmost app change. Either can be labelled with quiet;
        // in the case of the frontmost app, the quiet label from the most
        // recent frontmost update of that app applies (even if the actual
        // event was a global frontmost change). If the main window changes on
        // a non-quiet edge we will produce a layout event.
        let (event_pid, quiet_edge) = match event {
            &Event::ApplicationLaunched {
                pid, is_frontmost, main_window, ..
            } => {
                self.apps.insert(
                    pid,
                    AppState {
                        is_frontmost,
                        frontmost_is_quiet: Quiet::No,
                        main_window,
                    },
                );
                (pid, Quiet::No)
            }
            &Event::ApplicationThreadTerminated(pid) => {
                self.apps.remove(&pid);
                return None;
            }
            &Event::ApplicationActivated(pid, quiet) => {
                let app = self.apps.get_mut(&pid)?;
                app.is_frontmost = true;
                app.frontmost_is_quiet = quiet;
                (pid, quiet)
            }
            &Event::ApplicationDeactivated(pid) => {
                let app = self.apps.get_mut(&pid)?;
                app.is_frontmost = false;
                return None;
            }
            &Event::ApplicationGloballyActivated(pid) => {
                // See the comment in main_window() for the difference between
                // this and the ApplicationActivated event.
                self.global_frontmost = Some(pid);
                let Some(app) = self.apps.get(&pid) else { return None };
                (pid, app.frontmost_is_quiet)
            }
            &Event::ApplicationGloballyDeactivated(pid) => {
                if self.global_frontmost == Some(pid) {
                    self.global_frontmost = None;
                }
                return None;
            }
            &Event::ApplicationMainWindowChanged(pid, wid, quiet) => {
                let app = self.apps.get_mut(&pid)?;
                app.main_window = wid;
                (pid, quiet)
            }
            Event::ApplicationTerminated(..)
            | Event::StartupComplete
            | Event::WindowsDiscovered { .. }
            | Event::WindowCreated(..)
            | Event::WindowBecameVisible(..)
            | Event::WindowDestroyed(..)
            | Event::WindowFrameChanged(..)
            | Event::ScreenParametersChanged { .. }
            | Event::SpaceChanged(..)
            | Event::MouseUp
            | Event::MouseMovedOverWindow(..)
            | Event::RaiseCompleted { .. }
            | Event::RaiseTimeout { .. }
            | Event::ScrollWheel { .. }
            | Event::LeftMouseDown(_)
            | Event::LeftMouseDragged(_)
            | Event::Command(..)
            | Event::ConfigChanged(_)
            | Event::WindowsOnScreenUpdated { .. } => return None,
        };
        if Some(event_pid) == self.global_frontmost && quiet_edge == Quiet::No {
            if let Some(wid) = self.main_window() {
                return Some(wid);
            }
        }
        None
    }

    /// The main window of the active app, if any.
    pub fn main_window(&self) -> Option<WindowId> {
        // Because apps self-report this event from their respective
        // threads, they can appear out of order. To mitigate this, we
        // require that the "global" view from NSNotificationCenter
        // agrees with the app about which is frontmost. This guarantees
        // eventual consistency.
        //
        // Since the global events provide an authoritative ordering, why
        // care about this event at all? The reason is that we want to
        // know what the main window of the app is upon activation. This
        // is important when the user clicks on a window of the app
        // that was not previously the main window: The frontmost app
        // and its main window can switch at the same time. In that case
        // we don't want to record the old main window as having focus,
        // since it never did. So we wait until both events are received.
        let Some(pid) = self.global_frontmost else {
            return None;
        };
        match self.apps.get(&pid) {
            Some(&AppState {
                is_frontmost: true,
                main_window: Some(window),
                ..
            }) => Some(window),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use objc2_core_foundation::CGRect;
    use test_log::test;

    use super::super::testing::{Apps, make_windows};
    use super::super::{Event, LayoutManager, Quiet, Reactor, SpaceId, WindowId};
    use crate::sys::screen::CoordinateConverter;

    #[test]
    fn it_tracks_frontmost_app_and_main_window_correctly() {
        use Event::*;
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new());
        let space = SpaceId::new(1);
        reactor.handle_event(ScreenParametersChanged {
            frames: vec![CGRect::ZERO],
            spaces: vec![Some(space)],

            converter: CoordinateConverter::default(),
            scale_factors: vec![2.0],
        });
        assert_eq!(None, reactor.main_window());

        reactor.handle_event(ApplicationGloballyActivated(1));
        reactor.handle_events(apps.make_app_with_opts(
            1,
            make_windows(2),
            Some(WindowId::new(1, 1)),
            true,
        ));
        reactor.handle_events(apps.make_app_with_opts(2, make_windows(2), None, false));
        assert_eq!(Some(WindowId::new(1, 1)), reactor.main_window());
        assert_eq!(reactor.layout.selected_window(space), Some(WindowId::new(1, 1)));

        reactor.handle_event(ApplicationGloballyDeactivated(1));
        assert_eq!(None, reactor.main_window());
        reactor.handle_event(ApplicationActivated(2, Quiet::No));
        reactor.handle_event(ApplicationGloballyActivated(2));
        assert_eq!(None, reactor.main_window());
        reactor.handle_event(ApplicationMainWindowChanged(
            2,
            Some(WindowId::new(2, 2)),
            Quiet::No,
        ));
        assert_eq!(Some(WindowId::new(2, 2)), reactor.main_window());
        assert_eq!(reactor.layout.selected_window(space), Some(WindowId::new(2, 2)));
        reactor.handle_event(ApplicationMainWindowChanged(
            1,
            Some(WindowId::new(1, 2)),
            Quiet::No,
        ));
        assert_eq!(Some(WindowId::new(2, 2)), reactor.main_window());
        reactor.handle_event(ApplicationDeactivated(1));
        assert_eq!(Some(WindowId::new(2, 2)), reactor.main_window());
        reactor.handle_event(ApplicationDeactivated(2));
        assert_eq!(None, reactor.main_window());

        reactor.handle_event(ApplicationGloballyActivated(3));
        assert_eq!(None, reactor.main_window());

        reactor.handle_events(apps.make_app_with_opts(
            3,
            make_windows(2),
            Some(WindowId::new(3, 1)),
            true,
        ));
        assert_eq!(Some(WindowId::new(3, 1)), reactor.main_window());
        assert_eq!(reactor.layout.selected_window(space), Some(WindowId::new(3, 1)));
    }

    #[test]
    fn it_does_not_update_layout_for_quiet_raises() {
        use Event::*;
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new());
        let space = SpaceId::new(1);
        reactor.handle_event(ScreenParametersChanged {
            frames: vec![CGRect::ZERO],
            spaces: vec![Some(space)],

            converter: CoordinateConverter::default(),
            scale_factors: vec![2.0],
        });

        reactor.handle_event(ApplicationGloballyActivated(1));
        reactor.handle_events(apps.make_app_with_opts(
            1,
            make_windows(2),
            Some(WindowId::new(1, 1)),
            true,
        ));
        reactor.handle_events(apps.make_app_with_opts(2, make_windows(2), None, false));
        assert_eq!(Some(WindowId::new(1, 1)), reactor.main_window());
        assert_eq!(reactor.layout.selected_window(space), Some(WindowId::new(1, 1)));

        reactor.handle_event(ApplicationGloballyDeactivated(1));
        assert_eq!(None, reactor.main_window());
        reactor.handle_event(ApplicationGloballyActivated(2));
        reactor.handle_event(ApplicationActivated(2, Quiet::Yes));
        assert_eq!(None, reactor.main_window());
        reactor.handle_event(ApplicationMainWindowChanged(
            2,
            Some(WindowId::new(2, 2)),
            Quiet::Yes,
        ));
        assert_eq!(Some(WindowId::new(2, 2)), reactor.main_window());
        assert_eq!(reactor.layout.selected_window(space), Some(WindowId::new(1, 1)));

        reactor.handle_event(ApplicationActivated(2, Quiet::No));
        assert_eq!(reactor.layout.selected_window(space), Some(WindowId::new(2, 2)));

        reactor.handle_event(ApplicationMainWindowChanged(
            2,
            Some(WindowId::new(2, 1)),
            Quiet::Yes,
        ));
        assert_eq!(Some(WindowId::new(2, 1)), reactor.main_window());
        assert_eq!(reactor.layout.selected_window(space), Some(WindowId::new(2, 2)));

        reactor.handle_event(ApplicationActivated(1, Quiet::Yes));
        reactor.handle_event(ApplicationGloballyActivated(1));
        assert_eq!(Some(WindowId::new(1, 1)), reactor.main_window());
        assert_eq!(reactor.layout.selected_window(space), Some(WindowId::new(2, 2)));

        reactor.handle_event(ApplicationMainWindowChanged(
            1,
            Some(WindowId::new(1, 2)),
            Quiet::No,
        ));
        assert_eq!(Some(WindowId::new(1, 2)), reactor.main_window());
        assert_eq!(reactor.layout.selected_window(space), Some(WindowId::new(1, 2)));
    }

    #[test]
    fn it_selects_main_window_when_space_is_enabled() {
        use Event::*;
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new());
        let pid = 3;
        let windows = make_windows(2);
        let space = SpaceId::new(1);
        reactor.handle_event(ScreenParametersChanged {
            frames: vec![CGRect::ZERO],
            spaces: vec![Some(space)],

            converter: CoordinateConverter::default(),
            scale_factors: vec![2.0],
        });

        reactor.handle_events(apps.make_app_with_opts(
            pid,
            windows,
            Some(WindowId::new(3, 1)),
            false,
        ));

        reactor.handle_event(SpaceChanged(vec![None]));
        reactor.handle_event(ApplicationActivated(3, Quiet::No));
        reactor.handle_event(ApplicationGloballyActivated(3));
        reactor.handle_event(WindowsDiscovered {
            pid,
            new: vec![],
            known_visible: vec![WindowId::new(3, 1), WindowId::new(3, 2)],
        });
        assert_eq!(Some(WindowId::new(3, 1)), reactor.main_window());

        reactor.handle_event(SpaceChanged(vec![Some(space)]));
        assert_eq!(reactor.layout.selected_window(space), Some(WindowId::new(3, 1)));
    }
}
