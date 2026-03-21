//! ers — window border renderer for tarmac
//!
//! Standalone process that draws colored border overlays around macOS windows
//! using SkyLight private framework APIs. Tracks window events via
//! SLSRegisterNotifyProc and renders via CGContext into transparent SLS windows.

mod border;
mod config;
mod events;
mod skylight;
mod windows;

use config::Config;
use events::WmEvent;
use skylight::*;
use windows::WindowTracker;

use std::sync::mpsc;

fn main() {
    eprintln!("[ers] pid={}", std::process::id());

    // Smoke test bypass — must be before Config::from_args()
    if std::env::args().any(|a| a == "--smoke") {
        run_smoke_test();
        return;
    }

    // Smoke test 2: border around a real window, all inline
    if let Some(pos) = std::env::args().position(|a| a == "--smoke2") {
        let wid_str = std::env::args().nth(pos + 1).expect("usage: --smoke2 <wid>");
        let target_wid: u32 = wid_str.parse().expect("wid must be a number");
        run_smoke2(target_wid);
        return;
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ers=info".parse().unwrap()),
        )
        .init();

    let config = Config::from_args();
    tracing::info!(
        width = config.border_width,
        color = format!(
            "({:.2},{:.2},{:.2},{:.2})",
            config.active_color.r,
            config.active_color.g,
            config.active_color.b,
            config.active_color.a
        ),
        "starting ers"
    );

    let cid = unsafe { SLSMainConnectionID() };
    if cid == 0 {
        eprintln!("failed to get SLS connection — not running in a WindowServer session?");
        std::process::exit(1);
    }

    let (tx, rx) = mpsc::channel::<WmEvent>();

    let _ = tx;

    // Full discovery + border creation with fresh SLS connections
    let mut tracker = windows::WindowTracker::new(cid);
    tracker.add_existing_windows(&config);
    eprintln!("[main] {} borders created", tracker.border_count());
    std::mem::forget(tracker);

    // DISABLED: keep tracker on main thread, no events
    // let config_clone = config.clone();
    // std::thread::spawn(move || {
    //     event_loop(rx, &mut tracker, &config_clone);
    // });
    let _ = rx;

    // Run the CFRunLoop on the main thread — required for SLS events
    tracing::info!("entering CFRunLoop");
    unsafe { CFRunLoopRun() };
}

/// Process events from the channel.
fn event_loop(rx: mpsc::Receiver<WmEvent>, tracker: &mut WindowTracker, config: &Config) {
    while let Ok(event) = rx.recv() {
        match event {
            WmEvent::WindowMove(wid) => {
                tracker.move_window(wid, config);
            }
            WmEvent::WindowResize(wid)
            | WmEvent::WindowReorder(wid)
            | WmEvent::WindowLevel(wid) => {
                tracker.update_window(wid, config);
            }
            WmEvent::WindowClose(wid) => {
                tracker.destroy_border(wid);
            }
            WmEvent::WindowCreate { wid, .. } => {
                if tracker.create_border(wid, config) {
                    tracker.determine_focus(config);
                }
            }
            WmEvent::WindowDestroy { wid, .. } => {
                tracker.destroy_border(wid);
                tracker.determine_focus(config);
            }
            WmEvent::WindowHide(wid) => {
                tracker.hide_window(wid);
            }
            WmEvent::WindowUnhide(wid) => {
                tracker.unhide_window(wid, config);
            }
            WmEvent::SpaceChange => {
                tracker.update_all(config);
            }
            WmEvent::FrontAppChange => {
                tracker.determine_focus(config);
            }
            WmEvent::FocusCheck => {
                tracker.determine_focus(config);
            }
        }
    }
}

/// Set up the SLS event port on the current thread's CFRunLoop.
fn setup_event_port(cid: CGSConnectionID) {
    unsafe {
        let mut port: u32 = 0;
        let err = SLSGetEventPort(cid, &mut port);
        if err != kCGErrorSuccess {
            tracing::warn!("SLSGetEventPort failed: {err}");
            return;
        }

        let cf_mach_port = CFMachPortCreateWithPort(
            std::ptr::null(),
            port,
            event_port_callback as *const _,
            std::ptr::null(),
            false,
        );

        if cf_mach_port.is_null() {
            tracing::warn!("CFMachPortCreateWithPort failed");
            return;
        }

        _CFMachPortSetOptions(cf_mach_port, 0x40);

        let source = CFMachPortCreateRunLoopSource(std::ptr::null(), cf_mach_port, 0);
        if !source.is_null() {
            CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode);
            CFRelease(source);
        }
        CFRelease(cf_mach_port);
    }
}

/// Callback for the SLS event mach port — drains pending events.
unsafe extern "C" fn event_port_callback(
    _port: CFMachPortRef,
    _message: *mut std::ffi::c_void,
    _size: i64,
    _context: *mut std::ffi::c_void,
) {
    unsafe {
        let cid = SLSMainConnectionID();
        let mut event = SLEventCreateNextEvent(cid);
        while !event.is_null() {
            CFRelease(event as CFTypeRef);
            event = SLEventCreateNextEvent(cid);
        }
    }
}

