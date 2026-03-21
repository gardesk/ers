//! ers — window border renderer
//!
//! Sprint 1: static border on a single window by ID.

mod skylight;

use skylight::*;
use std::ptr;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let target_wid: u32 = match args.get(1) {
        Some(s) => s.parse().expect("usage: ers <window-id>"),
        None => {
            eprintln!("usage: ers <window-id>");
            eprintln!("  draws a border around the specified window");
            std::process::exit(1);
        }
    };

    let border_width: f64 = args
        .get(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(4.0);

    let cid = unsafe { SLSMainConnectionID() };

    // Task 1.2: create overlay with solid fill (proven working)
    let overlay = create_overlay(cid, target_wid, border_width);
    match overlay {
        Some((bcid, wid)) => {
            eprintln!("border wid={wid} for target={target_wid} (cid={bcid})");
        }
        None => {
            eprintln!("failed to create border for wid {target_wid}");
            std::process::exit(1);
        }
    }

    // Keep alive
    unsafe { CFRunLoopRun() };
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

        // Window properties
        SLSSetWindowResolution(bcid, wid, 2.0); // HiDPI
        SLSSetWindowOpacity(bcid, wid, false); // transparent
        SLSSetWindowLevel(bcid, wid, 1); // above normal
        SLSOrderWindow(bcid, wid, 1, 0); // order in

        // Draw solid fill (proven to show on all 4 sides)
        let ctx = SLWindowContextCreate(bcid, wid, ptr::null());
        if ctx.is_null() {
            SLSReleaseWindow(bcid, wid);
            SLSReleaseConnection(bcid);
            return None;
        }

        let scale = 2.0;
        let full = CGRect::new(0.0, 0.0, ow * scale, oh * scale);
        CGContextClearRect(ctx, full);
        CGContextSetRGBFillColor(ctx, 0.32, 0.58, 0.89, 1.0);
        let path = CGPathCreateMutable();
        CGPathAddRect(path, ptr::null(), full);
        CGContextAddPath(ctx, path as CGPathRef);
        CGContextFillPath(ctx);
        CGPathRelease(path as CGPathRef);
        CGContextFlush(ctx);
        SLSFlushWindowContentRegion(bcid, wid, ptr::null());
        CGContextRelease(ctx);

        Some((bcid, wid))
    }
}
