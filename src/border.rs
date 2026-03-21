//! Border overlay window: create, draw, reposition, destroy.
//!
//! Each BorderWindow is a transparent SLS overlay that draws a colored
//! rounded-rect stroke around a target application window.

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
    pub fn new(cid: CGSConnectionID, target_wid: u32, hidpi: bool) -> Option<Self> {
        // Get target window bounds from compositor
        let mut target_bounds = CGRect::default();
        let err = unsafe { SLSGetWindowBounds(cid, target_wid, &mut target_bounds) };
        if err != kCGErrorSuccess {
            tracing::warn!("SLSGetWindowBounds failed for wid {target_wid}: {err}");
            return None;
        }

        let mut border = Self {
            cid,
            wid: 0,
            target_wid,
            frame: CGRect::default(),
            target_bounds,
            origin: CGPoint::default(),
            focused: false,
            needs_redraw: true,
            context: ptr::null_mut(),
            hidpi,
        };

        border.create_window()?;
        Some(border)
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

    /// Create the SLS overlay window.
    fn create_window(&mut self) -> Option<()> {
        let cid = self.cid;

        // Use a 1x1 initial frame — we'll reshape before drawing
        let init_rect = CGRect::new(0.0, 0.0, 1.0, 1.0);
        let mut region: CFTypeRef = ptr::null();
        unsafe { CGSNewRegionWithRect(&init_rect, &mut region) };
        if region.is_null() {
            return None;
        }

        let mut wid: u32 = 0;
        let err = unsafe { SLSNewWindow(cid, kCGBackingStoreBuffered, -9999.0, -9999.0, region, &mut wid) };
        unsafe { CFRelease(region) };

        if err != kCGErrorSuccess || wid == 0 {
            tracing::warn!("SLSNewWindow failed: {err}");
            return None;
        }

        self.wid = wid;

        unsafe {
            // HiDPI
            SLSSetWindowResolution(cid, wid, if self.hidpi { 2.0 } else { 1.0 });

            // Tags: click-through (bit 1) + sticky (bit 9)
            let set_tags: u64 = (1 << 1) | (1 << 9);
            let clear_tags: u64 = 0;
            SLSSetWindowTags(cid, wid, &set_tags, 0x40);
            SLSClearWindowTags(cid, wid, &clear_tags, 0x40);

            // Non-opaque (required for transparency)
            SLSSetWindowOpacity(cid, wid, false);

            // Disable shadow
            disable_shadow(wid);
        }

        Some(())
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

        // Refresh target bounds
        let mut new_bounds = CGRect::default();
        let err = unsafe { SLSGetWindowBounds(self.cid, self.target_wid, &mut new_bounds) };
        if err != kCGErrorSuccess {
            return;
        }
        self.target_bounds = new_bounds;

        // Check if target is visible
        let mut shown = false;
        unsafe { SLSWindowIsOrderedIn(self.cid, self.target_wid, &mut shown) };
        if !shown {
            self.hide();
            return;
        }

        let (frame, origin) = self.calculate_frame(border_width);
        self.origin = origin;

        // Reshape if size changed
        let size_changed = (frame.size.width - self.frame.size.width).abs() > 0.5
            || (frame.size.height - self.frame.size.height).abs() > 0.5;

        if size_changed || self.frame.size.width < 1.0 {
            unsafe {
                SLSDisableUpdate(self.cid);

                let mut region: CFTypeRef = ptr::null();
                CGSNewRegionWithRect(&frame, &mut region);
                if !region.is_null() {
                    SLSSetWindowShape(self.cid, self.wid, -9999.0, -9999.0, region);
                    CFRelease(region);
                }

                SLSReenableUpdate(self.cid);
            }
            self.frame = frame;
            self.needs_redraw = true;

            // Recreate context for new size
            if !self.context.is_null() {
                unsafe { CGContextRelease(self.context) };
                self.context = ptr::null_mut();
            }
            self.context = unsafe { SLWindowContextCreate(self.cid, self.wid, ptr::null()) };
            if !self.context.is_null() {
                unsafe { CGContextSetInterpolationQuality(self.context, 0) };
            }
        }

        // Draw
        if self.needs_redraw {
            let color = if self.focused { active_color } else { inactive_color };
            self.draw(color, border_width, radius);
        }

        // Position and order via transaction
        self.reposition(border_order);
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
            let transaction = SLSTransactionCreate(self.cid);
            if !transaction.is_null() {
                SLSTransactionMoveWindowWithGroup(transaction, self.wid, self.origin);
                SLSTransactionCommit(transaction, 0);
                CFRelease(transaction);
            }
        }
    }

    /// Draw the border stroke into the overlay context.
    fn draw(&mut self, color: &Color, border_width: f64, radius: f64) {
        if self.context.is_null() {
            self.context = unsafe { SLWindowContextCreate(self.cid, self.wid, ptr::null()) };
            if self.context.is_null() {
                return;
            }
            unsafe { CGContextSetInterpolationQuality(self.context, 0) };
        }

        let scale = if self.hidpi { 2.0 } else { 1.0 };
        let w = self.frame.size.width * scale;
        let h = self.frame.size.height * scale;

        let full = CGRect::new(0.0, 0.0, w, h);

        // The drawing bounds represent where the target window sits within our overlay
        let offset = (border_width + BORDER_PADDING) * scale;
        let drawing_rect = CGRect::new(offset, offset, w - 2.0 * offset, h - 2.0 * offset);

        let ctx = self.context;
        unsafe {
            CGContextSaveGState(ctx);

            // Clear to transparent
            CGContextClearRect(ctx, full);

            // Clip: draw only between the outer rounded rect and inner rounded rect
            // This creates a clean border stroke without fill artifacts
            let inner_radius = radius * scale;
            let inner_clip = CGPathCreateMutable();
            let inset_rect = drawing_rect.inset(1.0, 1.0);
            CGPathAddRoundedRect(inner_clip, ptr::null(), inset_rect, inner_radius, inner_radius);

            // Clip between frame and inner path (even-odd rule)
            let clip_path = CGPathCreateMutable();
            CGPathAddRect(clip_path, ptr::null(), full);
            CGPathAddPath(clip_path, ptr::null(), inner_clip as CGPathRef);
            CGContextAddPath(ctx, clip_path as CGPathRef);
            CGContextEOClip(ctx);
            CGPathRelease(inner_clip as CGPathRef);
            CGPathRelease(clip_path as CGPathRef);

            // Draw the rounded rect border
            CGContextSetRGBFillColor(ctx, color.r, color.g, color.b, color.a);
            CGContextSetRGBStrokeColor(ctx, color.r, color.g, color.b, color.a);
            CGContextSetLineWidth(ctx, border_width * scale);

            let stroke_path = CGPathCreateWithRoundedRect(
                drawing_rect,
                inner_radius,
                inner_radius,
                ptr::null(),
            );
            if !stroke_path.is_null() {
                CGContextAddPath(ctx, stroke_path);
                CGContextFillPath(ctx);
                CGPathRelease(stroke_path);
            }

            CGContextFlush(ctx);
            CGContextRestoreGState(ctx);
            SLSFlushWindowContentRegion(self.cid, self.wid, ptr::null());
        }

        self.needs_redraw = false;
    }

    /// Reposition and re-order the overlay relative to target via transaction.
    fn reposition(&self, border_order: i32) {
        unsafe {
            let transaction = SLSTransactionCreate(self.cid);
            if transaction.is_null() {
                return;
            }

            SLSTransactionMoveWindowWithGroup(transaction, self.wid, self.origin);

            // Match target window level
            let mut level: i64 = 0;
            SLSGetWindowLevel(self.cid, self.target_wid, &mut level);
            SLSTransactionSetWindowLevel(transaction, self.wid, level as i32);

            // Set transform to correct for the move-with-group offset
            let transform = CGAffineTransform {
                tx: -self.origin.x,
                ty: -self.origin.y,
                ..CGAffineTransform::IDENTITY
            };
            SLSTransactionSetWindowTransform(transaction, self.wid, 0, 0, transform);

            // Order relative to target
            SLSTransactionOrderWindow(transaction, self.wid, border_order, self.target_wid);
            SLSTransactionCommit(transaction, 0);
            CFRelease(transaction);
        }

        // Ensure sticky tags are set
        unsafe {
            let set_tags: u64 = (1 << 1) | (1 << 9);
            SLSSetWindowTags(self.cid, self.wid, &set_tags, 0x40);
        }
    }

    pub fn hide(&self) {
        if self.wid == 0 {
            return;
        }
        unsafe {
            let transaction = SLSTransactionCreate(self.cid);
            if !transaction.is_null() {
                // Order out (mode 0 = remove from window list)
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

        // "com.apple.WindowShadowDensity" as CFString
        // We build it via CoreFoundation to avoid linking Foundation
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