/// Minimal smoke test: create a bright red 200x200 square at (100,100).
/// If this isn't visible, SLS window creation is fundamentally broken.
fn run_smoke_test() {
    let cid = unsafe { SLSMainConnectionID() };
    eprintln!("[smoke] cid={cid}");
    unsafe {
        // --- Test A: direct create (known working) ---
        // Test A: main connection (known working)
        eprintln!("[smoke A] main cid={cid}, 200x200 RED at (100,100)");
        let wid_a = {
            let rect = CGRect::new(0.0, 0.0, 200.0, 200.0);
            let mut region: CFTypeRef = std::ptr::null();
            CGSNewRegionWithRect(&rect, &mut region);
            let mut wid: u32 = 0;
            SLSNewWindow(cid, 2, 100.0, 100.0, region, &mut wid);
            CFRelease(region);
            SLSSetWindowResolution(cid, wid, 2.0);
            SLSSetWindowOpacity(cid, wid, false);
            SLSSetWindowLevel(cid, wid, 25);
            SLSOrderWindow(cid, wid, 1, 0);
            let ctx = SLWindowContextCreate(cid, wid, std::ptr::null());
            let full = CGRect::new(0.0, 0.0, 400.0, 400.0);
            CGContextClearRect(ctx, full);
            CGContextSetRGBFillColor(ctx, 1.0, 0.0, 0.0, 1.0);
            let path = CGPathCreateMutable();
            CGPathAddRect(path, std::ptr::null(), full);
            CGContextAddPath(ctx, path as CGPathRef);
            CGContextFillPath(ctx);
            CGPathRelease(path as CGPathRef);
            CGContextFlush(ctx);
            SLSFlushWindowContentRegion(cid, wid, std::ptr::null());
            eprintln!("[smoke A] wid={wid} — RED square at (100,100)");
            wid
        };

        // --- Test B: create 1x1 at -9999, reshape, move (border lifecycle) ---
        eprintln!("[smoke B] create 1x1 at -9999, reshape to 200x200, move to (400,100)");
        {
            let init_rect = CGRect::new(0.0, 0.0, 1.0, 1.0);
            let mut region: CFTypeRef = std::ptr::null();
            CGSNewRegionWithRect(&init_rect, &mut region);
            let mut wid: u32 = 0;
            SLSNewWindow(cid, 2, -9999.0, -9999.0, region, &mut wid);
            CFRelease(region);
            SLSSetWindowResolution(cid, wid, 2.0);
            SLSSetWindowOpacity(cid, wid, false);
            eprintln!("[smoke B] wid={wid}, created 1x1 offscreen");

            // Reshape to 200x200
            let new_rect = CGRect::new(0.0, 0.0, 200.0, 200.0);
            let mut new_region: CFTypeRef = std::ptr::null();
            CGSNewRegionWithRect(&new_rect, &mut new_region);
            SLSSetWindowShape(cid, wid, -9999.0, -9999.0, new_region);
            CFRelease(new_region);
            eprintln!("[smoke B] reshaped to 200x200");

            // Get context and draw green
            let ctx = SLWindowContextCreate(cid, wid, std::ptr::null());
            eprintln!("[smoke B] ctx null={}", ctx.is_null());
            if !ctx.is_null() {
                let full = CGRect::new(0.0, 0.0, 400.0, 400.0);
                CGContextClearRect(ctx, full);
                CGContextSetRGBFillColor(ctx, 0.0, 1.0, 0.0, 1.0); // GREEN
                let path = CGPathCreateMutable();
                CGPathAddRect(path, std::ptr::null(), full);
                CGContextAddPath(ctx, path as CGPathRef);
                CGContextFillPath(ctx);
                CGPathRelease(path as CGPathRef);
                CGContextFlush(ctx);
                SLSFlushWindowContentRegion(cid, wid, std::ptr::null());
            }

            // Move to visible position
            let origin = CGPoint { x: 400.0, y: 100.0 };
            SLSMoveWindow(cid, wid, &origin);
            SLSSetWindowLevel(cid, wid, 25);
            SLSOrderWindow(cid, wid, 1, 0);
            eprintln!("[smoke B] moved to (400,100) — GREEN square should appear next to red");
        }

        // POISON TEST: call SLSCopyManagedDisplaySpaces on main cid first
        {
            let ds = SLSCopyManagedDisplaySpaces(cid);
            if !ds.is_null() {
                eprintln!("[smoke] POISON: called SLSCopyManagedDisplaySpaces, releasing");
                CFRelease(ds);
            }
        }

        // Test C: SLSNewConnection AFTER the poison
        eprintln!("[smoke C] SLSNewConnection AFTER SLSCopyManagedDisplaySpaces, 200x200 YELLOW at (700,100)");
        {
            let mut new_cid: i32 = 0;
            SLSNewConnection(0, &mut new_cid);
            eprintln!("[smoke C] new_cid={new_cid}");
            let rect = CGRect::new(0.0, 0.0, 200.0, 200.0);
            let mut region: CFTypeRef = std::ptr::null();
            CGSNewRegionWithRect(&rect, &mut region);
            let mut wid: u32 = 0;
            SLSNewWindow(new_cid, 2, 700.0, 100.0, region, &mut wid);
            CFRelease(region);
            SLSSetWindowResolution(new_cid, wid, 2.0);
            SLSSetWindowOpacity(new_cid, wid, false);
            SLSSetWindowLevel(new_cid, wid, 25);
            SLSOrderWindow(new_cid, wid, 1, 0);
            let ctx = SLWindowContextCreate(new_cid, wid, std::ptr::null());
            eprintln!("[smoke C] wid={wid} ctx_null={}", ctx.is_null());
            if !ctx.is_null() {
                let full = CGRect::new(0.0, 0.0, 400.0, 400.0);
                CGContextClearRect(ctx, full);
                CGContextSetRGBFillColor(ctx, 1.0, 1.0, 0.0, 1.0); // YELLOW
                let path = CGPathCreateMutable();
                CGPathAddRect(path, std::ptr::null(), full);
                CGContextAddPath(ctx, path as CGPathRef);
                CGContextFillPath(ctx);
                CGPathRelease(path as CGPathRef);
                CGContextFlush(ctx);
                SLSFlushWindowContentRegion(new_cid, wid, std::ptr::null());
            }
        }

        eprintln!("[smoke] RED=(100,100) main | GREEN=(400,100) reshape | YELLOW=(700,100) SLSNewConnection");
        eprintln!("[smoke] Ctrl-C to exit");
        CFRunLoopRun();
    }
}

