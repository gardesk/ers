//! ers — window border renderer

mod events;
mod skylight;

use events::Event;
use skylight::*;
use std::collections::HashMap;
use std::ptr;
use std::sync::mpsc;

/// Per-overlay state: the connection it was created on + its wid.
struct Overlay {
    cid: CGSConnectionID,
    wid: u32,
}

/// Tracks overlays for target windows.
struct BorderMap {
    overlays: HashMap<u32, Overlay>,
    main_cid: CGSConnectionID,
    own_pid: i32,
    border_width: f64,
    focused_wid: u32,
    active_color: (f64, f64, f64, f64),
    inactive_color: (f64, f64, f64, f64),
}

impl BorderMap {
    fn new(cid: CGSConnectionID, own_pid: i32, border_width: f64) -> Self {
        Self {
            overlays: HashMap::new(),
            main_cid: cid,
            own_pid,
            border_width,
            focused_wid: 0,
            active_color: (0.32, 0.58, 0.89, 1.0),   // #5294e2
            inactive_color: (0.35, 0.35, 0.35, 0.8),  // dim gray
        }
    }

    fn color_for(&self, target_wid: u32) -> (f64, f64, f64, f64) {
        if target_wid == self.focused_wid { self.active_color } else { self.inactive_color }
    }

    fn is_overlay(&self, wid: u32) -> bool {
        self.overlays.values().any(|o| o.wid == wid)
    }

    /// Add border (batch mode, uses main cid).
    fn add_batch(&mut self, target_wid: u32) {
        if self.overlays.contains_key(&target_wid) { return; }
        let color = self.color_for(target_wid);
        if let Some((cid, wid)) = create_overlay(self.main_cid, target_wid, self.border_width, color) {
            self.overlays.insert(target_wid, Overlay { cid, wid });
        }
    }

    /// Add border (event mode). Uses main_cid — fresh connections create
    /// invisible windows on Tahoe.
    fn add_fresh(&mut self, target_wid: u32) {
        if self.overlays.contains_key(&target_wid) { return; }
        let color = self.color_for(target_wid);
        if let Some((cid, wid)) = create_overlay(self.main_cid, target_wid, self.border_width, color) {
            self.overlays.insert(target_wid, Overlay { cid, wid });
        }
    }

    fn remove(&mut self, target_wid: u32) {
        if let Some(overlay) = self.overlays.remove(&target_wid) {
            unsafe {
                // Move off-screen first (most reliable hide on Tahoe)
                let offscreen = CGPoint { x: -99999.0, y: -99999.0 };
                SLSMoveWindow(overlay.cid, overlay.wid, &offscreen);
                SLSSetWindowAlpha(overlay.cid, overlay.wid, 0.0);
                SLSOrderWindow(overlay.cid, overlay.wid, 0, 0);
                SLSReleaseWindow(overlay.cid, overlay.wid);
                if overlay.cid != self.main_cid {
                    SLSReleaseConnection(overlay.cid);
                }
            }
        }
    }

    /// Move overlay to match target's current position (no recreate).
    fn reposition(&self, target_wid: u32) {
        if let Some(overlay) = self.overlays.get(&target_wid) {
            unsafe {
                let mut bounds = CGRect::default();
                if SLSGetWindowBounds(overlay.cid, target_wid, &mut bounds) != kCGErrorSuccess {
                    return;
                }
                let bw = self.border_width;
                let origin = CGPoint {
                    x: bounds.origin.x - bw,
                    y: bounds.origin.y - bw,
                };
                SLSMoveWindow(overlay.cid, overlay.wid, &origin);
            }
        }
    }

    /// Recreate overlay at new size. Uses fresh connection.
    fn recreate(&mut self, target_wid: u32) {
        if !self.overlays.contains_key(&target_wid) { return; }
        self.remove(target_wid);
        self.add_fresh(target_wid);
        self.subscribe_target(target_wid);
    }

    fn hide(&self, target_wid: u32) {
        if let Some(o) = self.overlays.get(&target_wid) {
            unsafe { SLSOrderWindow(o.cid, o.wid, 0, 0); }
        }
    }

    fn unhide(&self, target_wid: u32) {
        if let Some(o) = self.overlays.get(&target_wid) {
            unsafe {
                SLSSetWindowLevel(o.cid, o.wid, 25);
                SLSOrderWindow(o.cid, o.wid, 1, 0);
            }
        }
    }

