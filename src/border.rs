//! Border overlay window: create, draw, reposition, destroy.
//!
//! Each BorderWindow is a transparent SLS overlay that draws a colored
//! rounded-rect stroke around a target application window. Follows the
//! JankyBorders rendering pattern exactly.

use crate::config::Color;
use crate::skylight::*;
use std::ptr;

/// Padding between the target window edge and the border stroke center.
const BORDER_PADDING: f64 = 0.0;

pub struct BorderWindow {
    pub cid: CGSConnectionID,
    pub wid: u32,
    pub target_wid: u32,
    pub frame: CGRect,
    pub target_bounds: CGRect,
    pub origin: CGPoint,
    pub focused: bool,
    pub needs_redraw: bool,
    context: CGContextRef,
    hidpi: bool,
}

// SAFETY: BorderWindow is only accessed from a single thread (the event loop thread).
// The raw pointer (context) is a CGContextRef that we create and destroy ourselves.
unsafe impl Send for BorderWindow {}

impl BorderWindow {
    /// Create a new border overlay for `target_wid`.
    /// Creates the overlay at the correct size and position from the start
    /// (no 1x1 + reshape — that path is broken).
    pub fn new(cid: CGSConnectionID, target_wid: u32, hidpi: bool, border_width: f64) -> Option<Self> {
        // Get target window bounds from compositor
        let mut target_bounds = CGRect::default();
        let err = unsafe { SLSGetWindowBounds(cid, target_wid, &mut target_bounds) };
        if err != kCGErrorSuccess {
            return None;
        }

        // Calculate overlay frame and position
        let bw = border_width;
        let overlay_w = target_bounds.size.width + 2.0 * bw;
        let overlay_h = target_bounds.size.height + 2.0 * bw;
        let overlay_x = target_bounds.origin.x - bw;
        let overlay_y = target_bounds.origin.y - bw;

        let frame = CGRect::new(0.0, 0.0, overlay_w, overlay_h);
        let origin = CGPoint { x: overlay_x, y: overlay_y };

        // === INLINE EVERYTHING like smoke2 (proven working) ===
        unsafe {
            let mut region: CFTypeRef = ptr::null();
            CGSNewRegionWithRect(&frame, &mut region);

            let mut wid: u32 = 0;
            SLSNewWindow(cid, 2, overlay_x as f32, overlay_y as f32, region, &mut wid);
            CFRelease(region);

            if wid == 0 {
                return None;
            }

            SLSSetWindowResolution(cid, wid, if hidpi { 2.0 } else { 1.0 });
            SLSSetWindowOpacity(cid, wid, false);
            SLSSetWindowLevel(cid, wid, 25);
            SLSOrderWindow(cid, wid, 1, 0);

            // Draw solid blue (like smoke2)
            let ctx = SLWindowContextCreate(cid, wid, ptr::null());
            eprintln!("[new] wid={wid} target={target_wid} ctx_null={} pos=({overlay_x:.0},{overlay_y:.0}) size=({overlay_w:.0},{overlay_h:.0})", ctx.is_null());
            if !ctx.is_null() {
                let scale = if hidpi { 2.0 } else { 1.0 };
                let full = CGRect::new(0.0, 0.0, overlay_w * scale, overlay_h * scale);
                CGContextClearRect(ctx, full);
                CGContextSetRGBFillColor(ctx, 0.2, 0.5, 0.9, 1.0);
                let path = CGPathCreateMutable();
                CGPathAddRect(path, ptr::null(), full);
                CGContextAddPath(ctx, path as CGPathRef);
                CGContextFillPath(ctx);
                CGPathRelease(path as CGPathRef);
                CGContextFlush(ctx);
                SLSFlushWindowContentRegion(cid, wid, ptr::null());
                CGContextRelease(ctx);
            }

            Some(Self {
                cid,
                wid,
                target_wid,
                frame,
                target_bounds,
                origin,
                focused: false,
                needs_redraw: false, // already drawn
                context: ptr::null_mut(),
                hidpi,
            })
        }
    }

    /// Calculate the overlay frame from target bounds and border width.
    fn calculate_frame(&self, border_width: f64) -> (CGRect, CGPoint) {
        let offset = border_width + BORDER_PADDING;
        let frame = CGRect::new(
            0.0,
            0.0,
            self.target_bounds.size.width + 2.0 * offset,
            self.target_bounds.size.height + 2.0 * offset,
        );
        let origin = CGPoint {
            x: self.target_bounds.origin.x - offset,
            y: self.target_bounds.origin.y - offset,
        };
        (frame, origin)
    }

