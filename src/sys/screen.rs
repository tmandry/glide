// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::f64;
use std::ffi::c_int;
use std::mem::MaybeUninit;
use std::num::NonZeroU64;

use bitflags::bitflags;
use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::TCFType;
use core_foundation::string::{CFString, CFStringRef};
use objc2::rc::Retained;
use objc2::{ClassType, msg_send};
use objc2_app_kit::NSScreen;
use objc2_core_foundation::{CGPoint, CGRect};
use objc2_core_graphics::{CGDisplayBounds, CGError, CGGetActiveDisplayList};
use objc2_foundation::{MainThreadMarker, NSArray, NSDictionary, NSNumber, ns_string};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[repr(transparent)]
pub struct SpaceId(NonZeroU64);

#[cfg(test)]
impl SpaceId {
    pub fn new(id: u64) -> SpaceId {
        SpaceId(NonZeroU64::new(id).unwrap())
    }
}

/// Calculates the screen and space configuration.
pub struct ScreenCache<S: System = Actual> {
    system: S,
    uuids: Vec<CFString>,
}

impl ScreenCache<Actual> {
    pub fn new() -> Self {
        Self::new_with(Actual)
    }
}

impl<S: System> ScreenCache<S> {
    fn new_with(system: S) -> ScreenCache<S> {
        ScreenCache { uuids: vec![], system }
    }

    /// Returns a list containing the usable frame for each screen.
    ///
    /// This method must be called when there is an update to the screen
    /// configuration. It updates the internal cache so that calls to
    /// screen_spaces are fast.
    ///
    /// The main screen (if any) is always first. Note that there may be no
    /// screens.
    #[forbid(unsafe_code)] // called from test
    pub fn update_screen_config(
        &mut self,
        ns_screens: Vec<NSScreenInfo>,
    ) -> Option<(Vec<ScreenInfo>, CoordinateConverter)> {
        debug!("ns_screens={ns_screens:?}");

        let mut cg_screens = self.system.cg_screens().unwrap();
        debug!("cg_screens={cg_screens:?}");

        if ns_screens.len() != cg_screens.len() {
            // This occasionally happens when unlocking (#16). The events where
            // this happens have been extraneous, so ignoring them is fine.
            warn!(
                "Ignoring screen config change: There are {} ns_screens but {} cg_screens",
                ns_screens.len(),
                cg_screens.len(),
            );
            return None;
        }

        if cg_screens.is_empty() {
            return Some((vec![], CoordinateConverter::default()));
        };

        // Ensure that the main screen is always first.
        if let Some(main_screen_idx) =
            cg_screens.iter().position(|s| s.bounds.origin == CGPoint::ZERO)
        {
            cg_screens.swap(0, main_screen_idx);
        } else {
            warn!("Could not find main screen. cg_screens={cg_screens:?}");
        }

        self.uuids = cg_screens
            .iter()
            .map(|screen| self.system.uuid_for_rect(screen.bounds))
            .collect();

        // We want to get the visible_frame of the NSScreenInfo, but in CG's
        // top-left coordinates from NSScreen's bottom-left.
        // The main screen has origin (0, 0) in both coordinate systems.
        let converter = CoordinateConverter {
            screen_height: cg_screens[0].bounds.max().y,
        };

        let screens: Vec<ScreenInfo> = cg_screens
            .iter()
            .flat_map(|&CGScreenInfo { cg_id, .. }| {
                let Some(ns_screen) = ns_screens.iter().find(|s| s.cg_id == cg_id) else {
                    warn!("Can't find NSScreen corresponding to {cg_id:?}");
                    return None;
                };
                let converted = converter.convert_rect(ns_screen.visible_frame).unwrap();
                Some(ScreenInfo {
                    visible_frame: converted,
                    id: cg_id,
                    scale_factor: ns_screen.backing_scale_factor,
                })
            })
            .collect();
        Some((screens, converter))
    }

