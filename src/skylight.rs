//! FFI bindings for SkyLight, CoreGraphics, and CoreFoundation.
//!
//! These are private/undocumented APIs used by the WindowServer compositor.
//! Signatures sourced from JankyBorders and yabai reverse engineering.

#![allow(non_snake_case, non_upper_case_globals, dead_code)]

use std::ffi::c_void;

// --- Core types ---

pub type CGError = i32;
pub type CGSConnectionID = i32;
pub const kCGErrorSuccess: CGError = 0;
pub const kCGBackingStoreBuffered: i32 = 2;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct CGPoint {
    pub x: f64,
    pub y: f64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct CGSize {
    pub width: f64,
    pub height: f64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct CGRect {
    pub origin: CGPoint,
    pub size: CGSize,
}

impl CGRect {
    pub fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self {
            origin: CGPoint { x, y },
            size: CGSize {
                width: w,
                height: h,
            },
        }
    }

    pub fn inset(&self, dx: f64, dy: f64) -> Self {
        Self::new(
            self.origin.x + dx,
            self.origin.y + dy,
            self.size.width - 2.0 * dx,
            self.size.height - 2.0 * dy,
        )
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CGAffineTransform {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub tx: f64,
    pub ty: f64,
}

impl Default for CGAffineTransform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl CGAffineTransform {
    pub const IDENTITY: Self = Self {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        tx: 0.0,
        ty: 0.0,
    };
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ProcessSerialNumber {
    pub high: u32,
    pub low: u32,
}

// --- Opaque CF types ---

pub type CFTypeRef = *const c_void;
pub type CFArrayRef = *const c_void;
pub type CFStringRef = *const c_void;
pub type CFDictionaryRef = *const c_void;
pub type CFNumberRef = *const c_void;
pub type CFMachPortRef = *const c_void;
pub type CFRunLoopSourceRef = *const c_void;
pub type CFRunLoopRef = *const c_void;
pub type CFAllocatorRef = *const c_void;
pub type CGContextRef = *mut c_void;
pub type CGPathRef = *const c_void;
pub type CGMutablePathRef = *mut c_void;
pub type CGEventRef = *const c_void;
pub type CGDisplayModeRef = *const c_void;

// CF constants
pub const kCFNumberSInt32Type: i32 = 3;
pub const kCFNumberSInt64Type: i32 = 4;
pub const kCFNumberCFIndexType: i32 = 14;
pub const kCFBooleanFalse: *const c_void = std::ptr::null(); // placeholder

// --- SkyLight / CGS functions ---

unsafe extern "C" {
    // Connection
    pub fn SLSMainConnectionID() -> CGSConnectionID;
    pub fn SLSNewConnection(zero: i32, cid: *mut CGSConnectionID) -> CGError;
    pub fn SLSReleaseConnection(cid: CGSConnectionID) -> CGError;

    // Event port
    pub fn SLSGetEventPort(cid: CGSConnectionID, port_out: *mut u32) -> CGError;
    pub fn SLEventCreateNextEvent(cid: CGSConnectionID) -> CGEventRef;
    pub fn _CFMachPortSetOptions(mach_port: CFMachPortRef, options: i32);

    // Event registration
    pub fn SLSRegisterNotifyProc(
        handler: *const c_void,
        event: u32,
        context: *mut c_void,
    ) -> CGError;

    pub fn SLSRequestNotificationsForWindows(
        cid: CGSConnectionID,
        window_list: *const u32,
        window_count: i32,
    ) -> CGError;

    // Window queries
    pub fn SLSGetWindowOwner(
        cid: CGSConnectionID,
        wid: u32,
        out_cid: *mut CGSConnectionID,
    ) -> CGError;
    pub fn SLSConnectionGetPID(cid: CGSConnectionID, pid: *mut i32) -> CGError;
    pub fn SLSGetWindowBounds(cid: CGSConnectionID, wid: u32, frame: *mut CGRect) -> CGError;
    pub fn SLSWindowIsOrderedIn(cid: CGSConnectionID, wid: u32, shown: *mut bool) -> CGError;
    pub fn SLSGetWindowLevel(cid: CGSConnectionID, wid: u32, level_out: *mut i64) -> CGError;

    // Window iterator queries
    pub fn SLSWindowQueryWindows(
        cid: CGSConnectionID,
        windows: CFArrayRef,
        options: u32,
    ) -> CFTypeRef;
    pub fn SLSWindowQueryResultCopyWindows(window_query: CFTypeRef) -> CFTypeRef;
    pub fn SLSWindowIteratorGetCount(iterator: CFTypeRef) -> i32;
    pub fn SLSWindowIteratorAdvance(iterator: CFTypeRef) -> bool;
    pub fn SLSWindowIteratorGetParentID(iterator: CFTypeRef) -> u32;
    pub fn SLSWindowIteratorGetWindowID(iterator: CFTypeRef) -> u32;
    pub fn SLSWindowIteratorGetTags(iterator: CFTypeRef) -> u64;
    pub fn SLSWindowIteratorGetAttributes(iterator: CFTypeRef) -> u64;
    pub fn SLSWindowIteratorGetLevel(iterator: CFTypeRef) -> i32;

    // Window lifecycle
    pub fn SLSNewWindow(
        cid: CGSConnectionID,
        window_type: i32,
        x: f32,
        y: f32,
        region: CFTypeRef,
        wid_out: *mut u32,
    ) -> CGError;
    /// JankyBorders' `SLSNewWindowWithOpaqueShapeAndContext` — creates a
    /// window with a custom hit-test shape and tag bits applied at
    /// creation. Used so that screenshot-exclusion tag bit 9 lands on
    /// the window before macOS Tahoe's compositor classifies it; setting
    /// the bit post-creation is unreliable on Tahoe.
    /// Reference: .refs/JankyBorders/src/misc/window.h:239
    /// Reference: .refs/JankyBorders/src/misc/extern.h
    pub fn SLSNewWindowWithOpaqueShapeAndContext(
        cid: CGSConnectionID,
        window_type: i32,
        region: CFTypeRef,
        opaque_shape: CFTypeRef,
        options: i32,
        tags: *mut u64,
        x: f32,
        y: f32,
        tag_size: i32,
        wid_out: *mut u32,
        context: *mut std::ffi::c_void,
    ) -> CGError;
    pub fn SLSReleaseWindow(cid: CGSConnectionID, wid: u32) -> CGError;

    // Window properties
    pub fn SLSSetWindowTags(
        cid: CGSConnectionID,
        wid: u32,
        tags: *const u64,
        tag_size: i32,
    ) -> CGError;
    pub fn SLSClearWindowTags(
        cid: CGSConnectionID,
        wid: u32,
        tags: *const u64,
        tag_size: i32,
    ) -> CGError;
    pub fn CGSGetWindowTags(
        cid: CGSConnectionID,
        wid: u32,
        tags: *mut u64,
        tag_size: i32,
    ) -> CGError;
    pub fn SLSSetWindowShape(
        cid: CGSConnectionID,
        wid: u32,
        x_offset: f32,
        y_offset: f32,
        shape: CFTypeRef,
    ) -> CGError;
    pub fn SLSSetWindowResolution(cid: CGSConnectionID, wid: u32, res: f64) -> CGError;
    pub fn SLSSetWindowOpacity(cid: CGSConnectionID, wid: u32, is_opaque: bool) -> CGError;
    /// SLS-level NSWindow.sharingType. Values: 0 = None (excluded from
    /// screen capture / picker / recording — equivalent to
    /// kCGWindowSharingNone), 1 = ReadOnly, 2 = ReadWrite.
    pub fn SLSSetWindowSharingState(cid: CGSConnectionID, wid: u32, state: u32) -> CGError;
    pub fn SLSGetWindowSharingState(
        cid: CGSConnectionID,
        wid: u32,
        state_out: *mut u32,
    ) -> CGError;
    /// Mask of events the SLS window captures. Set to 0 to make the window
    /// click-through (mouse events pass to the window beneath).
    pub fn SLSSetWindowEventMask(cid: CGSConnectionID, wid: u32, mask: u32) -> CGError;
    /// Hit-test/input shape. An empty region passes all mouse events
    /// through to the window beneath. Equivalent to NSWindow's
    /// `setIgnoresMouseEvents(true)` at the SLS layer.
    pub fn SLSSetWindowEventShape(cid: CGSConnectionID, wid: u32, shape: CFTypeRef) -> CGError;
    pub fn SLSSetWindowAlpha(cid: CGSConnectionID, wid: u32, alpha: f32) -> CGError;
    pub fn SLSSetWindowBackgroundBlurRadius(cid: CGSConnectionID, wid: u32, radius: u32)
    -> CGError;
    pub fn SLSSetWindowLevel(cid: CGSConnectionID, wid: u32, level: i32) -> CGError;
    pub fn SLSOrderWindow(cid: CGSConnectionID, wid: u32, mode: i32, relative_to: u32) -> CGError;
    pub fn SLSMoveWindow(cid: CGSConnectionID, wid: u32, point: *const CGPoint) -> CGError;

    // Shadow
    pub fn SLSWindowSetShadowProperties(wid: u32, properties: CFDictionaryRef) -> CGError;

    // Drawing context
    pub fn SLWindowContextCreate(
        cid: CGSConnectionID,
        wid: u32,
        options: CFDictionaryRef,
    ) -> CGContextRef;

    // Transactions
    pub fn SLSTransactionCreate(cid: CGSConnectionID) -> CFTypeRef;
    pub fn SLSTransactionSetWindowLevel(transaction: CFTypeRef, wid: u32, level: i32) -> CGError;
    pub fn SLSTransactionMoveWindowWithGroup(
        transaction: CFTypeRef,
        wid: u32,
        point: CGPoint,
    ) -> CGError;
    pub fn SLSTransactionOrderWindow(
        transaction: CFTypeRef,
        wid: u32,
        order: i32,
        rel_wid: u32,
    ) -> CGError;
    pub fn SLSTransactionSetWindowAlpha(transaction: CFTypeRef, wid: u32, alpha: f32) -> CGError;
    pub fn SLSTransactionSetWindowTransform(
        transaction: CFTypeRef,
        wid: u32,
        not: i32,
        important: i32,
        transform: CGAffineTransform,
    ) -> CGError;
    pub fn SLSTransactionCommit(transaction: CFTypeRef, synchronous: i32) -> CGError;
    pub fn SLSTransactionSetWindowShape(
        transaction: CFTypeRef,
        wid: u32,
        x_offset: f32,
        y_offset: f32,
        shape: CFTypeRef,
    ) -> CGError;

    // Flicker suppression
    pub fn SLSDisableUpdate(cid: CGSConnectionID) -> CGError;
    pub fn SLSReenableUpdate(cid: CGSConnectionID) -> CGError;
    pub fn SLSFlushWindowContentRegion(
        cid: CGSConnectionID,
        wid: u32,
        dirty: *const c_void,
    ) -> CGError;

    // Space management
    pub fn SLSCopySpacesForWindows(
        cid: CGSConnectionID,
        selector: i32,
        window_list: CFArrayRef,
    ) -> CFArrayRef;
    pub fn SLSCopyManagedDisplays(cid: CGSConnectionID) -> CFArrayRef;
    pub fn SLSCopyManagedDisplaySpaces(cid: CGSConnectionID) -> CFArrayRef;
    pub fn SLSCopyManagedDisplayForWindow(cid: CGSConnectionID, wid: u32) -> CFStringRef;
    pub fn SLSManagedDisplayGetCurrentSpace(cid: CGSConnectionID, uuid: CFStringRef) -> u64;
    pub fn SLSCopyActiveMenuBarDisplayIdentifier(cid: CGSConnectionID) -> CFStringRef;
    pub fn SLSMoveWindowsToManagedSpace(
        cid: CGSConnectionID,
        window_list: CFArrayRef,
        sid: u64,
    ) -> CGError;

    // Window enumeration
    pub fn SLSCopyWindowsWithOptionsAndTags(
        cid: CGSConnectionID,
        owner: u32,
        spaces: CFArrayRef,
        options: u32,
        set_tags: *const u64,
        clear_tags: *const u64,
    ) -> CFArrayRef;

    // Front process detection
    pub fn _SLPSGetFrontProcess(psn: *mut ProcessSerialNumber) -> i32;
    pub fn SLSGetConnectionIDForPSN(
        cid: CGSConnectionID,
        psn: *mut ProcessSerialNumber,
        psn_cid: *mut CGSConnectionID,
    ) -> CGError;

    // Region
    pub fn CGSNewRegionWithRect(rect: *const CGRect, region: *mut CFTypeRef) -> CGError;
}

// --- CoreGraphics drawing ---

// --- CGWindowList (public CoreGraphics API) ---

pub const kCGWindowListOptionOnScreenOnly: u32 = 1 << 0;
pub const kCGWindowListOptionAll: u32 = 0;
pub const kCGNullWindowID: u32 = 0;

unsafe extern "C" {
    pub fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> CFArrayRef;
    pub fn CGGetDisplaysWithPoint(
        point: CGPoint,
        max_displays: u32,
        displays: *mut u32,
        count: *mut u32,
    ) -> CGError;
    pub fn CGDisplayCopyDisplayMode(display: u32) -> CGDisplayModeRef;
    pub fn CGDisplayModeGetWidth(mode: CGDisplayModeRef) -> usize;
    pub fn CGDisplayModeGetHeight(mode: CGDisplayModeRef) -> usize;
    pub fn CGDisplayModeGetPixelWidth(mode: CGDisplayModeRef) -> usize;
    pub fn CGDisplayModeGetPixelHeight(mode: CGDisplayModeRef) -> usize;
    pub fn CFDictionaryGetValueIfPresent(
        dict: CFDictionaryRef,
        key: CFTypeRef,
        value_out: *mut CFTypeRef,
    ) -> bool;
    pub fn CFStringCreateWithCString(
        alloc: CFAllocatorRef,
        c_str: *const u8,
        encoding: u32,
    ) -> CFStringRef;
}

pub const kCFStringEncodingUTF8: u32 = 0x0800_0100;

// --- CoreGraphics drawing ---

unsafe extern "C" {
    pub fn CGContextSetRGBStrokeColor(ctx: CGContextRef, r: f64, g: f64, b: f64, a: f64);
    pub fn CGContextSetRGBFillColor(ctx: CGContextRef, r: f64, g: f64, b: f64, a: f64);
    pub fn CGContextSetLineWidth(ctx: CGContextRef, width: f64);
    pub fn CGContextClearRect(ctx: CGContextRef, rect: CGRect);
    pub fn CGContextEOFillPath(ctx: CGContextRef);
    pub fn CGContextAddPath(ctx: CGContextRef, path: CGPathRef);
    pub fn CGContextStrokePath(ctx: CGContextRef);
    pub fn CGContextFillPath(ctx: CGContextRef);
    pub fn CGContextFlush(ctx: CGContextRef);
    pub fn CGContextRelease(ctx: CGContextRef);
    pub fn CGContextSaveGState(ctx: CGContextRef);
    pub fn CGContextRestoreGState(ctx: CGContextRef);
    pub fn CGContextSetInterpolationQuality(ctx: CGContextRef, quality: i32);
    pub fn CGContextClip(ctx: CGContextRef);
    pub fn CGContextEOClip(ctx: CGContextRef);
    pub fn CGPathCreateWithRoundedRect(
        rect: CGRect,
        rx: f64,
        ry: f64,
        transform: *const CGAffineTransform,
    ) -> CGPathRef;
    pub fn CGPathCreateMutable() -> CGMutablePathRef;
    pub fn CGPathAddRoundedRect(
        path: CGMutablePathRef,
        transform: *const CGAffineTransform,
        rect: CGRect,
        rx: f64,
        ry: f64,
    );
    pub fn CGPathAddRect(path: CGMutablePathRef, transform: *const CGAffineTransform, rect: CGRect);
    pub fn CGPathAddPath(
        path: CGMutablePathRef,
        transform: *const CGAffineTransform,
        other: CGPathRef,
    );
    pub fn CGPathRelease(path: CGPathRef);
}

// --- CoreFoundation ---

unsafe extern "C" {
    pub fn CFRelease(cf: CFTypeRef);
    pub fn CFRetain(cf: CFTypeRef) -> CFTypeRef;

    pub fn CFArrayGetCount(array: CFArrayRef) -> i64;
    pub fn CFArrayGetValueAtIndex(array: CFArrayRef, idx: i64) -> CFTypeRef;
    pub fn CFArrayCreate(
        allocator: CFAllocatorRef,
        values: *const CFTypeRef,
        count: i64,
        callbacks: *const c_void,
    ) -> CFArrayRef;

    pub fn CFNumberCreate(
        allocator: CFAllocatorRef,
        the_type: i32,
        value_ptr: *const c_void,
    ) -> CFNumberRef;
    pub fn CFNumberGetValue(number: CFNumberRef, the_type: i32, value_ptr: *mut c_void) -> bool;
    pub fn CFNumberGetType(number: CFNumberRef) -> i32;

    pub fn CFDictionaryCreate(
        allocator: CFAllocatorRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        count: i64,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFDictionaryRef;
    pub fn CFDictionaryGetValue(dict: CFDictionaryRef, key: CFTypeRef) -> CFTypeRef;

    pub fn CFMachPortCreateWithPort(
        allocator: CFAllocatorRef,
        port: u32,
        callback: *const c_void,
        context: *const c_void,
        should_free: bool,
    ) -> CFMachPortRef;

    pub fn CFMachPortCreateRunLoopSource(
        allocator: CFAllocatorRef,
        port: CFMachPortRef,
        order: i64,
    ) -> CFRunLoopSourceRef;

    pub fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    pub fn CFRunLoopGetMain() -> CFRunLoopRef;
    pub fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    pub fn CFRunLoopRun();
    pub fn CFRunLoopStop(rl: CFRunLoopRef);
    pub fn CFRunLoopWakeUp(rl: CFRunLoopRef);

    pub static kCFAllocatorDefault: CFAllocatorRef;
    pub static kCFTypeDictionaryKeyCallBacks: c_void;
    pub static kCFTypeDictionaryValueCallBacks: c_void;
    pub static kCFTypeArrayCallBacks: c_void;
    pub static kCFRunLoopDefaultMode: CFStringRef;
}

// --- macOS process ---

unsafe extern "C" {
    pub fn getpid() -> i32;
    pub fn pid_for_task(task: u32, pid: *mut i32) -> i32;
    pub static mach_task_self_: u32;
}

pub fn mach_task_self() -> u32 {
    unsafe { mach_task_self_ }
}

// --- Helper: create CFArray of CFNumbers ---

pub unsafe fn cfarray_of_cfnumbers(
    values: *const c_void,
    size: usize,
    count: i32,
    num_type: i32,
) -> CFArrayRef {
    unsafe {
        let mut temp: Vec<CFNumberRef> = Vec::with_capacity(count as usize);
        for i in 0..count {
            let ptr = (values as *const u8).add(size * i as usize) as *const c_void;
            temp.push(CFNumberCreate(std::ptr::null(), num_type, ptr));
        }
        let array = CFArrayCreate(
            std::ptr::null(),
            temp.as_ptr() as *const CFTypeRef,
            count as i64,
            &kCFTypeArrayCallBacks as *const _,
        );
        for n in &temp {
            CFRelease(*n);
        }
        array
    }
}