/// Smoke test 2: create a border overlay around a real window by ID.
/// All SLS calls inline — no BorderWindow, no reshape, no struct.
fn run_smoke2(target_wid: u32) {
    let cid = unsafe { SLSMainConnectionID() };
    eprintln!("[smoke2] cid={cid}, target_wid={target_wid}");

    unsafe {
        // Step 1: get target window bounds
        let mut bounds = CGRect::default();
        let err = SLSGetWindowBounds(cid, target_wid, &mut bounds);
        eprintln!(
            "[smoke2] bounds: x={:.0} y={:.0} w={:.0} h={:.0} (err={err})",
            bounds.origin.x, bounds.origin.y, bounds.size.width, bounds.size.height
        );
        if err != 0 {
            eprintln!("[smoke2] SLSGetWindowBounds failed");
            return;
        }

        // Step 2: calculate overlay frame (4px border around target)
        let bw = 4.0;
        let overlay_w = bounds.size.width + 2.0 * bw;
        let overlay_h = bounds.size.height + 2.0 * bw;
        let overlay_x = bounds.origin.x - bw;
        let overlay_y = bounds.origin.y - bw;
        eprintln!("[smoke2] overlay: x={overlay_x:.0} y={overlay_y:.0} w={overlay_w:.0} h={overlay_h:.0}");

        // Step 3: create overlay at correct position and size directly
        let rect = CGRect::new(0.0, 0.0, overlay_w, overlay_h);
        let mut region: CFTypeRef = std::ptr::null();
        CGSNewRegionWithRect(&rect, &mut region);

        let mut wid: u32 = 0;
        SLSNewWindow(cid, 2, overlay_x as f32, overlay_y as f32, region, &mut wid);
        CFRelease(region);
        eprintln!("[smoke2] overlay wid={wid}");

        SLSSetWindowResolution(cid, wid, 2.0);
        SLSSetWindowOpacity(cid, wid, false);
        SLSSetWindowLevel(cid, wid, 25);
        SLSOrderWindow(cid, wid, 1, 0);

        // Step 4: draw solid blue fill
        let ctx = SLWindowContextCreate(cid, wid, std::ptr::null());
        eprintln!("[smoke2] ctx null={}", ctx.is_null());
        if !ctx.is_null() {
            let scale = 2.0;
            let full = CGRect::new(0.0, 0.0, overlay_w * scale, overlay_h * scale);
            CGContextClearRect(ctx, full);
            CGContextSetRGBFillColor(ctx, 0.2, 0.5, 0.9, 1.0); // blue
            let path = CGPathCreateMutable();
            CGPathAddRect(path, std::ptr::null(), full);
            CGContextAddPath(ctx, path as CGPathRef);
            CGContextFillPath(ctx);
            CGPathRelease(path as CGPathRef);
            CGContextFlush(ctx);
            SLSFlushWindowContentRegion(cid, wid, std::ptr::null());
            CGContextRelease(ctx);
            eprintln!("[smoke2] drew blue overlay around target wid {target_wid}");
        }

        eprintln!("[smoke2] Ctrl-C to exit");
        CFRunLoopRun();
    }
}
