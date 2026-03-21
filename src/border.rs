//! Border overlay window: create, draw, reposition, destroy.
//!
//! Each BorderWindow is a transparent SLS overlay that draws a colored
//! rounded-rect border around a target application window.
//! Uses a fresh SLSNewConnection per border (like JankyBorders).

use crate::config::Color;
use crate::skylight::*;
use std::ptr;

pub struct BorderWindow {
    pub cid: CGSConnectionID,
    pub wid: u32,
    pub target_wid: u32,
    pub frame: CGRect,
    pub target_bounds: CGRect,
    pub origin: CGPoint,
    pub focused: bool,
    pub needs_redraw: bool,
    hidpi: bool,
}

// SAFETY: BorderWindow is only accessed from a single thread (the event loop thread).
unsafe impl Send for BorderWindow {}

impl BorderWindow {
    /// Create a new border overlay for `target_wid`.
    pub fn new(_main_cid: CGSConnectionID, target_wid: u32, hidpi: bool, border_width: f64) -> Option<Self> {
        // Fresh SLS connection per border (like JankyBorders border_create).
        // Required because SLSCopyManagedDisplaySpaces poisons the main
        // connection's ability to create visible windows on macOS Tahoe.
        let mut border_cid: CGSConnectionID = 0;
        unsafe { SLSNewConnection(0, &mut border_cid) };
        if border_cid == 0 {
            return None;
        }

        // Get target window bounds
        let mut target_bounds = CGRect::default();
        let err = unsafe { SLSGetWindowBounds(border_cid, target_wid, &mut target_bounds) };
        if err != kCGErrorSuccess {
            unsafe { SLSReleaseConnection(border_cid) };
            return None;
        }

        // Calculate overlay frame (extends border_width beyond target on each side)
        let bw = border_width;
        let overlay_w = target_bounds.size.width + 2.0 * bw;
        let overlay_h = target_bounds.size.height + 2.0 * bw;
        let overlay_x = target_bounds.origin.x - bw;
        let overlay_y = target_bounds.origin.y - bw;
        let frame = CGRect::new(0.0, 0.0, overlay_w, overlay_h);
        let origin = CGPoint { x: overlay_x, y: overlay_y };

        // Create overlay window at correct position and size
        let mut region: CFTypeRef = ptr::null();
        unsafe { CGSNewRegionWithRect(&frame, &mut region) };
        if region.is_null() {
            unsafe { SLSReleaseConnection(border_cid) };
            return None;
        }

        let mut wid: u32 = 0;
        unsafe {
            SLSNewWindow(border_cid, 2, overlay_x as f32, overlay_y as f32, region, &mut wid);
            CFRelease(region);
        }
        if wid == 0 {
            unsafe { SLSReleaseConnection(border_cid) };
            return None;
        }

        unsafe {
            SLSSetWindowResolution(border_cid, wid, if hidpi { 2.0 } else { 1.0 });
            SLSSetWindowOpacity(border_cid, wid, false);

            // Tags: click-through (bit 1) + sticky/all-spaces (bit 9)
            let set_tags: u64 = (1 << 1) | (1 << 9);
            SLSSetWindowTags(border_cid, wid, &set_tags, 64);

            // Disable shadow
            disable_shadow(wid);
        }

        Some(Self {
            cid: border_cid,
            wid,
            target_wid,
            frame,
            target_bounds,
            origin,
            focused: false,
            needs_redraw: true,
            hidpi,
        })
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

        let offset = border_width;
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

        // Reshape if size changed
        let size_changed = (frame.size.width - self.frame.size.width).abs() > 0.5
            || (frame.size.height - self.frame.size.height).abs() > 0.5;

        if size_changed {
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

        if self.needs_redraw {
            let color = if self.focused { active_color } else { inactive_color };
            self.draw(color, border_width, radius);
        }

        // Position and order relative to target
        unsafe {
            SLSMoveWindow(self.cid, self.wid, &self.origin);

            let mut level: i64 = 0;
            SLSGetWindowLevel(self.cid, self.target_wid, &mut level);
            SLSSetWindowLevel(self.cid, self.wid, level as i32);

            SLSOrderWindow(self.cid, self.wid, border_order, self.target_wid);
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

        let offset = border_width;
        self.origin = CGPoint {
            x: self.target_bounds.origin.x - offset,
            y: self.target_bounds.origin.y - offset,
        };

        unsafe {
            SLSMoveWindow(self.cid, self.wid, &self.origin);
        }
    }

    /// Draw the border: fill overlay with color, punch out transparent center.
    fn draw(&mut self, color: &Color, border_width: f64, radius: f64) {
        let ctx = unsafe { SLWindowContextCreate(self.cid, self.wid, ptr::null()) };
        if ctx.is_null() {
            self.needs_redraw = false;
            return;
        }

        let scale = if self.hidpi { 2.0 } else { 1.0 };
        let w = self.frame.size.width * scale;
        let h = self.frame.size.height * scale;
        let bw = border_width * scale;

        if w < bw * 2.0 || h < bw * 2.0 {
            unsafe { CGContextRelease(ctx) };
            self.needs_redraw = false;
            return;
        }

        let full = CGRect::new(0.0, 0.0, w, h);

        // Stroke rect: centered in the border ring so the stroke
        // straddles the edge between overlay and window area
        let stroke_rect = CGRect::new(bw / 2.0, bw / 2.0, w - bw, h - bw);
        let max_r = (stroke_rect.size.width.min(stroke_rect.size.height) / 2.0).max(0.0);
        let r = (radius * scale).min(max_r);

        unsafe {
            CGContextClearRect(ctx, full);

            CGContextSetRGBStrokeColor(ctx, color.r, color.g, color.b, color.a);
            CGContextSetLineWidth(ctx, bw);

            let path = CGPathCreateWithRoundedRect(stroke_rect, r, r, ptr::null());
            if !path.is_null() {
                CGContextAddPath(ctx, path);
                CGContextStrokePath(ctx);
                CGPathRelease(path);
            }

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
            SLSOrderWindow(self.cid, self.wid, 0, 0);
        }
    }

    pub fn unhide(&self, border_order: i32) {
        if self.wid == 0 {
            return;
        }
        unsafe {
            SLSOrderWindow(self.cid, self.wid, border_order, self.target_wid);
        }
    }
}

impl Drop for BorderWindow {
    fn drop(&mut self) {
        if self.wid != 0 {
            unsafe { SLSReleaseWindow(self.cid, self.wid) };
        }
        if self.cid != 0 {
            unsafe { SLSReleaseConnection(self.cid) };
        }
    }
}

/// Disable shadow via SLSWindowSetShadowProperties.
fn disable_shadow(wid: u32) {
    unsafe {
        let density: i64 = 0;
        let density_cf = CFNumberCreate(
            ptr::null(),
            kCFNumberCFIndexType,
            &density as *const _ as *const _,
        );
        let key = CFStringCreateWithCString(
            ptr::null(),
            b"com.apple.WindowShadowDensity\0".as_ptr(),
            kCFStringEncodingUTF8,
        );
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
