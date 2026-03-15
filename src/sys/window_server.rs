// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::ffi::{c_int, c_void};
use std::marker::PhantomData;

use accessibility::AXUIElement;
use accessibility_sys::{AXError, AXUIElementRef, kAXErrorSuccess, pid_t};
use core_foundation::array::CFArray;
use core_foundation::base::{CFType, CFTypeRef, ItemRef, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::base::CGError;
use core_graphics::display::CGWindowListCopyWindowInfo;
use core_graphics::window::{
    CGWindowID, CGWindowListCreateDescriptionFromArray, kCGNullWindowID, kCGWindowBounds,
    kCGWindowLayer, kCGWindowListExcludeDesktopElements, kCGWindowListOptionOnScreenOnly,
    kCGWindowNumber, kCGWindowOwnerPID,
};
use objc2_app_kit::NSWindow;
use objc2_core_foundation::{CGPoint, CGRect};
use objc2_foundation::MainThreadMarker;
use serde::{Deserialize, Serialize};
use sorted_vec::SortedVec;
use static_assertions::assert_not_impl_any;
use tracing::warn;

use super::geometry::{CGRectDef, ToICrate};
use super::screen::CoordinateConverter;
use crate::sys::app::ProcessSerialNumber;

/// The window ID used by the window server.
///
/// Obtaining this from AXUIElement uses a private API and is *not* guaranteed.
/// Any functionality depending on this should have a backup plan in case it
/// breaks in the future.
#[derive(PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Clone, Copy, Serialize, Deserialize)]
pub struct WindowServerId(pub CGWindowID);

impl WindowServerId {
    pub fn new(id: CGWindowID) -> Self {
        WindowServerId(id)
    }

    pub fn as_u32(&self) -> u32 {
        self.0
    }
}

impl Into<u32> for WindowServerId {
    fn into(self) -> u32 {
        self.0
    }
}

impl TryFrom<&AXUIElement> for WindowServerId {
    type Error = accessibility::Error;
    fn try_from(element: &AXUIElement) -> Result<Self, accessibility::Error> {
        let mut id = 0;
        let res = unsafe { _AXUIElementGetWindow(element.as_concrete_TypeRef(), &mut id) };
        if res != kAXErrorSuccess {
            return Err(accessibility::Error::Ax(res));
        }
        Ok(WindowServerId(id))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(unused)]
pub struct WindowServerInfo {
    pub id: WindowServerId,
    pub pid: pid_t,
    pub layer: i32,
    #[serde(with = "CGRectDef")]
    pub frame: CGRect,
}

/// Returns a list of windows visible on the screen, in order starting with the
/// frontmost. Excludes desktop elements.
pub fn get_visible_windows_with_layer(layer: Option<i32>) -> Vec<WindowServerInfo> {
    get_visible_windows_raw()
        .iter()
        .filter_map(|win| make_info(win, layer))
        .collect::<Vec<_>>()
}

/// Returns only the window server IDs of windows visible on the screen.
/// This is cheaper than `get_visible_windows_with_layer` when you don't need
/// the full `WindowServerInfo`.
pub fn get_visible_window_ids() -> Vec<WindowServerId> {
    get_visible_windows_raw()
        .iter()
        .filter_map(|win| {
            let id: u32 = get_num(&win, unsafe { kCGWindowNumber })?.try_into().ok()?;
            Some(WindowServerId(id))
        })
        .collect()
}

/// Returns a list of windows visible on the screen, in order starting with the
/// frontmost.
pub fn get_visible_windows_raw() -> CFArray<CFDictionary<CFString, CFType>> {
    // Note that the ordering is not documented. But
    // NSWindow::windowNumbersWithOptions *is* documented to return the windows
    // in order, so we could always combine their information if the behavior
    // changed.
    unsafe {
        CFArray::wrap_under_get_rule(CGWindowListCopyWindowInfo(
            kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
            kCGNullWindowID,
        ))
    }
}

fn make_info(
    win: ItemRef<CFDictionary<CFString, CFType>>,
    layer_filter: Option<i32>,
) -> Option<WindowServerInfo> {
    let layer = get_num(&win, unsafe { kCGWindowLayer })?.try_into().ok()?;
    if !(layer_filter.is_none() || layer_filter == Some(layer)) {
        return None;
    }
    let id = get_num(&win, unsafe { kCGWindowNumber })?;
    let pid = get_num(&win, unsafe { kCGWindowOwnerPID })?;
    let frame: CFDictionary = win.find(unsafe { kCGWindowBounds })?.downcast()?;
    let frame = core_graphics_types::geometry::CGRect::from_dict_representation(&frame)?;
    Some(WindowServerInfo {
        id: WindowServerId(id.try_into().ok()?),
        pid: pid.try_into().ok()?,
        layer,
        frame: frame.to_icrate(),
    })
}

pub fn get_windows(ids: &[WindowServerId]) -> Vec<WindowServerInfo> {
    if ids.is_empty() {
        return Vec::new();
    }
    get_windows_inner(ids).iter().flat_map(|w| make_info(w, None)).collect()
}

pub fn get_window(id: WindowServerId) -> Option<WindowServerInfo> {
    get_windows_inner(&[id]).iter().next().and_then(|w| make_info(w, None))
}

fn get_windows_inner(ids: &[WindowServerId]) -> CFArray<CFDictionary<CFString, CFType>> {
    let array = CFArray::from_copyable(ids);
    unsafe {
        CFArray::wrap_under_create_rule(CGWindowListCreateDescriptionFromArray(
            array.as_concrete_TypeRef(),
        ))
    }
}

fn get_num(dict: &CFDictionary<CFString, CFType>, key: CFStringRef) -> Option<i64> {
    let item: CFNumber = dict.find(key)?.downcast()?;
    Some(item.to_i64()?)
}

pub fn get_window_at_point(
    point: CGPoint,
    converter: CoordinateConverter,
    mtm: MainThreadMarker,
) -> Option<WindowServerId> {
    let ns_loc = converter.convert_point(point)?;
    let win = NSWindow::windowNumberAtPoint_belowWindowWithWindowNumber(ns_loc, 0, mtm);
    Some(WindowServerId(win as u32))
}

unsafe extern "C" {
    fn _AXUIElementGetWindow(elem: AXUIElementRef, wid: *mut CGWindowID) -> AXError;
}

/// Set the key window of the window server and application.
pub fn make_key_window(pid: pid_t, wsid: WindowServerId) -> Result<(), ()> {
    // See https://github.com/Hammerspoon/hammerspoon/issues/370#issuecomment-545545468.
    #[allow(non_upper_case_globals)]
    const kCPSUserGenerated: u32 = 0x200;

    let mut event1 = [0; 0x100];
    event1[0x04] = 0xf8;
    event1[0x08] = 0x01;
    event1[0x3a] = 0x10;
    event1[0x3c..0x3c + 4].copy_from_slice(&wsid.0.to_le_bytes());
    event1[0x20..(0x20 + 0x10)].fill(0xff);

    let mut event2 = event1.clone();
    event2[0x08] = 0x02;

    let psn = ProcessSerialNumber::for_pid(pid)?;
    let check = |err| if err == 0 { Ok(()) } else { Err(()) };
    unsafe {
        check(_SLPSSetFrontProcessWithOptions(&psn, wsid.0, kCPSUserGenerated))?;
        check(SLPSPostEventRecordTo(&psn, event1.as_ptr()))?;
        check(SLPSPostEventRecordTo(&psn, event2.as_ptr()))?;
    }
    Ok(())
}

pub type SLSConnectionID = c_int;

#[link(name = "SkyLight", kind = "framework")]
unsafe extern "C" {
    fn _SLPSSetFrontProcessWithOptions(
        psn: *const ProcessSerialNumber,
        wid: u32,
        mode: u32,
    ) -> CGError;

    fn SLPSPostEventRecordTo(psn: *const ProcessSerialNumber, bytes: *const u8) -> CGError;

    // safe fn SLSMainConnectionID() -> SLSConnectionID;
    safe fn SLSDefaultConnectionForThread() -> SLSConnectionID;

    safe fn SLSRegisterConnectionNotifyProc(
        cid: SLSConnectionID,
        callback: SkylightNotifierCallback,
        event: u32,
        context: *mut c_void,
    ) -> CGError;

    safe fn SLSRemoveConnectionNotifyProc(
        cid: SLSConnectionID,
        callback: SkylightNotifierCallback,
        event: u32,
        context: *mut c_void,
    ) -> CGError;

    fn SLSRequestNotificationsForWindows(
        cid: SLSConnectionID,
        window_list: *const CGWindowID,
        window_count: i32,
    ) -> CGError;
}

type SkylightNotifierCallback = extern "C-unwind" fn(
    event: u32,
    data: *mut c_void,
    data_len: usize,
    context: *mut c_void,
    cid: SLSConnectionID,
);

/// Manages a thread-local connection to the skylight server for subscribing to
/// events. This uses private APIs.
pub struct SkylightConnection {
    cid: SLSConnectionID,
    window_list: SortedVec<WindowServerId>,
    _phantom: PhantomData<*mut ()>,
}
assert_not_impl_any!(SkylightConnection: Send, Sync);

impl SkylightConnection {
    pub fn new(_mtm: MainThreadMarker) -> Self {
        Self {
            cid: SLSDefaultConnectionForThread(),
            window_list: SortedVec::new(),
            _phantom: PhantomData,
        }
    }

    pub fn add_window(&mut self, wsid: WindowServerId) -> Result<(), CGError> {
        self.window_list.insert(wsid);
        self.update_windows()
    }

    pub fn remove_window(&mut self, wsid: WindowServerId) -> Result<(), CGError> {
        self.window_list.remove_item(&wsid);
        self.update_windows()
    }

    pub fn on_window_destroyed(&mut self, wsid: WindowServerId) {
        self.window_list.remove_item(&wsid);
    }

    fn update_windows(&mut self) -> Result<(), CGError> {
        check(unsafe {
            SLSRequestNotificationsForWindows(
                self.cid,
                self.window_list.as_ptr().cast(),
                self.window_list.len() as i32,
            )
        })
    }

    pub fn on_event(
        &self,
        event: u32,
        callback: impl Fn(u32, &[u8]) + 'static,
    ) -> Result<SkylightNotifier, CGError> {
        SkylightNotifier::new_for_event(self.cid, event, callback)
    }
}

#[derive(Debug)]
pub struct SkylightNotifier {
    cid: SLSConnectionID,
    event: u32,
    internal_callback: SkylightNotifierCallback,
    callback: *mut (),
    dtor: unsafe fn(*mut ()),
}

fn check(err: CGError) -> Result<(), CGError> {
    if err == 0 { Ok(()) } else { Err(err) }
}

unsafe fn destruct<T>(ptr: *mut ()) {
    let _ = unsafe { Box::from_raw(ptr as *mut T) };
}

impl SkylightNotifier {
    fn new_for_event<F: Fn(u32, &[u8]) + 'static>(
        cid: SLSConnectionID,
        event: u32,
        callback: F,
    ) -> Result<Self, CGError> {
        extern "C-unwind" fn internal_callback<F: Fn(u32, &[u8]) + 'static>(
            event: u32,
            data: *mut c_void,
            data_len: usize,
            context: *mut c_void,
            _cid: SLSConnectionID,
        ) {
            // SAFETY: Pointer is from Box<F>::into_raw.
            let callback: &F = unsafe { &*context.cast() };
            // SAFETY: We assume the API is sound.
            let data = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), data_len) };
            callback(event, data)
        }
        let callback = Box::into_raw(Box::new(callback));
        check(SLSRegisterConnectionNotifyProc(
            cid,
            internal_callback::<F>,
            event,
            callback.cast(),
        ))?;
        Ok(Self {
            cid,
            internal_callback: internal_callback::<F>,
            callback: callback.cast(),
            dtor: destruct::<F>,
            event,
        })
    }
}

