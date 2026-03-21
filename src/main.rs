//! ers — window border renderer

mod skylight;

use skylight::*;
use std::ptr;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.get(1).is_some_and(|s| s == "--list") {
        list_windows();
        return;
    }

    let border_width: f64 = args
        .iter()
        .position(|s| s == "--width" || s == "-w")
        .and_then(|i| args.get(i + 1)?.parse().ok())
        .unwrap_or(4.0);

    let cid = unsafe { SLSMainConnectionID() };
    let own_pid = unsafe {
        let mut pid: i32 = 0;
        pid_for_task(mach_task_self(), &mut pid);
        pid
    };

    // Single window mode or all-windows mode
    if let Some(target) = args.get(1).and_then(|s| s.parse::<u32>().ok()) {
        match create_overlay(cid, target, border_width) {
            Some((_bcid, wid)) => eprintln!("border wid={wid} for target={target}"),
            None => {
                eprintln!("failed to create border for wid {target}");
                std::process::exit(1);
            }
        }
    } else {
        // Discover all on-screen windows and create borders
        let wids = discover_windows(cid, own_pid);
        eprintln!("{} windows discovered", wids.len());

        let mut borders = Vec::new();
        for &target in &wids {
            if let Some(overlay) = create_overlay(cid, target, border_width) {
                eprintln!("  border for wid={target} -> overlay={}", overlay.1);
                borders.push(overlay);
            } else {
                eprintln!("  FAILED wid={target}");
            }
        }
        eprintln!("{} borders created", borders.len());

        // Leak to keep alive (no Drop cleanup until exit)
        std::mem::forget(borders);
    }

    unsafe { CFRunLoopRun() };
}

/// Discover on-screen application windows via CGWindowListCopyWindowInfo.
/// Returns collected wids after releasing all CF objects.
fn discover_windows(cid: CGSConnectionID, own_pid: i32) -> Vec<u32> {
    unsafe {
        let list = CGWindowListCopyWindowInfo(kCGWindowListOptionOnScreenOnly, kCGNullWindowID);
        if list.is_null() { return vec![]; }

        let count = CFArrayGetCount(list);
        let wid_key = CFStringCreateWithCString(ptr::null(), b"kCGWindowNumber\0".as_ptr(), kCFStringEncodingUTF8);
        let pid_key = CFStringCreateWithCString(ptr::null(), b"kCGWindowOwnerPID\0".as_ptr(), kCFStringEncodingUTF8);
        let layer_key = CFStringCreateWithCString(ptr::null(), b"kCGWindowLayer\0".as_ptr(), kCFStringEncodingUTF8);

        let mut wids = Vec::new();
        for i in 0..count {
            let dict = CFArrayGetValueAtIndex(list, i);
            if dict.is_null() { continue; }

            let mut v: CFTypeRef = ptr::null();

            let mut wid: u32 = 0;
            if CFDictionaryGetValueIfPresent(dict, wid_key as CFTypeRef, &mut v) {
                CFNumberGetValue(v, kCFNumberSInt32Type, &mut wid as *mut _ as *mut _);
            }
            if wid == 0 { continue; }

            let mut pid: i32 = 0;
            if CFDictionaryGetValueIfPresent(dict, pid_key as CFTypeRef, &mut v) {
                CFNumberGetValue(v, kCFNumberSInt32Type, &mut pid as *mut _ as *mut _);
            }
            if pid == own_pid { continue; }

            let mut layer: i32 = -1;
            if CFDictionaryGetValueIfPresent(dict, layer_key as CFTypeRef, &mut v) {
                CFNumberGetValue(v, kCFNumberSInt32Type, &mut layer as *mut _ as *mut _);
            }
            if layer != 0 { continue; }

            wids.push(wid);
        }

        CFRelease(wid_key as CFTypeRef);
        CFRelease(pid_key as CFTypeRef);
        CFRelease(layer_key as CFTypeRef);
        CFRelease(list);
        wids
    }
}