    fn apply_tags_all(&self) {
        unsafe {
            let tags: u64 = 1 << 1;
            for o in self.overlays.values() {
                SLSSetWindowTags(o.cid, o.wid, &tags, 64);
                disable_shadow(o.wid);
            }
        }
    }

    fn subscribe_target(&self, target_wid: u32) {
        unsafe {
            SLSRequestNotificationsForWindows(self.main_cid, &target_wid, 1);
        }
    }

    fn subscribe_all(&self) {
        let target_wids: Vec<u32> = self.overlays.keys().copied().collect();
        if target_wids.is_empty() { return; }
        unsafe {
            SLSRequestNotificationsForWindows(
                self.main_cid,
                target_wids.as_ptr(),
                target_wids.len() as i32,
            );
        }
    }

    /// Redraw an existing overlay with a new color (no destroy/recreate).
    fn redraw(&self, target_wid: u32) {
        if let Some(overlay) = self.overlays.get(&target_wid) {
            unsafe {
                let mut bounds = CGRect::default();
                if SLSGetWindowBounds(overlay.cid, target_wid, &mut bounds) != kCGErrorSuccess {
                    return;
                }
                let bw = self.border_width;
                let ow = bounds.size.width + 2.0 * bw;
                let oh = bounds.size.height + 2.0 * bw;

                let ctx = SLWindowContextCreate(overlay.cid, overlay.wid, ptr::null());
                if ctx.is_null() { return; }

                let full = CGRect::new(0.0, 0.0, ow, oh);
                CGContextClearRect(ctx, full);

                let color = self.color_for(target_wid);
                let stroke_rect = CGRect::new(bw / 2.0, bw / 2.0, ow - bw, oh - bw);
                let radius = 10.0_f64;
                let max_r = (stroke_rect.size.width.min(stroke_rect.size.height) / 2.0).max(0.0);
                let r = radius.min(max_r);

                CGContextSetRGBStrokeColor(ctx, color.0, color.1, color.2, color.3);
                CGContextSetLineWidth(ctx, bw);
                let path = CGPathCreateWithRoundedRect(stroke_rect, r, r, ptr::null());
                if !path.is_null() {
                    CGContextAddPath(ctx, path);
                    CGContextStrokePath(ctx);
                    CGPathRelease(path);
                }

                CGContextFlush(ctx);
                SLSFlushWindowContentRegion(overlay.cid, overlay.wid, ptr::null());
                CGContextRelease(ctx);
            }
        }
    }

    /// Detect focused window and update border colors if focus changed.
    /// Returns true if focus changed (callers should resubscribe).
    fn update_focus(&mut self) -> bool {
        let front = get_front_window(self.own_pid);
        if front == 0 || front == self.focused_wid { return false; }

        let old = self.focused_wid;
        self.focused_wid = front;
        eprintln!("[focus] {} -> {} (tracked={})", old, front, self.overlays.contains_key(&front));

        // Recreate overlays with new colors — re-obtaining a CGContext
        // for an existing window is unreliable on Tahoe
        if self.overlays.contains_key(&old) {
            self.recreate(old);
        }
        if self.overlays.contains_key(&front) {
            self.recreate(front);
        }
        true
    }
}