    /// Full update: recalculate bounds, reshape if needed, redraw, reposition.
    pub fn update(
        &mut self,
        active_color: &Color,
        inactive_color: &Color,
        border_width: f64,
        radius: f64,
        border_order: i32,
    ) {
        if self.wid == 0 {
            return;
        }

        // Refresh target bounds from compositor
        let mut new_bounds = CGRect::default();
        let err = unsafe { SLSGetWindowBounds(self.cid, self.target_wid, &mut new_bounds) };
        if err != kCGErrorSuccess {
            return;
        }
        self.target_bounds = new_bounds;

        // Check if target is ordered in (visible)
        let mut shown = false;
        unsafe { SLSWindowIsOrderedIn(self.cid, self.target_wid, &mut shown) };
        if !shown {
            eprintln!("[border] wid={} target={} NOT SHOWN, hiding", self.wid, self.target_wid);
            self.hide();
            return;
        }

        let (frame, origin) = self.calculate_frame(border_width);

        // Reshape if target size changed
        let size_changed = (frame.size.width - self.frame.size.width).abs() > 0.5
            || (frame.size.height - self.frame.size.height).abs() > 0.5;

        if size_changed {
            eprintln!("[border] wid={} reshaping to {:.0}x{:.0}", self.wid, frame.size.width, frame.size.height);
            unsafe {
                let mut region: CFTypeRef = ptr::null();
                CGSNewRegionWithRect(&frame, &mut region);
                if !region.is_null() {
                    SLSSetWindowShape(self.cid, self.wid, -9999.0, -9999.0, region);
                    CFRelease(region);
                }
            }
            self.frame = frame;
            self.needs_redraw = true;
        }

        self.origin = origin;

        // Draw if needed
        if self.needs_redraw {
            eprintln!("[border] wid={} DRAWING at ({:.0},{:.0}) size {:.0}x{:.0}",
                self.wid, self.origin.x, self.origin.y, self.frame.size.width, self.frame.size.height);
            let color = if self.focused { active_color } else { inactive_color };
            self.draw(color, border_width, radius);
        }

        // Position and order
        eprintln!("[border] wid={} ORDERING level=25 above-all", self.wid);
        unsafe {
            SLSMoveWindow(self.cid, self.wid, &self.origin);
            SLSSetWindowLevel(self.cid, self.wid, 25);
            SLSOrderWindow(self.cid, self.wid, 1, 0);
        }
    }

    /// Move overlay to track window (fast path for move-only events).
    pub fn reposition_only(&mut self, border_width: f64) {
        if self.wid == 0 {
            return;
        }

        let mut new_bounds = CGRect::default();
        let err = unsafe { SLSGetWindowBounds(self.cid, self.target_wid, &mut new_bounds) };
        if err != kCGErrorSuccess {
            return;
        }
        self.target_bounds = new_bounds;

        let (_, origin) = self.calculate_frame(border_width);
        self.origin = origin;

        unsafe {
            SLSMoveWindow(self.cid, self.wid, &self.origin);
        }
    }

    /// Draw the border stroke into the overlay context.
    /// Currently a debug version: solid red fill, identical to working smoke test.
    fn draw(&mut self, _color: &Color, _border_width: f64, _radius: f64) {
        // Get a fresh context every time (matches smoke test pattern)
        let ctx = unsafe { SLWindowContextCreate(self.cid, self.wid, ptr::null()) };
        eprintln!("[draw] wid={} ctx_null={} cid={}", self.wid, ctx.is_null(), self.cid);
        if ctx.is_null() {
            eprintln!("[draw] wid={} CONTEXT IS NULL — cannot draw!", self.wid);
            self.needs_redraw = false;
            return;
        }

        let scale = if self.hidpi { 2.0 } else { 1.0 };
        let w = self.frame.size.width * scale;
        let h = self.frame.size.height * scale;
        tracing::info!(wid = self.wid, w, h, "draw: filling solid red");

        let full = CGRect::new(0.0, 0.0, w, h);

        unsafe {
            CGContextClearRect(ctx, full);
            CGContextSetRGBFillColor(ctx, 1.0, 0.0, 0.0, 1.0);
            let path = CGPathCreateMutable();
            CGPathAddRect(path, ptr::null(), full);
            CGContextAddPath(ctx, path as CGPathRef);
            CGContextFillPath(ctx);
            CGPathRelease(path as CGPathRef);
            CGContextFlush(ctx);
            SLSFlushWindowContentRegion(self.cid, self.wid, ptr::null());
            CGContextRelease(ctx);
        }

        self.needs_redraw = false;
    }

    pub fn hide(&self) {
        if self.wid == 0 {
            return;
        }
        unsafe {
            let transaction = SLSTransactionCreate(self.cid);
            if !transaction.is_null() {
                SLSTransactionOrderWindow(transaction, self.wid, 0, self.target_wid);
                SLSTransactionCommit(transaction, 0);
                CFRelease(transaction);
            }
        }
    }

    pub fn unhide(&self, border_order: i32) {
        if self.wid == 0 {
            return;
        }
        unsafe {
            let transaction = SLSTransactionCreate(self.cid);
            if !transaction.is_null() {
                SLSTransactionOrderWindow(transaction, self.wid, border_order, self.target_wid);
                SLSTransactionCommit(transaction, 0);
                CFRelease(transaction);
            }
        }
    }
}

impl Drop for BorderWindow {
    fn drop(&mut self) {
        if !self.context.is_null() {
            unsafe { CGContextRelease(self.context) };
        }
        if self.wid != 0 {
            unsafe { SLSReleaseWindow(self.cid, self.wid) };
        }
    }
}

/// Disable shadow on an overlay window via SLSWindowSetShadowProperties.
fn disable_shadow(wid: u32) {
    unsafe {
        let density: i64 = 0;
        let density_cf = CFNumberCreate(
            ptr::null(),
            kCFNumberCFIndexType,
            &density as *const _ as *const _,
        );

        let key_bytes = b"com.apple.WindowShadowDensity\0";
        let key = CFStringCreateWithCString(ptr::null(), key_bytes.as_ptr(), 0x0800_0100);

        let keys = [key as CFTypeRef];
        let values = [density_cf as CFTypeRef];
        let dict = CFDictionaryCreate(
            ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            1,
            &kCFTypeDictionaryKeyCallBacks as *const _ as *const _,
            &kCFTypeDictionaryValueCallBacks as *const _ as *const _,
        );

        SLSWindowSetShadowProperties(wid, dict);

        CFRelease(dict);
        CFRelease(density_cf);
        CFRelease(key as CFTypeRef);
    }
}

unsafe extern "C" {
    fn CFStringCreateWithCString(
        alloc: CFAllocatorRef,
        c_str: *const u8,
        encoding: u32,
    ) -> CFStringRef;
}
