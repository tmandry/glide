// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::ffi::{c_int, c_void};
use std::marker::PhantomData;
use std::ptr;

use accessibility::AXUIElement;
use accessibility_sys::pid_t;
use core_graphics::base::CGError;
use objc2_app_kit::NSWindow;
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFIndex, CFNumber, CFRetained, CFString, CFType, CGPoint,
    CGRect, CGSize,
};
use objc2_core_graphics::{
    CGRectMakeWithDictionaryRepresentation, CGWindowID, CGWindowListCopyWindowInfo,
    CGWindowListCreateDescriptionFromArray, CGWindowListOption, kCGNullWindowID, kCGWindowBounds,
    kCGWindowLayer, kCGWindowNumber, kCGWindowOwnerPID,
};
use objc2_foundation::MainThreadMarker;
use serde::{Deserialize, Serialize};
use sorted_vec::SortedVec;
use static_assertions::assert_not_impl_any;
use tracing::warn;

use super::geometry::CGRectDef;
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

/// Note that this can return kCGNullWindowID for some non-window elements; see
/// https://github.com/tmandry/glide/issues/248.
impl TryFrom<&AXUIElement> for WindowServerId {
    type Error = accessibility::Error;
    fn try_from(element: &AXUIElement) -> Result<Self, accessibility::Error> {
        let mut id = 0;
        let res = unsafe { _AXUIElementGetWindow(element.as_sys(), &mut id) };
        if let Some(err) = accessibility::AXError::from_raw(res) {
            return Err(accessibility::Error::Ax(err));
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

/// A snapshot of windows currently visible on screen from the window server.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WindowsOnScreen {
    pub visible: Vec<WindowServerId>,
    pub info: Vec<WindowServerInfo>,
}

impl WindowsOnScreen {
    pub fn new(info: Vec<WindowServerInfo>) -> Self {
        let visible = info.iter().map(|w| w.id).collect();
        Self { visible, info }
    }
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
pub fn get_visible_windows_raw() -> CFRetained<CFArray<CFDictionary<CFString, CFType>>> {
    // Note that the ordering is not documented. But
    // NSWindow::windowNumbersWithOptions *is* documented to return the windows
    // in order, so we could always combine their information if the behavior
    // changed.
    let options =
        CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements;
    let array = CGWindowListCopyWindowInfo(options, kCGNullWindowID)
        .expect("CGWindowListCopyWindowInfo returned NULL");
    // SAFETY: CGWindowListCopyWindowInfo returns an array of window info dicts.
    unsafe { CFRetained::cast_unchecked(array) }
}

fn make_info(
    win: CFRetained<CFDictionary<CFString, CFType>>,
    layer_filter: Option<i32>,
) -> Option<WindowServerInfo> {
    let layer = get_num(&win, unsafe { kCGWindowLayer })?.try_into().ok()?;
    if !(layer_filter.is_none() || layer_filter == Some(layer)) {
        return None;
    }
    let id = get_num(&win, unsafe { kCGWindowNumber })?;
    let pid = get_num(&win, unsafe { kCGWindowOwnerPID })?;
    let frame_dict: CFRetained<CFDictionary> =
        win.get(unsafe { kCGWindowBounds })?.downcast().ok()?;
    let mut frame = CGRect {
        origin: CGPoint { x: 0.0, y: 0.0 },
        size: CGSize { width: 0.0, height: 0.0 },
    };
    // SAFETY: `frame_dict` and `&mut frame` are valid for the duration of this call.
    if !unsafe { CGRectMakeWithDictionaryRepresentation(Some(&frame_dict), &mut frame) } {
        return None;
    }
    Some(WindowServerInfo {
        id: WindowServerId(id.try_into().ok()?),
        pid: pid.try_into().ok()?,
        layer,
        frame,
    })
}

pub fn get_all_windows_with_layer(layer: i32) -> Vec<WindowServerInfo> {
    let array = CGWindowListCopyWindowInfo(CGWindowListOption::OptionAll, kCGNullWindowID)
        .expect("CGWindowListCopyWindowInfo returned NULL");
    // SAFETY: CGWindowListCopyWindowInfo returns an array of window info dicts.
    let array: CFRetained<CFArray<CFDictionary<CFString, CFType>>> =
        unsafe { CFRetained::cast_unchecked(array) };
    array.iter().filter_map(|win| make_info(win, Some(layer))).collect()
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

fn get_windows_inner(
    ids: &[WindowServerId],
) -> CFRetained<CFArray<CFDictionary<CFString, CFType>>> {
    // SAFETY: `ids` outlives this call, and `CGWindowListCreateDescriptionFromArray`
    // reads it as an array of raw (non-retained) `CGWindowID` values, matching how
    // the array is constructed here (no retain/release callbacks).
    let array = unsafe {
        CFArray::new(
            None,
            ids.as_ptr() as *mut *const c_void,
            ids.len() as CFIndex,
            ptr::null(),
        )
    }
    .expect("CFArrayCreate returned NULL");
    let described = unsafe { CGWindowListCreateDescriptionFromArray(Some(&array)) }
        .expect("CGWindowListCreateDescriptionFromArray returned NULL");
    // SAFETY: CGWindowListCreateDescriptionFromArray returns an array of window
    // info dicts.
    unsafe { CFRetained::cast_unchecked(described) }
}

fn get_num(dict: &CFDictionary<CFString, CFType>, key: &CFString) -> Option<i64> {
    dict.get(key)?.downcast::<CFNumber>().ok()?.as_i64()
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
    fn _AXUIElementGetWindow(
        elem: &accessibility_sys::AXUIElement,
        wid: *mut CGWindowID,
    ) -> accessibility_sys::AXError;
}

/// Returns the process the window server considers frontmost.
///
/// Returns None if the window server could not be asked.
pub fn get_front_process_pid() -> Option<pid_t> {
    let mut psn = ProcessSerialNumber::default();
    // SAFETY: The out parameter is a valid pointer.
    if unsafe { _SLPSGetFrontProcess(&mut psn) } != 0 {
        return None;
    }
    psn.pid().ok()
}

/// Returns the process the window server routes keyboard events to.
///
/// This is not always the frontmost process. A non-activating panel, like the
/// one Spotlight uses, takes keyboard focus without activating the application
/// it belongs to.
///
/// Returns None if the window server could not be asked.
pub fn get_key_focus_pid() -> Option<pid_t> {
    let mut psn = ProcessSerialNumber::default();
    let mut is_front = 0;
    // SAFETY: Both out parameters are valid pointers.
    if unsafe { SLPSGetKeyFocusProcess(&mut psn, &mut is_front) } != 0 {
        return None;
    }
    psn.pid().ok()
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

    fn _SLPSGetFrontProcess(psn: *mut ProcessSerialNumber) -> CGError;

    /// Reads the process the window server routes keyboard events to. The
    /// second parameter is an out flag that is 1 when that process is the
    /// front process.
    fn SLPSGetKeyFocusProcess(psn: *mut ProcessSerialNumber, is_front: *mut u8) -> CGError;

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

    safe fn SLSRegisterNotifyProc(
        callback: GlobalNotifierCallback,
        event: u32,
        context: *mut c_void,
    ) -> CGError;

    safe fn SLSRemoveNotifyProc(
        callback: GlobalNotifierCallback,
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

/// Reads the payload of a window server notification.
///
/// Some notifications have no payload and pass a null pointer, sometimes with a
/// nonzero length.
///
/// # Safety
///
/// If `data` is non-null it must point to `data_len` initialized bytes that
/// outlive the returned slice.
unsafe fn notification_data<'a>(data: *mut c_void, data_len: usize) -> &'a [u8] {
    if data.is_null() {
        return &[];
    }
    unsafe { std::slice::from_raw_parts(data.cast::<u8>(), data_len) }
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
            let data = unsafe { notification_data(data, data_len) };
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

/// Subscribes to a window server notification event for the whole session.
///
/// Unlike [`SkylightConnection::on_event`], this is not limited to the windows
/// a connection has requested notifications for.
pub fn on_global_event(
    event: u32,
    callback: impl Fn(u32, &[u8]) + 'static,
) -> Result<GlobalNotifier, CGError> {
    GlobalNotifier::new_for_event(event, callback)
}

#[derive(Debug)]
pub struct GlobalNotifier {
    event: u32,
    internal_callback: GlobalNotifierCallback,
    callback: *mut (),
    dtor: unsafe fn(*mut ()),
}

type GlobalNotifierCallback =
    extern "C-unwind" fn(event: u32, data: *mut c_void, data_len: usize, context: *mut c_void);

impl GlobalNotifier {
    fn new_for_event<F: Fn(u32, &[u8]) + 'static>(
        event: u32,
        callback: F,
    ) -> Result<Self, CGError> {
        extern "C-unwind" fn internal_callback<F: Fn(u32, &[u8]) + 'static>(
            event: u32,
            data: *mut c_void,
            data_len: usize,
            context: *mut c_void,
        ) {
            // SAFETY: Pointer is from Box<F>::into_raw.
            let callback: &F = unsafe { &*context.cast() };
            // SAFETY: We assume the API is sound.
            let data = unsafe { notification_data(data, data_len) };
            callback(event, data)
        }
        let callback = Box::into_raw(Box::new(callback));
        check(SLSRegisterNotifyProc(
            internal_callback::<F>,
            event,
            callback.cast(),
        ))?;
        Ok(Self {
            event,
            internal_callback: internal_callback::<F>,
            callback: callback.cast(),
            dtor: destruct::<F>,
        })
    }
}

impl Drop for GlobalNotifier {
    fn drop(&mut self) {
        #[allow(irrefutable_let_patterns)]
        if let e = SLSRemoveNotifyProc(self.internal_callback, self.event, self.callback.cast())
            && e != 0
        {
            warn!("Failed to remove notify proc for event {}: {e}", self.event);
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

/// Sent for every window in the session as it is created, whether or not
/// notifications were requested for it.
#[expect(non_upper_case_globals)]
pub const kCGSWindowCreated: u32 = 811;

/// Sent when a window whose notifications were requested is ordered in.
#[expect(non_upper_case_globals)]
pub const kCGSWindowIsVisible: u32 = 815;

/// Sent when a window whose notifications were requested is ordered out.
#[expect(non_upper_case_globals)]
pub const kCGSWindowIsInvisible: u32 = 816;

/// This must be called to allow hiding the mouse from a background application.
///
/// It relies on a private API, so not guaranteed to continue working, but it is
/// discussed by Apple engineers on developer forums.
pub fn allow_hide_mouse() -> Result<(), CGError> {
    let cid = unsafe { CGSMainConnectionID() };
    let property = CFString::from_static_str("SetsCursorInBackground");
    check(unsafe { CGSSetConnectionProperty(cid, cid, &property, CFBoolean::new(true)) })
}

type CGSConnectionID = c_int;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn CGSMainConnectionID() -> CGSConnectionID;
    fn CGSSetConnectionProperty(
        cid: CGSConnectionID,
        target_cid: CGSConnectionID,
        key: &CFString,
        value: &CFType,
    ) -> CGError;
}