/// Get the front (focused) window ID using CGWindowListCopyWindowInfo.
/// Avoids all SLS display/space queries which poison SLSNewWindow globally.
fn get_front_window(own_pid: i32) -> u32 {
    unsafe {
        let list = CGWindowListCopyWindowInfo(kCGWindowListOptionOnScreenOnly, kCGNullWindowID);
        if list.is_null() { return 0; }

        let count = CFArrayGetCount(list);
        let wid_key = CFStringCreateWithCString(ptr::null(), b"kCGWindowNumber\0".as_ptr(), kCFStringEncodingUTF8);
        let pid_key = CFStringCreateWithCString(ptr::null(), b"kCGWindowOwnerPID\0".as_ptr(), kCFStringEncodingUTF8);
        let layer_key = CFStringCreateWithCString(ptr::null(), b"kCGWindowLayer\0".as_ptr(), kCFStringEncodingUTF8);

        // CGWindowListCopyWindowInfo returns windows in front-to-back order.
        // First layer-0 window not owned by us is the focused window.
        let mut front_wid: u32 = 0;
        for i in 0..count {
            let dict = CFArrayGetValueAtIndex(list, i);
            if dict.is_null() { continue; }

            let mut v: CFTypeRef = ptr::null();

            let mut layer: i32 = -1;
            if CFDictionaryGetValueIfPresent(dict, layer_key as CFTypeRef, &mut v) {
                CFNumberGetValue(v, kCFNumberSInt32Type, &mut layer as *mut _ as *mut _);
            }
            if layer != 0 { continue; }

            let mut pid: i32 = 0;
            if CFDictionaryGetValueIfPresent(dict, pid_key as CFTypeRef, &mut v) {
                CFNumberGetValue(v, kCFNumberSInt32Type, &mut pid as *mut _ as *mut _);
            }
            if pid == own_pid { continue; }

            let mut wid: u32 = 0;
            if CFDictionaryGetValueIfPresent(dict, wid_key as CFTypeRef, &mut v) {
                CFNumberGetValue(v, kCFNumberSInt32Type, &mut wid as *mut _ as *mut _);
            }
            if wid != 0 {
                front_wid = wid;
                break;
            }
        }

        CFRelease(wid_key as CFTypeRef);
        CFRelease(pid_key as CFTypeRef);
        CFRelease(layer_key as CFTypeRef);
        CFRelease(list);
        front_wid
    }
}

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

    // Event channel
    let (tx, rx) = mpsc::channel();
    events::init(tx, own_pid);
    events::register(cid);
    setup_event_port(cid);

    // Discover and create borders
    let mut borders = BorderMap::new(cid, own_pid, border_width);

    if let Some(target) = args.get(1).and_then(|s| s.parse::<u32>().ok()) {
        borders.add_batch(target);
    } else {
        let wids = discover_windows(cid, own_pid);
        eprintln!("{} windows discovered", wids.len());
        for &wid in &wids {
            borders.add_batch(wid);
        }
        eprintln!("{} borders created", borders.overlays.len());
    }

    borders.subscribe_all();

    if borders.update_focus() {
        borders.subscribe_all();
    }

    eprintln!("{} overlays tracked", borders.overlays.len());

    // Process events on background thread with coalescing
    std::thread::spawn(move || {
        use std::collections::HashSet;

        // Persist across batches: windows we know about but haven't bordered yet
        let mut pending: HashSet<u32> = HashSet::new();

        loop {
            let first = match rx.recv() {
                Ok(e) => e,
                Err(_) => break,
            };

            std::thread::sleep(std::time::Duration::from_millis(150));

            let mut events = vec![first];
            while let Ok(e) = rx.try_recv() {
                events.push(e);
            }

            let mut moved: HashSet<u32> = HashSet::new();
            let mut resized: HashSet<u32> = HashSet::new();
            let mut destroyed: HashSet<u32> = HashSet::new();
            let mut needs_resubscribe = false;

            for event in events {
                match event {
                    Event::Move(wid) => {
                        if !borders.is_overlay(wid) {
                            moved.insert(wid);
                        }
                    }
                    Event::Resize(wid) => {
                        if !borders.is_overlay(wid) {
                            resized.insert(wid);
                        }
                    }
                    Event::Close(wid) | Event::Destroy(wid) => {
                        if !borders.is_overlay(wid) {
                            destroyed.insert(wid);
                            pending.remove(&wid);
                        }
                    }
                    Event::Create(wid) => {
                        if !borders.is_overlay(wid) {
                            pending.insert(wid);
                            borders.subscribe_target(wid);
                        }
                    }
                    Event::Hide(wid) => borders.hide(wid),
                    Event::Unhide(wid) => borders.unhide(wid),
                    Event::FrontChange => {
                        needs_resubscribe = true; // focus change may need resubscribe
                    }
                    Event::SpaceChange => {}
                }
            }

            // Destroys
            for wid in &destroyed {
                borders.remove(*wid);
            }

            // Promote ALL pending creates that weren't destroyed
            // (the 150ms debounce is enough for tarmac to position them)
            let ready: Vec<u32> = pending.iter()
                .filter(|wid| !destroyed.contains(wid))
                .copied()
                .collect();
            // Filter overlapping creates: if two windows overlap, keep smaller one
            let mut bounds_map: Vec<(u32, CGRect)> = Vec::new();
            for &wid in &ready {
                unsafe {
                    let mut b = CGRect::default();
                    SLSGetWindowBounds(borders.main_cid, wid, &mut b);
                    bounds_map.push((wid, b));
                }
            }

            // If two new windows overlap closely, skip the larger one (container)
            let mut skip: std::collections::HashSet<u32> = HashSet::new();
            for i in 0..bounds_map.len() {
                for j in (i+1)..bounds_map.len() {
                    let (wid_a, a) = &bounds_map[i];
                    let (wid_b, b) = &bounds_map[j];
                    // Check if centers are close (within 30px)
                    let cx_a = a.origin.x + a.size.width / 2.0;
                    let cy_a = a.origin.y + a.size.height / 2.0;
                    let cx_b = b.origin.x + b.size.width / 2.0;
                    let cy_b = b.origin.y + b.size.height / 2.0;
                    if (cx_a - cx_b).abs() < 30.0 && (cy_a - cy_b).abs() < 30.0 {
                        // Skip the larger one
                        let area_a = a.size.width * a.size.height;
                        let area_b = b.size.width * b.size.height;
                        if area_a > area_b {
                            skip.insert(*wid_a);
                        } else {
                            skip.insert(*wid_b);
                        }
                    }
                }
            }

            for &wid in &ready {
                pending.remove(&wid);
                if !skip.contains(&wid) {
                    borders.add_fresh(wid);
                    needs_resubscribe = true;
                }
            }

            // Moves and resizes: recreate at new position/size
            let changed: HashSet<u32> = moved.union(&resized).copied().collect();
            for wid in &changed {
                if borders.overlays.contains_key(wid) {
                    borders.recreate(*wid);
                    needs_resubscribe = true;
                }
            }

            // Update focus (detects front window, recreates borders if changed)
            if borders.update_focus() {
                needs_resubscribe = true;
            }

            // Re-subscribe ALL tracked windows (SLSRequestNotificationsForWindows replaces, not appends)
            if needs_resubscribe || !destroyed.is_empty() {
                borders.subscribe_all();
            }
        }
    });

    unsafe { CFRunLoopRun() };
}