    /// Returns a list of the active spaces on each screen. The order
    /// corresponds to the screens returned by `screen_frames`.
    pub fn get_screen_spaces(&self) -> Vec<Option<SpaceId>> {
        self.uuids
            .iter()
            .map(|screen| unsafe {
                CGSManagedDisplayGetCurrentSpace(
                    CGSMainConnectionID(),
                    screen.as_concrete_TypeRef(),
                )
            })
            .map(|id| Some(SpaceId(NonZeroU64::new(id)?)))
            .collect()
    }
}

/// Converts between Quartz and Cocoa coordinate systems.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct CoordinateConverter {
    /// The y offset of the Cocoa origin in the Quartz coordinate system, and
    /// vice versa. This is the height of the first screen. The origins
    /// are the bottom left and top left of the screen, respectively.
    screen_height: f64,
}

/// Creates a `CoordinateConverter` that returns None for any conversion.
impl Default for CoordinateConverter {
    fn default() -> Self {
        Self { screen_height: f64::NAN }
    }
}

impl CoordinateConverter {
    pub fn convert_point(&self, point: CGPoint) -> Option<CGPoint> {
        if self.screen_height.is_nan() {
            return None;
        }
        Some(CGPoint::new(point.x, self.screen_height - point.y))
    }

    pub fn convert_rect(&self, rect: CGRect) -> Option<CGRect> {
        if self.screen_height.is_nan() {
            return None;
        }
        Some(CGRect::new(
            CGPoint::new(rect.origin.x, self.screen_height - rect.max().y),
            rect.size,
        ))
    }
}

#[allow(private_interfaces)]
pub trait System {
    fn cg_screens(&self) -> Result<Vec<CGScreenInfo>, CGError>;
    fn uuid_for_rect(&self, rect: CGRect) -> CFString;
}

#[derive(Debug, Clone)]
struct CGScreenInfo {
    cg_id: ScreenId,
    bounds: CGRect,
}

#[derive(Debug, Clone)]
pub struct NSScreenInfo {
    pub frame: CGRect,
    pub visible_frame: CGRect,
    pub cg_id: ScreenId,
    pub backing_scale_factor: f64,
}

/// Gathers NSScreen information. Must be called on the main thread.
pub fn get_ns_screens(mtm: MainThreadMarker) -> Vec<NSScreenInfo> {
    NSScreen::screens(mtm)
        .iter()
        .flat_map(|s| {
            Some(NSScreenInfo {
                frame: s.frame(),
                visible_frame: s.visibleFrame(),
                cg_id: s.get_number().ok()?,
                backing_scale_factor: s.backingScaleFactor(),
            })
        })
        .collect()
}

pub struct Actual;
#[allow(private_interfaces)]
impl System for Actual {
    fn cg_screens(&self) -> Result<Vec<CGScreenInfo>, CGError> {
        const MAX_SCREENS: usize = 64;
        let mut ids: MaybeUninit<[CGDirectDisplayID; MAX_SCREENS]> = MaybeUninit::uninit();
        let mut count: u32 = 0;
        let ids = unsafe {
            let err = CGGetActiveDisplayList(
                MAX_SCREENS as u32,
                ids.as_mut_ptr() as *mut CGDirectDisplayID,
                &mut count,
            );
            if err != CGError::Success {
                return Err(err);
            }
            std::slice::from_raw_parts(ids.as_ptr() as *const u32, count as usize)
        };
        Ok(ids
            .iter()
            .map(|&cg_id| CGScreenInfo {
                cg_id: ScreenId(cg_id),
                bounds: CGDisplayBounds(cg_id),
            })
            .collect())
    }

    fn uuid_for_rect(&self, rect: CGRect) -> CFString {
        unsafe {
            CFString::wrap_under_create_rule(CGSCopyBestManagedDisplayForRect(
                CGSMainConnectionID(),
                rect,
            ))
        }
    }
}

type CGDirectDisplayID = u32;