impl Drop for SkylightNotifier {
    fn drop(&mut self) {
        #[allow(irrefutable_let_patterns)]
        if let e = SLSRemoveConnectionNotifyProc(
            self.cid,
            self.internal_callback,
            self.event,
            self.callback.cast(),
        ) && e != 0
        {
            warn!("Failed to release SLS connection {cid}: {e}", cid = self.cid);
            return; // so we don't free data referred to by the callback
        }
        // SAFETY: destruct<F> takes a pointer to F.
        unsafe {
            (self.dtor)(self.callback);
        }
    }
}

#[expect(non_upper_case_globals)]
pub const kCGSWindowIsTerminated: u32 = 804;

/// This must be called to allow hiding the mouse from a background application.
///
/// It relies on a private API, so not guaranteed to continue working, but it is
/// discussed by Apple engineers on developer forums.
pub fn allow_hide_mouse() -> Result<(), CGError> {
    let cid = unsafe { CGSMainConnectionID() };
    let property = CFString::from_static_string("SetsCursorInBackground");
    check(unsafe {
        CGSSetConnectionProperty(
            cid,
            cid,
            property.as_concrete_TypeRef(),
            CFBoolean::true_value().as_CFTypeRef(),
        )
    })
}

type CGSConnectionID = c_int;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn CGSMainConnectionID() -> CGSConnectionID;
    fn CGSSetConnectionProperty(
        cid: CGSConnectionID,
        target_cid: CGSConnectionID,
        key: CFStringRef,
        value: CFTypeRef,
    ) -> CGError;
}