fn setup_event_port(cid: CGSConnectionID) {
    unsafe {
        let mut port: u32 = 0;
        if SLSGetEventPort(cid, &mut port) != kCGErrorSuccess { return; }
        let cf_port = CFMachPortCreateWithPort(ptr::null(), port, drain_events as *const _, ptr::null(), false);
        if cf_port.is_null() { return; }
        _CFMachPortSetOptions(cf_port, 0x40);
        let source = CFMachPortCreateRunLoopSource(ptr::null(), cf_port, 0);
        if !source.is_null() {
            CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode);
            CFRelease(source);
        }
        CFRelease(cf_port);
    }
}

unsafe extern "C" fn drain_events(_: CFMachPortRef, _: *mut std::ffi::c_void, _: i64, _: *mut std::ffi::c_void) {
    unsafe {
        let cid = SLSMainConnectionID();
        let mut ev = SLEventCreateNextEvent(cid);
        while !ev.is_null() {
            CFRelease(ev as CFTypeRef);
            ev = SLEventCreateNextEvent(cid);
        }
    }
}

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

fn create_overlay(
    cid: CGSConnectionID,
    target_wid: u32,
    border_width: f64,
    color: (f64, f64, f64, f64),
) -> Option<(CGSConnectionID, u32)> {
    unsafe {
        let mut bounds = CGRect::default();
        let rc = SLSGetWindowBounds(cid, target_wid, &mut bounds);
        if rc != kCGErrorSuccess {
            eprintln!("[create_overlay] SLSGetWindowBounds failed for wid={target_wid} rc={rc}");
            return None;
        }
        if bounds.size.width < 10.0 || bounds.size.height < 10.0 {
            eprintln!("[create_overlay] wid={target_wid} too small: {}x{}", bounds.size.width, bounds.size.height);
            return None;
        }

        let bw = border_width;
        let ow = bounds.size.width + 2.0 * bw;
        let oh = bounds.size.height + 2.0 * bw;
        let ox = bounds.origin.x - bw;
        let oy = bounds.origin.y - bw;

        let frame = CGRect::new(0.0, 0.0, ow, oh);
        let mut region: CFTypeRef = ptr::null();
        CGSNewRegionWithRect(&frame, &mut region);
        if region.is_null() {
            eprintln!("[create_overlay] CGSNewRegionWithRect failed for wid={target_wid}");
            return None;
        }

        let mut wid: u32 = 0;
        SLSNewWindow(cid, 2, ox as f32, oy as f32, region, &mut wid);
        CFRelease(region);
        if wid == 0 {
            eprintln!("[create_overlay] SLSNewWindow returned 0 for target={target_wid} cid={cid}");
            return None;
        }

        eprintln!("[create_overlay] created overlay wid={wid} for target={target_wid} color=({:.2},{:.2},{:.2},{:.2})",
            color.0, color.1, color.2, color.3);

        SLSSetWindowResolution(cid, wid, 2.0);
        SLSSetWindowOpacity(cid, wid, false);
        SLSSetWindowLevel(cid, wid, 25);
        SLSOrderWindow(cid, wid, 1, 0);

        // Draw border (point coordinates)
        let ctx = SLWindowContextCreate(cid, wid, ptr::null());
        if ctx.is_null() {
            eprintln!("[create_overlay] SLWindowContextCreate returned null for overlay wid={wid}");
            SLSReleaseWindow(cid, wid);
            return None;
        }

        let full = CGRect::new(0.0, 0.0, ow, oh);
        CGContextClearRect(ctx, full);

        let stroke_rect = CGRect::new(bw / 2.0, bw / 2.0, ow - bw, oh - bw);
        let radius = 10.0_f64;
        let max_r = (stroke_rect.size.width.min(stroke_rect.size.height) / 2.0).max(0.0);
        let r = radius.min(max_r);

        CGContextSetRGBStrokeColor(ctx, color.0, color.1, color.2, color.3);
        CGContextSetLineWidth(ctx, bw);
        let path = CGPathCreateWithRoundedRect(stroke_rect, r, r, ptr::null());
        if !path.is_null() {
            CGContextAddPath(ctx, path);
            CGContextStrokePath(ctx);
            CGPathRelease(path);
        }

        CGContextFlush(ctx);
        SLSFlushWindowContentRegion(cid, wid, ptr::null());
        CGContextRelease(ctx);

        Some((cid, wid))
    }
}