#[derive(Debug, Clone)]
pub struct ScreenInfo {
    pub visible_frame: CGRect,
    pub id: ScreenId,
    pub scale_factor: f64,
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Clone, Copy)]
pub struct ScreenId(CGDirectDisplayID);

#[cfg(test)]
impl ScreenId {
    pub fn new(id: u32) -> ScreenId {
        ScreenId(id)
    }
}

pub trait NSScreenExt {
    fn get_number(&self) -> Result<ScreenId, ()>;
}
impl NSScreenExt for NSScreen {
    fn get_number(&self) -> Result<ScreenId, ()> {
        let desc = self.deviceDescription();
        match desc.objectForKey(ns_string!("NSScreenNumber")) {
            Some(val) if unsafe { msg_send![&*val, isKindOfClass:NSNumber::class() ] } => {
                let number: &NSNumber = unsafe { std::mem::transmute(val) };
                Ok(ScreenId(number.as_u32()))
            }
            val => {
                warn!(
                    "Could not get NSScreenNumber for screen with name {:?}: {:?}",
                    self.localizedName(),
                    val,
                );
                Err(())
            }
        }
    }
}

/// Returns the user-facing number of the currently active space.
///
/// Note: This relies on private APIs and might break.
pub fn get_active_space_number() -> Option<usize> {
    let cid = unsafe { CGSMainConnectionID() };
    let space_info = unsafe { Retained::from_raw(CGSCopyManagedDisplaySpaces(cid))? };
    let active_id = unsafe { CGSGetActiveSpace(cid) };
    let mut count = 0;
    for screen in space_info {
        let screen: Retained<NSDictionary> = screen.downcast().ok()?;
        let spaces: Retained<NSArray> =
            screen.valueForKey(ns_string!("Spaces"))?.downcast().ok()?;
        for space in spaces {
            count += 1;
            let Some(id) = (|| {
                let space: Retained<NSDictionary> = space.downcast().ok()?;
                let id: Retained<NSNumber> =
                    space.valueForKey(ns_string!("ManagedSpaceID"))?.downcast().ok()?;
                Some(id)
            })() else {
                continue;
            };
            if id.as_u64() == active_id {
                return Some(count);
            }
        }
    }
    None
}

/// Utilities for querying the current system configuration. For diagnostic purposes only.
#[allow(dead_code)]
pub mod diagnostic {
    use super::*;

    pub fn cur_space() -> SpaceId {
        SpaceId(NonZeroU64::new(unsafe { CGSGetActiveSpace(CGSMainConnectionID()) }).unwrap())
    }

    pub fn visible_spaces() -> CFArray<SpaceId> {
        unsafe {
            let arr = CGSCopySpaces(CGSMainConnectionID(), CGSSpaceMask::ALL_VISIBLE_SPACES);
            CFArray::wrap_under_create_rule(arr)
        }
    }

    pub fn all_spaces() -> CFArray<SpaceId> {
        unsafe {
            let arr = CGSCopySpaces(CGSMainConnectionID(), CGSSpaceMask::ALL_SPACES);
            CFArray::wrap_under_create_rule(arr)
        }
    }

    pub fn managed_displays() -> CFArray {
        unsafe { CFArray::wrap_under_create_rule(CGSCopyManagedDisplays(CGSMainConnectionID())) }
    }

    pub fn managed_display_spaces() -> Retained<NSArray> {
        unsafe { Retained::from_raw(CGSCopyManagedDisplaySpaces(CGSMainConnectionID())) }.unwrap()
    }
}

// Based on https://github.com/asmagill/hs._asm.undocumented.spaces/blob/master/CGSSpace.h.
// Also see https://github.com/koekeishiya/yabai/blob/d55a647913ab72d8d8b348bee2d3e59e52ce4a5d/src/misc/extern.h.

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGSMainConnectionID() -> c_int;
    fn CGSGetActiveSpace(cid: c_int) -> u64;
    fn CGSCopySpaces(cid: c_int, mask: CGSSpaceMask) -> CFArrayRef;
    fn CGSCopyManagedDisplays(cid: c_int) -> CFArrayRef;
    fn CGSCopyManagedDisplaySpaces(cid: c_int) -> *mut NSArray;
    fn CGSManagedDisplayGetCurrentSpace(cid: c_int, uuid: CFStringRef) -> u64;
    fn CGSCopyBestManagedDisplayForRect(cid: c_int, rect: CGRect) -> CFStringRef;
}