/// Create a border overlay around `target_wid`.
/// Returns (border_cid, overlay_wid) on success.
fn create_overlay(
    _main_cid: CGSConnectionID,
    target_wid: u32,
    border_width: f64,
) -> Option<(CGSConnectionID, u32)> {
    unsafe {
        // Fresh connection (required — main cid gets poisoned by space queries)
        let mut bcid: CGSConnectionID = 0;
        SLSNewConnection(0, &mut bcid);
        if bcid == 0 {
            return None;
        }

        // Get target bounds
        let mut bounds = CGRect::default();
        if SLSGetWindowBounds(bcid, target_wid, &mut bounds) != kCGErrorSuccess {
            SLSReleaseConnection(bcid);
            return None;
        }

        let bw = border_width;
        let ow = bounds.size.width + 2.0 * bw;
        let oh = bounds.size.height + 2.0 * bw;
        let ox = bounds.origin.x - bw;
        let oy = bounds.origin.y - bw;

        eprintln!(
            "target: ({:.0},{:.0}) {:.0}x{:.0}  overlay: ({ox:.0},{oy:.0}) {ow:.0}x{oh:.0}",
            bounds.origin.x, bounds.origin.y, bounds.size.width, bounds.size.height
        );

        // Create overlay at correct position and size
        let frame = CGRect::new(0.0, 0.0, ow, oh);
        let mut region: CFTypeRef = ptr::null();
        CGSNewRegionWithRect(&frame, &mut region);
        if region.is_null() {
            SLSReleaseConnection(bcid);
            return None;
        }

        let mut wid: u32 = 0;
        SLSNewWindow(bcid, 2, ox as f32, oy as f32, region, &mut wid);
        CFRelease(region);
        if wid == 0 {
            SLSReleaseConnection(bcid);
            return None;
        }

        SLSSetWindowResolution(bcid, wid, 2.0);
        SLSSetWindowOpacity(bcid, wid, false);
        SLSSetWindowLevel(bcid, wid, 1);
        SLSOrderWindow(bcid, wid, 1, 0);

        // Click-through only (no sticky — stay on creation space)
        let tags: u64 = 1 << 1;
        SLSSetWindowTags(bcid, wid, &tags, 64);

        // Disable shadow
        disable_shadow(wid);

        // Draw border: 4 filled rectangles (no clipping tricks)
        let ctx = SLWindowContextCreate(bcid, wid, ptr::null());
        if ctx.is_null() {
            SLSReleaseWindow(bcid, wid);
            SLSReleaseConnection(bcid);
            return None;
        }

        // Context is in POINTS, not pixels (SLSSetWindowResolution handles HiDPI)
        let w = ow;
        let h = oh;
        let b = bw;
        let full = CGRect::new(0.0, 0.0, w, h);
        CGContextClearRect(ctx, full);

        // Rounded border via stroke path (point coordinates)
        let radius = 10.0_f64;
        let stroke_rect = CGRect::new(b / 2.0, b / 2.0, w - b, h - b);
        let max_r = (stroke_rect.size.width.min(stroke_rect.size.height) / 2.0).max(0.0);
        let r = radius.min(max_r);

        CGContextSetRGBStrokeColor(ctx, 0.32, 0.58, 0.89, 1.0);
        CGContextSetLineWidth(ctx, b);
        let path = CGPathCreateWithRoundedRect(stroke_rect, r, r, ptr::null());
        if !path.is_null() {
            CGContextAddPath(ctx, path);
            CGContextStrokePath(ctx);
            CGPathRelease(path);
        }

        CGContextFlush(ctx);
        SLSFlushWindowContentRegion(bcid, wid, ptr::null());
        CGContextRelease(ctx);

        Some((bcid, wid))
    }
}

/// List on-screen windows with positions.
fn list_windows() {
    let cid = unsafe { SLSMainConnectionID() };
    unsafe {
        let list = CGWindowListCopyWindowInfo(kCGWindowListOptionOnScreenOnly, kCGNullWindowID);
        if list.is_null() { return; }
        let count = CFArrayGetCount(list);
        let wid_key = CFStringCreateWithCString(ptr::null(), b"kCGWindowNumber\0".as_ptr(), kCFStringEncodingUTF8);
        let pid_key = CFStringCreateWithCString(ptr::null(), b"kCGWindowOwnerPID\0".as_ptr(), kCFStringEncodingUTF8);
        let layer_key = CFStringCreateWithCString(ptr::null(), b"kCGWindowLayer\0".as_ptr(), kCFStringEncodingUTF8);
        let name_key = CFStringCreateWithCString(ptr::null(), b"kCGWindowOwnerName\0".as_ptr(), kCFStringEncodingUTF8);

        eprintln!("{:>6}  {:>5}  {:>8}  {:>8}  {:>6}  {:>6}", "wid", "layer", "x", "y", "w", "h");
        for i in 0..count {
            let dict = CFArrayGetValueAtIndex(list, i);
            if dict.is_null() { continue; }

            let mut v: CFTypeRef = ptr::null();
            let mut wid: u32 = 0;
            let mut layer: i32 = -1;
            if CFDictionaryGetValueIfPresent(dict, wid_key as CFTypeRef, &mut v) {
                CFNumberGetValue(v, kCFNumberSInt32Type, &mut wid as *mut _ as *mut _);
            }
            if CFDictionaryGetValueIfPresent(dict, layer_key as CFTypeRef, &mut v) {
                CFNumberGetValue(v, kCFNumberSInt32Type, &mut layer as *mut _ as *mut _);
            }
            if layer != 0 || wid == 0 { continue; }

            let mut bounds = CGRect::default();
            SLSGetWindowBounds(cid, wid, &mut bounds);

            eprintln!("{wid:>6}  {layer:>5}  {:>8.0}  {:>8.0}  {:>6.0}  {:>6.0}",
                bounds.origin.x, bounds.origin.y, bounds.size.width, bounds.size.height);
        }
        CFRelease(wid_key as CFTypeRef);
        CFRelease(pid_key as CFTypeRef);
        CFRelease(layer_key as CFTypeRef);
        CFRelease(name_key as CFTypeRef);
        CFRelease(list);
    }
}

unsafe fn disable_shadow(wid: u32) {
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