fn list_windows() {
    let cid = unsafe { SLSMainConnectionID() };
    unsafe {
        let list = CGWindowListCopyWindowInfo(kCGWindowListOptionOnScreenOnly, kCGNullWindowID);
        if list.is_null() { return; }
        let count = CFArrayGetCount(list);
        let wid_key = CFStringCreateWithCString(ptr::null(), b"kCGWindowNumber\0".as_ptr(), kCFStringEncodingUTF8);
        let layer_key = CFStringCreateWithCString(ptr::null(), b"kCGWindowLayer\0".as_ptr(), kCFStringEncodingUTF8);

        eprintln!("{:>6}  {:>8}  {:>8}  {:>6}  {:>6}", "wid", "x", "y", "w", "h");
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
            eprintln!("{wid:>6}  {:>8.0}  {:>8.0}  {:>6.0}  {:>6.0}",
                bounds.origin.x, bounds.origin.y, bounds.size.width, bounds.size.height);
        }
        CFRelease(wid_key as CFTypeRef);
        CFRelease(layer_key as CFTypeRef);
        CFRelease(list);
    }
}

unsafe fn disable_shadow(wid: u32) {
    let density: i64 = 0;
    let density_cf = CFNumberCreate(ptr::null(), kCFNumberCFIndexType, &density as *const _ as *const _);
    let key = CFStringCreateWithCString(ptr::null(), b"com.apple.WindowShadowDensity\0".as_ptr(), kCFStringEncodingUTF8);
    let keys = [key as CFTypeRef];
    let values = [density_cf as CFTypeRef];
    let dict = CFDictionaryCreate(
        ptr::null(), keys.as_ptr(), values.as_ptr(), 1,
        &kCFTypeDictionaryKeyCallBacks as *const _ as *const _,
        &kCFTypeDictionaryValueCallBacks as *const _ as *const _,
    );
    SLSWindowSetShadowProperties(wid, dict);
    CFRelease(dict);
    CFRelease(density_cf);
    CFRelease(key as CFTypeRef);
}