bitflags! {
    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
    #[repr(transparent)]
    struct CGSSpaceMask: c_int {
        const INCLUDE_CURRENT = 1 << 0;
        const INCLUDE_OTHERS  = 1 << 1;

        const INCLUDE_USER    = 1 << 2;
        const INCLUDE_OS      = 1 << 3;

        const VISIBLE         = 1 << 16;

        const CURRENT_SPACES = Self::INCLUDE_USER.bits() | Self::INCLUDE_CURRENT.bits();
        const OTHER_SPACES = Self::INCLUDE_USER.bits() | Self::INCLUDE_OTHERS.bits();
        const ALL_SPACES =
            Self::INCLUDE_USER.bits() | Self::INCLUDE_OTHERS.bits() | Self::INCLUDE_CURRENT.bits();

        const ALL_VISIBLE_SPACES = Self::ALL_SPACES.bits() | Self::VISIBLE.bits();

        const CURRENT_OS_SPACES = Self::INCLUDE_OS.bits() | Self::INCLUDE_CURRENT.bits();
        const OTHER_OS_SPACES = Self::INCLUDE_OS.bits() | Self::INCLUDE_OTHERS.bits();
        const ALL_OS_SPACES =
            Self::INCLUDE_OS.bits() | Self::INCLUDE_OTHERS.bits() | Self::INCLUDE_CURRENT.bits();
    }
}

#[cfg(test)]
mod test {
    use core_foundation::string::CFString;
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};

    use super::{CGError, CGScreenInfo, NSScreenInfo, ScreenCache, ScreenId, System};

    struct Stub {
        cg_screens: Vec<CGScreenInfo>,
    }
    impl System for Stub {
        fn cg_screens(&self) -> Result<Vec<CGScreenInfo>, CGError> {
            Ok(self.cg_screens.clone())
        }
        fn uuid_for_rect(&self, _rect: CGRect) -> CFString {
            CFString::new("stub")
        }
    }

    #[test]
    fn it_calculates_the_visible_frame() {
        let stub = Stub {
            cg_screens: vec![
                CGScreenInfo {
                    cg_id: ScreenId(1),
                    bounds: CGRect::new(CGPoint::new(3840.0, 1080.0), CGSize::new(1512.0, 982.0)),
                },
                CGScreenInfo {
                    cg_id: ScreenId(3),
                    bounds: CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(3840.0, 2160.0)),
                },
            ],
        };
        let ns_screens = vec![
            NSScreenInfo {
                cg_id: ScreenId(3),
                frame: CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(3840.0, 2160.0)),
                visible_frame: CGRect::new(CGPoint::new(0.0, 76.0), CGSize::new(3840.0, 2059.0)),
                backing_scale_factor: 2.0,
            },
            NSScreenInfo {
                cg_id: ScreenId(1),
                frame: CGRect::new(CGPoint::new(3840.0, 98.0), CGSize::new(1512.0, 982.0)),
                visible_frame: CGRect::new(CGPoint::new(3840.0, 98.0), CGSize::new(1512.0, 950.0)),
                backing_scale_factor: 2.0,
            },
        ];
        let mut sc = ScreenCache::new_with(stub);
        assert_eq!(
            vec![
                CGRect::new(CGPoint::new(0.0, 25.0), CGSize::new(3840.0, 2059.0)),
                CGRect::new(CGPoint::new(3840.0, 1112.0), CGSize::new(1512.0, 950.0)),
            ],
            sc.update_screen_config(ns_screens)
                .unwrap()
                .0
                .iter()
                .map(|s| s.visible_frame)
                .collect::<Vec<_>>()
        );
    }
}
