//! ers — window border renderer

mod events;
mod skylight;

use events::Event;
use skylight::*;
use std::collections::HashMap;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use tracing::debug;

static SIGNAL_STOP_REQUESTED: AtomicBool = AtomicBool::new(false);
const MIN_TRACKED_WINDOW_SIZE: f64 = 4.0;
const GEOMETRY_EPSILON: f64 = 0.5;
const SCALE_EPSILON: f64 = 0.01;
const WINDOW_ATTRIBUTE_REAL: u64 = 1 << 1;
const WINDOW_TAG_DOCUMENT: u64 = 1 << 0;
const WINDOW_TAG_FLOATING: u64 = 1 << 1;
const WINDOW_TAG_ATTACHED: u64 = 1 << 7;
const WINDOW_TAG_IGNORES_CYCLE: u64 = 1 << 18;
const WINDOW_TAG_MODAL: u64 = 1 << 31;
const WINDOW_TAG_REAL_SURFACE: u64 = 1 << 58;

/// Per-overlay state: the connection it was created on + its wid.
struct Overlay {
    cid: CGSConnectionID,
    wid: u32,
    bounds: CGRect,
    scale: f64,
}

fn window_area(bounds: CGRect) -> f64 {
    bounds.size.width * bounds.size.height
}

fn intersection_area(a: CGRect, b: CGRect) -> f64 {
    let left = a.origin.x.max(b.origin.x);
    let top = a.origin.y.max(b.origin.y);
    let right = (a.origin.x + a.size.width).min(b.origin.x + b.size.width);
    let bottom = (a.origin.y + a.size.height).min(b.origin.y + b.size.height);
    let width = (right - left).max(0.0);
    let height = (bottom - top).max(0.0);
    width * height
}

fn is_same_window_surface(a: CGRect, b: CGRect) -> bool {
    let smaller = window_area(a).min(window_area(b));
    smaller > 0.0 && intersection_area(a, b) / smaller >= 0.9
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SurfacePreference {
    KeepExisting,
    ReplaceExisting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WindowMetadata {
    parent_wid: u32,
    tags: u64,
    attributes: u64,
}

fn surface_preference(existing: CGRect, candidate: CGRect) -> Option<SurfacePreference> {
    if !is_same_window_surface(existing, candidate) {
        return None;
    }

    if window_area(candidate) > window_area(existing) {
        Some(SurfacePreference::ReplaceExisting)
    } else {
        Some(SurfacePreference::KeepExisting)
    }
}

fn minimum_trackable_dimension(border_width: f64) -> f64 {
    border_width.max(MIN_TRACKED_WINDOW_SIZE)
}

fn is_trackable_window(bounds: CGRect, border_width: f64) -> bool {
    let min_dimension = minimum_trackable_dimension(border_width);
    bounds.size.width >= min_dimension && bounds.size.height >= min_dimension
}

fn origin_changed(a: CGRect, b: CGRect) -> bool {
    (a.origin.x - b.origin.x).abs() > GEOMETRY_EPSILON
        || (a.origin.y - b.origin.y).abs() > GEOMETRY_EPSILON
}

fn size_changed(a: CGRect, b: CGRect) -> bool {
    (a.size.width - b.size.width).abs() > GEOMETRY_EPSILON
        || (a.size.height - b.size.height).abs() > GEOMETRY_EPSILON
}

fn is_suitable_window_metadata(metadata: WindowMetadata) -> bool {
    metadata.parent_wid == 0
        && ((metadata.attributes & WINDOW_ATTRIBUTE_REAL) != 0
            || (metadata.tags & WINDOW_TAG_REAL_SURFACE) != 0)
        && (metadata.tags & WINDOW_TAG_ATTACHED) == 0
        && (metadata.tags & WINDOW_TAG_IGNORES_CYCLE) == 0
        && ((metadata.tags & WINDOW_TAG_DOCUMENT) != 0
            || ((metadata.tags & WINDOW_TAG_FLOATING) != 0
                && (metadata.tags & WINDOW_TAG_MODAL) != 0))
}

fn query_window_metadata(cid: CGSConnectionID, wid: u32) -> Option<WindowMetadata> {
    unsafe {
        let window_ref = cfarray_of_cfnumbers(
            (&wid as *const u32).cast(),
            std::mem::size_of::<u32>(),
            1,
            kCFNumberSInt32Type,
        );
        if window_ref.is_null() {
            return None;
        }

        let query = SLSWindowQueryWindows(cid, window_ref, 0x0);
        CFRelease(window_ref);
        if query.is_null() {
            return None;
        }

        let iterator = SLSWindowQueryResultCopyWindows(query);
        CFRelease(query);
        if iterator.is_null() {
            return None;
        }

        let metadata = if SLSWindowIteratorAdvance(iterator) {
            Some(WindowMetadata {
                parent_wid: SLSWindowIteratorGetParentID(iterator),
                tags: SLSWindowIteratorGetTags(iterator),
                attributes: SLSWindowIteratorGetAttributes(iterator),
            })
        } else {
            None
        };

        CFRelease(iterator);
        metadata
    }
}

fn is_suitable_window(cid: CGSConnectionID, wid: u32) -> bool {
    match query_window_metadata(cid, wid) {
        Some(metadata) => {
            let suitable = is_suitable_window_metadata(metadata);
            if !suitable {
                debug!(
                    "[window_filter] rejecting wid={} parent={} tags={:#x} attributes={:#x}",
                    wid, metadata.parent_wid, metadata.tags, metadata.attributes
                );
            }
            suitable
        }
        None => false,
    }
}

fn cf_string_from_static(name: &std::ffi::CStr) -> CFStringRef {
    unsafe { CFStringCreateWithCString(ptr::null(), name.as_ptr().cast(), kCFStringEncodingUTF8) }
}

unsafe extern "C" fn handle_sigint(_: libc::c_int) {
    SIGNAL_STOP_REQUESTED.store(true, Ordering::Relaxed);
}

fn display_scale_for_bounds(bounds: CGRect) -> f64 {
    let point = CGPoint {
        x: bounds.origin.x + bounds.size.width / 2.0,
        y: bounds.origin.y + bounds.size.height / 2.0,
    };

    unsafe {
        let mut display_id = 0u32;
        let mut count = 0u32;
        if CGGetDisplaysWithPoint(point, 1, &mut display_id, &mut count) != kCGErrorSuccess
            || count == 0
        {
            return 2.0;
        }

        let mode = CGDisplayCopyDisplayMode(display_id);
        if mode.is_null() {
            return 2.0;
        }

        let width = CGDisplayModeGetWidth(mode) as f64;
        let height = CGDisplayModeGetHeight(mode) as f64;
        let pixel_width = CGDisplayModeGetPixelWidth(mode) as f64;
        let pixel_height = CGDisplayModeGetPixelHeight(mode) as f64;
        CFRelease(mode as CFTypeRef);

        let scale_x = if width > 0.0 {
            pixel_width / width
        } else {
            0.0
        };
        let scale_y = if height > 0.0 {
            pixel_height / height
        } else {
            0.0
        };

        let scale = match (scale_x.is_finite(), scale_y.is_finite()) {
            (true, true) if scale_x >= 1.0 && scale_y >= 1.0 => (scale_x + scale_y) / 2.0,
            (true, _) if scale_x >= 1.0 => scale_x,
            (_, true) if scale_y >= 1.0 => scale_y,
            _ => 2.0,
        };

        debug!(
            "[display_scale] display={} point=({:.1},{:.1}) scale={:.2}",
            display_id, point.x, point.y, scale
        );

        scale
    }
}

/// Tracks overlays for target windows.
struct BorderMap {
    overlays: HashMap<u32, Overlay>,
    main_cid: CGSConnectionID,
    own_pid: i32,
    border_width: f64,
    radius: f64,
    focused_wid: u32,
    active_color: (f64, f64, f64, f64),
    inactive_color: (f64, f64, f64, f64),
    active_only: bool,
}

impl BorderMap {
    fn new(cid: CGSConnectionID, own_pid: i32, border_width: f64) -> Self {
        Self {
            overlays: HashMap::new(),
            main_cid: cid,
            own_pid,
            border_width,
            radius: 10.0,
            focused_wid: 0,
            active_color: (0.32, 0.58, 0.89, 1.0),   // #5294e2
            inactive_color: (0.35, 0.35, 0.35, 0.8), // dim gray
            active_only: false,
        }
    }

    fn color_for(&self, target_wid: u32) -> (f64, f64, f64, f64) {
        if target_wid == self.focused_wid {
            self.active_color
        } else {
            self.inactive_color
        }
    }

    fn is_overlay(&self, wid: u32) -> bool {
        self.overlays.values().any(|o| o.wid == wid)
    }

    /// Add border using the standard filtering path.
    fn add_batch(&mut self, target_wid: u32) {
        self.add_fresh(target_wid);
    }

    fn surface_replacements(&self, target_wid: u32, bounds: CGRect) -> Option<Vec<u32>> {
        let mut replacements = Vec::new();

        for &existing_wid in self.overlays.keys() {
            if existing_wid == target_wid {
                continue;
            }

            unsafe {
                let mut existing_bounds = CGRect::default();
                if SLSGetWindowBounds(self.main_cid, existing_wid, &mut existing_bounds)
                    != kCGErrorSuccess
                {
                    continue;
                }

                match surface_preference(existing_bounds, bounds) {
                    Some(SurfacePreference::KeepExisting) => return None,
                    Some(SurfacePreference::ReplaceExisting) => replacements.push(existing_wid),
                    None => {}
                }
            }
        }

        Some(replacements)
    }

    /// Add border (event mode). Uses main_cid — fresh connections create
    /// invisible windows on Tahoe.
    fn add_fresh(&mut self, target_wid: u32) {
        if self.overlays.contains_key(&target_wid) {
            return;
        }

        // Filter: must be visible, owned by another process, not tiny
        let bounds = unsafe {
            let mut shown = false;
            SLSWindowIsOrderedIn(self.main_cid, target_wid, &mut shown);
            if !shown {
                return;
            }

            let mut wid_cid: CGSConnectionID = 0;
            SLSGetWindowOwner(self.main_cid, target_wid, &mut wid_cid);
            let mut pid: i32 = 0;
            SLSConnectionGetPID(wid_cid, &mut pid);
            if pid == self.own_pid {
                return;
            }
            if !is_suitable_window(self.main_cid, target_wid) {
                return;
            }

            let mut bounds = CGRect::default();
            SLSGetWindowBounds(self.main_cid, target_wid, &mut bounds);
            if !is_trackable_window(bounds, self.border_width) {
                return;
            }
            bounds
        };

        let Some(replacements) = self.surface_replacements(target_wid, bounds) else {
            return;
        };

        for wid in replacements {
            self.remove(wid);
        }

        let color = self.color_for(target_wid);
        if let Some((cid, wid, bounds, scale)) = create_overlay(
            self.main_cid,
            target_wid,
            self.border_width,
            self.radius,
            color,
        ) {
            self.overlays.insert(
                target_wid,
                Overlay {
                    cid,
                    wid,
                    bounds,
                    scale,
                },
            );
        }
    }

    fn remove_all(&mut self) {
        let wids: Vec<u32> = self.overlays.keys().copied().collect();
        for wid in wids {
            self.remove(wid);
        }
    }

    fn remove(&mut self, target_wid: u32) {
        if let Some(overlay) = self.overlays.remove(&target_wid) {
            unsafe {
                // Move off-screen first (most reliable hide on Tahoe)
                let offscreen = CGPoint {
                    x: -99999.0,
                    y: -99999.0,
                };
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

    /// Reconcile a tracked overlay against its target window.
    fn sync_overlay(&mut self, target_wid: u32) -> bool {
        let Some((overlay_cid, overlay_wid, overlay_bounds, overlay_scale)) = self
            .overlays
            .get(&target_wid)
            .map(|overlay| (overlay.cid, overlay.wid, overlay.bounds, overlay.scale))
        else {
            return false;
        };

        unsafe {
            let mut bounds = CGRect::default();
            if SLSGetWindowBounds(self.main_cid, target_wid, &mut bounds) != kCGErrorSuccess {
                return false;
            }

            if !is_suitable_window(self.main_cid, target_wid) {
                self.remove(target_wid);
                return true;
            }

            if !is_trackable_window(bounds, self.border_width) {
                self.remove(target_wid);
                return true;
            }

            let scale = display_scale_for_bounds(bounds);
            if size_changed(overlay_bounds, bounds) || (scale - overlay_scale).abs() > SCALE_EPSILON
            {
                debug!(
                    "[sync_overlay] target={} geometry changed bounds=({:.1},{:.1},{:.1},{:.1}) -> ({:.1},{:.1},{:.1},{:.1}) scale {:.2} -> {:.2}, recreating",
                    target_wid,
                    overlay_bounds.origin.x,
                    overlay_bounds.origin.y,
                    overlay_bounds.size.width,
                    overlay_bounds.size.height,
                    bounds.origin.x,
                    bounds.origin.y,
                    bounds.size.width,
                    bounds.size.height,
                    overlay_scale,
                    scale
                );
                self.recreate(target_wid);
                return true;
            }

            if origin_changed(overlay_bounds, bounds) {
                let bw = self.border_width;
                let origin = CGPoint {
                    x: bounds.origin.x - bw,
                    y: bounds.origin.y - bw,
                };
                SLSMoveWindow(overlay_cid, overlay_wid, &origin);
            }

            if let Some(overlay) = self.overlays.get_mut(&target_wid) {
                overlay.bounds = bounds;
                overlay.scale = scale;
                overlay.cid = overlay_cid;
                overlay.wid = overlay_wid;
            }
        }

        false
    }

    fn reconcile_tracked(&mut self) -> bool {
        let tracked: Vec<u32> = self.overlays.keys().copied().collect();
        let mut changed = false;

        for wid in tracked {
            changed |= self.sync_overlay(wid);
        }

        changed
    }

    /// Recreate overlay at new size.
    fn recreate(&mut self, target_wid: u32) {
        if !self.overlays.contains_key(&target_wid) {
            return;
        }
        self.remove(target_wid);
        self.add_fresh(target_wid);
        if self.active_only && target_wid != self.focused_wid {
            self.hide(target_wid);
        }
        self.subscribe_target(target_wid);
    }

    fn hide(&self, target_wid: u32) {
        if let Some(o) = self.overlays.get(&target_wid) {
            unsafe {
                SLSOrderWindow(o.cid, o.wid, 0, 0);
            }
        }
    }

    fn unhide(&self, target_wid: u32) {
        if let Some(o) = self.overlays.get(&target_wid) {
            unsafe {
                SLSSetWindowLevel(o.cid, o.wid, 0);
                SLSOrderWindow(o.cid, o.wid, 1, target_wid);
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
        if target_wids.is_empty() {
            return;
        }
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
                if ctx.is_null() {
                    return;
                }

                let color = self.color_for(target_wid);
                draw_border(ctx, ow, oh, bw, self.radius, color);
                SLSFlushWindowContentRegion(overlay.cid, overlay.wid, ptr::null());
                CGContextRelease(ctx);
            }
        }
    }

    /// Detect focused window and update border colors if focus changed.
    fn update_focus(&mut self) {
        let front = get_front_window(self.own_pid);
        if front == 0 || front == self.focused_wid {
            return;
        }

        let old = self.focused_wid;
        self.focused_wid = front;
        debug!("[focus] {} -> {}", old, front);

        if self.active_only {
            self.hide(old);
            self.unhide(front);
        }
        self.redraw(old);
        self.redraw(front);
    }

    /// Discover on-screen windows and create borders for any untracked ones.
    /// Called on space changes to pick up windows from workspaces we haven't visited.
    fn discover_untracked(&mut self) {
        let wids = discover_windows(self.main_cid, self.own_pid);
        let mut added = false;
        for wid in wids {
            if !self.overlays.contains_key(&wid) {
                self.add_fresh(wid);
                if self.active_only && wid != self.focused_wid {
                    self.hide(wid);
                }
                added = true;
            }
        }
        if added {
            self.subscribe_all();
        }
    }

    /// In active-only mode, ensure only the focused overlay is visible.
    fn enforce_active_only(&self) {
        if !self.active_only {
            return;
        }
        for (&target_wid, o) in &self.overlays {
            if target_wid == self.focused_wid {
                unsafe {
                    SLSSetWindowLevel(o.cid, o.wid, 0);
                    SLSOrderWindow(o.cid, o.wid, 1, target_wid);
                }
            } else {
                unsafe {
                    SLSOrderWindow(o.cid, o.wid, 0, 0);
                }
            }
        }
    }
}

/// Get the front (focused) window ID.
/// Uses _SLPSGetFrontProcess to find the active app, then CGWindowListCopyWindowInfo
/// to find its topmost layer-0 window. This works with tiling WMs where focus
/// changes don't alter z-order.
fn get_front_window(own_pid: i32) -> u32 {
    unsafe {
        // Step 1: get the front (active) process PID
        let mut psn = ProcessSerialNumber { high: 0, low: 0 };
        _SLPSGetFrontProcess(&mut psn);
        let mut front_cid: CGSConnectionID = 0;
        SLSGetConnectionIDForPSN(SLSMainConnectionID(), &mut psn, &mut front_cid);
        let mut front_pid: i32 = 0;
        SLSConnectionGetPID(front_cid, &mut front_pid);
        if front_pid == 0 || front_pid == own_pid {
            return 0;
        }

        // Step 2: find the topmost layer-0 window belonging to that process
        let list = CGWindowListCopyWindowInfo(kCGWindowListOptionOnScreenOnly, kCGNullWindowID);
        if list.is_null() {
            return 0;
        }

        let count = CFArrayGetCount(list);
        let wid_key = cf_string_from_static(c"kCGWindowNumber");
        let pid_key = cf_string_from_static(c"kCGWindowOwnerPID");
        let layer_key = cf_string_from_static(c"kCGWindowLayer");

        let mut front_wid: u32 = 0;
        let mut front_bounds = CGRect::default();
        let mut have_front_bounds = false;
        let mut fallback_wid: u32 = 0;
        for i in 0..count {
            let dict = CFArrayGetValueAtIndex(list, i);
            if dict.is_null() {
                continue;
            }

            let mut v: CFTypeRef = ptr::null();

            let mut layer: i32 = -1;
            if CFDictionaryGetValueIfPresent(dict, layer_key as CFTypeRef, &mut v) {
                CFNumberGetValue(v, kCFNumberSInt32Type, &mut layer as *mut _ as *mut _);
            }
            if layer != 0 {
                continue;
            }

            let mut pid: i32 = 0;
            if CFDictionaryGetValueIfPresent(dict, pid_key as CFTypeRef, &mut v) {
                CFNumberGetValue(v, kCFNumberSInt32Type, &mut pid as *mut _ as *mut _);
            }
            if pid == own_pid {
                continue;
            }

            let mut wid: u32 = 0;
            if CFDictionaryGetValueIfPresent(dict, wid_key as CFTypeRef, &mut v) {
                CFNumberGetValue(v, kCFNumberSInt32Type, &mut wid as *mut _ as *mut _);
            }
            if wid == 0 {
                continue;
            }

            if !is_suitable_window(SLSMainConnectionID(), wid) {
                continue;
            }

            // Track first non-self window as fallback (z-order based)
            if fallback_wid == 0 {
                fallback_wid = wid;
            }

            // Prefer a window from the front process. If another layer-0 surface
            // from that app nearly fully contains the current one, treat the
            // larger surface as the real window. Firefox can surface a tab-strip
            // child ahead of the outer window after a tile.
            if pid == front_pid {
                let mut bounds = CGRect::default();
                if SLSGetWindowBounds(SLSMainConnectionID(), wid, &mut bounds) != kCGErrorSuccess {
                    if front_wid == 0 {
                        front_wid = wid;
                    }
                    continue;
                }

                if front_wid == 0 {
                    front_wid = wid;
                    front_bounds = bounds;
                    have_front_bounds = true;
                    continue;
                }

                if have_front_bounds
                    && is_same_window_surface(front_bounds, bounds)
                    && window_area(bounds) > window_area(front_bounds)
                {
                    front_wid = wid;
                    front_bounds = bounds;
                }
            }
        }

        // Fall back to z-order if front process has no visible windows
        // (e.g., switched to a workspace where the front app has no windows)
        if front_wid == 0 {
            front_wid = fallback_wid;
        }

        CFRelease(wid_key as CFTypeRef);
        CFRelease(pid_key as CFTypeRef);
        CFRelease(layer_key as CFTypeRef);
        CFRelease(list);
        front_wid
    }
}

/// Parse hex color string (#RRGGBB or #RRGGBBAA) to (r, g, b, a) floats.
fn parse_color(s: &str) -> Option<(f64, f64, f64, f64)> {
    let hex = s.strip_prefix('#').unwrap_or(s);
    if hex.len() != 6 && hex.len() != 8 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()? as f64 / 255.0;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()? as f64 / 255.0;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()? as f64 / 255.0;
    let a = if hex.len() == 8 {
        u8::from_str_radix(&hex[6..8], 16).ok()? as f64 / 255.0
    } else {
        1.0
    };
    Some((r, g, b, a))
}

fn flag_value<'a>(args: &'a [String], flags: &[&str]) -> Option<&'a str> {
    args.iter()
        .position(|s| flags.iter().any(|f| s == f))
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
}

fn print_help() {
    eprintln!("ers — window border renderer for tarmac");
    eprintln!();
    eprintln!("USAGE: ers [OPTIONS] [WINDOW_ID]");
    eprintln!();
    eprintln!("OPTIONS:");
    eprintln!("  -w, --width <PX>       Border width in pixels (default: 4.0)");
    eprintln!("  -r, --radius <PX>      Corner radius (default: 10.0)");
    eprintln!("  -c, --color <HEX>      Active border color (default: #5294e2)");
    eprintln!("  -i, --inactive <HEX>   Inactive border color (default: #59595980)");
    eprintln!("      --active-only      Only show border on focused window");
    eprintln!("      --list             List on-screen windows and exit");
    eprintln!("  -h, --help             Show this help");
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|s| s == "--help" || s == "-h") {
        print_help();
        return;
    }

    if args.get(1).is_some_and(|s| s == "--list") {
        list_windows();
        return;
    }

    let border_width: f64 = flag_value(&args, &["--width", "-w"])
        .and_then(|v| v.parse().ok())
        .unwrap_or(4.0);

    let radius: f64 = flag_value(&args, &["--radius", "-r"])
        .and_then(|v| v.parse().ok())
        .unwrap_or(10.0);

    let active_color = flag_value(&args, &["--color", "-c"])
        .and_then(parse_color)
        .unwrap_or((0.32, 0.58, 0.89, 1.0));

    let inactive_color = flag_value(&args, &["--inactive", "-i"])
        .and_then(parse_color)
        .unwrap_or((0.35, 0.35, 0.35, 0.8));

    let active_only = args.iter().any(|s| s == "--active-only");

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
    borders.radius = radius;
    borders.active_color = active_color;
    borders.inactive_color = inactive_color;
    borders.active_only = active_only;

    if let Some(target) = args.get(1).and_then(|s| s.parse::<u32>().ok()) {
        borders.add_batch(target);
    } else {
        let wids = discover_windows(cid, own_pid);
        for &wid in &wids {
            borders.add_batch(wid);
        }
    }

    borders.subscribe_all();

    borders.update_focus();

    if borders.active_only {
        let focused = borders.focused_wid;
        let to_hide: Vec<u32> = borders
            .overlays
            .keys()
            .filter(|&&wid| wid != focused)
            .copied()
            .collect();
        for wid in to_hide {
            borders.hide(wid);
        }
    }

    debug!("{} overlays tracked", borders.overlays.len());

    SIGNAL_STOP_REQUESTED.store(false, Ordering::Relaxed);

    // Background watcher translates the signal-safe atomic into a normal
    // CoreFoundation shutdown request on a Rust thread.
    let running = Arc::new(AtomicBool::new(true));
    let signal_watcher = std::thread::spawn(|| {
        use std::time::Duration;

        while !SIGNAL_STOP_REQUESTED.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(10));
        }

        unsafe {
            let run_loop = CFRunLoopGetMain();
            CFRunLoopStop(run_loop);
            CFRunLoopWakeUp(run_loop);
        }
    });

    unsafe {
        libc::signal(
            libc::SIGINT,
            handle_sigint as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            handle_sigint as *const () as libc::sighandler_t,
        );
    }

    // Process events on background thread with coalescing
    let running_bg = Arc::clone(&running);
    let handle = std::thread::spawn(move || {
        use std::collections::HashSet;
        use std::time::{Duration, Instant};

        // Persist across batches: windows we know about but haven't bordered yet.
        // Value is the time the window was first seen — only promote after 100ms
        // so tarmac has time to position them.
        let mut pending: HashMap<u32, Instant> = HashMap::new();

        while running_bg.load(Ordering::Relaxed) {
            let first = match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(e) => e,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };

            std::thread::sleep(std::time::Duration::from_millis(16));

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
                            pending.entry(wid).or_insert_with(Instant::now);
                            borders.subscribe_target(wid);
                        }
                    }
                    Event::Hide(wid) => borders.hide(wid),
                    Event::Unhide(wid) => {
                        if !borders.active_only || wid == borders.focused_wid {
                            borders.unhide(wid);
                        }
                    }
                    Event::FrontChange => {
                        needs_resubscribe = true;
                    }
                    Event::SpaceChange => {
                        needs_resubscribe = true;
                    }
                }
            }

            // Destroys
            for wid in &destroyed {
                borders.remove(*wid);
            }

            // Promote pending creates that have waited ≥100ms (tarmac positioning time)
            let now = Instant::now();
            let ready: Vec<u32> = pending
                .iter()
                .filter(|(wid, seen_at)| {
                    !destroyed.contains(wid)
                        && now.duration_since(**seen_at) >= Duration::from_millis(100)
                })
                .map(|(wid, _)| *wid)
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
                for j in (i + 1)..bounds_map.len() {
                    let (wid_a, a) = &bounds_map[i];
                    let (wid_b, b) = &bounds_map[j];
                    if let Some(preference) = surface_preference(*a, *b) {
                        match preference {
                            SurfacePreference::KeepExisting => {
                                skip.insert(*wid_b);
                            }
                            SurfacePreference::ReplaceExisting => {
                                skip.insert(*wid_a);
                            }
                        }
                    }
                }
            }

            for &wid in &ready {
                pending.remove(&wid);
                if !skip.contains(&wid) {
                    borders.add_fresh(wid);
                    if borders.active_only && wid != borders.focused_wid {
                        borders.hide(wid);
                    }
                    needs_resubscribe = true;
                }
            }

            // Moves: reposition overlay (no destroy/create)
            for wid in &moved {
                if !resized.contains(wid) && !ready.contains(wid) && borders.sync_overlay(*wid) {
                    needs_resubscribe = true;
                }
            }

            // Resizes: must recreate (can't reshape windows on Tahoe)
            // Skip windows just created this batch — already at correct size
            for wid in &resized {
                if !ready.contains(wid)
                    && borders.overlays.contains_key(wid)
                    && borders.sync_overlay(*wid)
                {
                    needs_resubscribe = true;
                }
            }

            // On space change, discover windows we haven't seen yet
            if needs_resubscribe {
                borders.discover_untracked();
            }

            needs_resubscribe |= borders.reconcile_tracked();

            // Update focus (redraws borders in-place if changed)
            borders.update_focus();

            // Re-subscribe ALL tracked windows (SLSRequestNotificationsForWindows replaces, not appends)
            if needs_resubscribe || !destroyed.is_empty() {
                borders.subscribe_all();
            }

            // After all processing, enforce active-only visibility
            borders.enforce_active_only();
        }

        // Clean up all overlays before exiting
        borders.remove_all();
    });

    unsafe { CFRunLoopRun() };

    // SIGINT received — signal background thread to stop and wait
    running.store(false, Ordering::Relaxed);
    SIGNAL_STOP_REQUESTED.store(true, Ordering::Relaxed);
    let _ = signal_watcher.join();
    let _ = handle.join();
}

fn setup_event_port(cid: CGSConnectionID) {
    unsafe {
        let mut port: u32 = 0;
        if SLSGetEventPort(cid, &mut port) != kCGErrorSuccess {
            return;
        }
        let cf_port = CFMachPortCreateWithPort(
            ptr::null(),
            port,
            drain_events as *const _,
            ptr::null(),
            false,
        );
        if cf_port.is_null() {
            return;
        }
        _CFMachPortSetOptions(cf_port, 0x40);
        let source = CFMachPortCreateRunLoopSource(ptr::null(), cf_port, 0);
        if !source.is_null() {
            CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode);
            CFRelease(source);
        }
        CFRelease(cf_port);
    }
}

unsafe extern "C" fn drain_events(
    _: CFMachPortRef,
    _: *mut std::ffi::c_void,
    _: i64,
    _: *mut std::ffi::c_void,
) {
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
        if list.is_null() {
            return vec![];
        }

        let count = CFArrayGetCount(list);
        let wid_key = cf_string_from_static(c"kCGWindowNumber");
        let pid_key = cf_string_from_static(c"kCGWindowOwnerPID");
        let layer_key = cf_string_from_static(c"kCGWindowLayer");

        let mut wids = Vec::new();
        for i in 0..count {
            let dict = CFArrayGetValueAtIndex(list, i);
            if dict.is_null() {
                continue;
            }

            let mut v: CFTypeRef = ptr::null();
            let mut wid: u32 = 0;
            if CFDictionaryGetValueIfPresent(dict, wid_key as CFTypeRef, &mut v) {
                CFNumberGetValue(v, kCFNumberSInt32Type, &mut wid as *mut _ as *mut _);
            }
            if wid == 0 {
                continue;
            }

            let mut pid: i32 = 0;
            if CFDictionaryGetValueIfPresent(dict, pid_key as CFTypeRef, &mut v) {
                CFNumberGetValue(v, kCFNumberSInt32Type, &mut pid as *mut _ as *mut _);
            }
            if pid == own_pid {
                continue;
            }

            if !is_suitable_window(cid, wid) {
                continue;
            }

            let mut layer: i32 = -1;
            if CFDictionaryGetValueIfPresent(dict, layer_key as CFTypeRef, &mut v) {
                CFNumberGetValue(v, kCFNumberSInt32Type, &mut layer as *mut _ as *mut _);
            }
            if layer != 0 {
                continue;
            }

            wids.push(wid);
        }

        CFRelease(wid_key as CFTypeRef);
        CFRelease(pid_key as CFTypeRef);
        CFRelease(layer_key as CFTypeRef);
        CFRelease(list);
        wids
    }
}

/// Draw a border ring into an existing CGContext, clearing first.
fn draw_border(
    ctx: CGContextRef,
    width: f64,
    height: f64,
    border_width: f64,
    radius: f64,
    color: (f64, f64, f64, f64),
) {
    unsafe {
        let full = CGRect::new(0.0, 0.0, width, height);
        CGContextClearRect(ctx, full);

        let bw = border_width;
        let stroke_rect = CGRect::new(bw / 2.0, bw / 2.0, width - bw, height - bw);
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
    }
}

fn create_overlay(
    cid: CGSConnectionID,
    target_wid: u32,
    border_width: f64,
    radius: f64,
    color: (f64, f64, f64, f64),
) -> Option<(CGSConnectionID, u32, CGRect, f64)> {
    unsafe {
        let mut bounds = CGRect::default();
        let rc = SLSGetWindowBounds(cid, target_wid, &mut bounds);
        if rc != kCGErrorSuccess {
            debug!("[create_overlay] SLSGetWindowBounds failed for wid={target_wid} rc={rc}");
            return None;
        }
        if !is_trackable_window(bounds, border_width) {
            debug!(
                "[create_overlay] wid={target_wid} too small: {}x{}",
                bounds.size.width, bounds.size.height
            );
            return None;
        }

        let bw = border_width;
        let ow = bounds.size.width + 2.0 * bw;
        let oh = bounds.size.height + 2.0 * bw;
        let ox = bounds.origin.x - bw;
        let oy = bounds.origin.y - bw;
        let scale = display_scale_for_bounds(bounds);

        let frame = CGRect::new(0.0, 0.0, ow, oh);
        let mut region: CFTypeRef = ptr::null();
        CGSNewRegionWithRect(&frame, &mut region);
        if region.is_null() {
            debug!("[create_overlay] CGSNewRegionWithRect failed for wid={target_wid}");
            return None;
        }

        let mut wid: u32 = 0;
        SLSNewWindow(cid, 2, ox as f32, oy as f32, region, &mut wid);
        CFRelease(region);
        if wid == 0 {
            debug!("[create_overlay] SLSNewWindow returned 0 for target={target_wid} cid={cid}");
            return None;
        }

        debug!(
            "[create_overlay] created overlay wid={wid} for target={target_wid} scale={scale:.2} color=({:.2},{:.2},{:.2},{:.2})",
            color.0, color.1, color.2, color.3
        );

        SLSSetWindowResolution(cid, wid, scale);
        SLSSetWindowOpacity(cid, wid, false);
        SLSSetWindowLevel(cid, wid, 0);
        SLSOrderWindow(cid, wid, 1, target_wid);

        // Draw border (point coordinates)
        let ctx = SLWindowContextCreate(cid, wid, ptr::null());
        if ctx.is_null() {
            debug!("[create_overlay] SLWindowContextCreate returned null for overlay wid={wid}");
            SLSReleaseWindow(cid, wid);
            return None;
        }

        draw_border(ctx, ow, oh, bw, radius, color);
        SLSFlushWindowContentRegion(cid, wid, ptr::null());
        CGContextRelease(ctx);

        // Click-through. Setting an empty event/hit-test shape makes mouse
        // events pass through the overlay to the window beneath. We use
        // SLSSetWindowEventShape rather than SLSSetWindowTags(kCGSIgnoreForEvents)
        // because tag mutation in the event-driven recreate path poisons
        // subsequent SLSNewWindow calls on the shared connection.
        // SLSSetWindowEventMask alone was insufficient on Tahoe.
        let empty = CGRect::new(0.0, 0.0, 0.0, 0.0);
        let mut empty_region: CFTypeRef = ptr::null();
        if CGSNewRegionWithRect(&empty, &mut empty_region) == kCGErrorSuccess
            && !empty_region.is_null()
        {
            SLSSetWindowEventShape(cid, wid, empty_region);
            CFRelease(empty_region);
        }
        SLSSetWindowEventMask(cid, wid, 0);

        // Capture-exclusion advisories. Tahoe's screen-capture picker
        // ignores both for SLS overlays but they're harmless and may help
        // capture clients that do honor them.
        SLSSetWindowSharingState(cid, wid, 0);
        SLSSetWindowClientPerceivedType(cid, wid, 2);

        // Replace the window's bounds region with a donut path: outer
        // rect plus an inner subpath for the interior. SLS interprets
        // the path with even-odd winding so the interior is *not* part
        // of the window — the screenshot picker hit-tests through to
        // the underlying app window. Done via SLSTransaction because
        // SLSTransactionSetWindowBoundsPath is the only path-based bounds
        // API exported on Tahoe; CGS region union APIs aren't linkable.
        let interior = CGRect::new(bw, bw, (ow - 2.0 * bw).max(0.0), (oh - 2.0 * bw).max(0.0));
        if interior.size.width > 0.0 && interior.size.height > 0.0 {
            let path = CGPathCreateMutable();
            if !path.is_null() {
                let outer = CGRect::new(0.0, 0.0, ow, oh);
                CGPathAddRect(path, ptr::null(), outer);
                CGPathAddRect(path, ptr::null(), interior);
                let txn = SLSTransactionCreate(cid);
                if !txn.is_null() {
                    SLSTransactionSetWindowBoundsPath(txn, cid, wid, path as CGPathRef);
                    SLSTransactionCommit(txn, 1);
                    CFRelease(txn);
                }
                CGPathRelease(path as CGPathRef);
            }
        }

        Some((cid, wid, bounds, scale))
    }
}

fn list_windows() {
    let cid = unsafe { SLSMainConnectionID() };
    unsafe {
        let list = CGWindowListCopyWindowInfo(kCGWindowListOptionOnScreenOnly, kCGNullWindowID);
        if list.is_null() {
            return;
        }
        let count = CFArrayGetCount(list);
        let wid_key = cf_string_from_static(c"kCGWindowNumber");
        let layer_key = cf_string_from_static(c"kCGWindowLayer");

        eprintln!(
            "{:>6}  {:>8}  {:>8}  {:>6}  {:>6}",
            "wid", "x", "y", "w", "h"
        );
        for i in 0..count {
            let dict = CFArrayGetValueAtIndex(list, i);
            if dict.is_null() {
                continue;
            }

            let mut v: CFTypeRef = ptr::null();
            let mut wid: u32 = 0;
            let mut layer: i32 = -1;
            if CFDictionaryGetValueIfPresent(dict, wid_key as CFTypeRef, &mut v) {
                CFNumberGetValue(v, kCFNumberSInt32Type, &mut wid as *mut _ as *mut _);
            }
            if CFDictionaryGetValueIfPresent(dict, layer_key as CFTypeRef, &mut v) {
                CFNumberGetValue(v, kCFNumberSInt32Type, &mut layer as *mut _ as *mut _);
            }
            if layer != 0 || wid == 0 {
                continue;
            }

            let mut bounds = CGRect::default();
            SLSGetWindowBounds(cid, wid, &mut bounds);
            eprintln!(
                "{wid:>6}  {:>8.0}  {:>8.0}  {:>6.0}  {:>6.0}",
                bounds.origin.x, bounds.origin.y, bounds.size.width, bounds.size.height
            );
        }
        CFRelease(wid_key as CFTypeRef);
        CFRelease(layer_key as CFTypeRef);
        CFRelease(list);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CGRect, SurfacePreference, WindowMetadata, intersection_area, is_same_window_surface,
        is_suitable_window_metadata, is_trackable_window, surface_preference,
    };

    #[test]
    fn same_surface_detects_contained_strip() {
        let outer = CGRect::new(100.0, 100.0, 1200.0, 900.0);
        let strip = CGRect::new(114.0, 105.0, 1160.0, 140.0);
        assert!(is_same_window_surface(outer, strip));
    }

    #[test]
    fn different_windows_are_not_treated_as_one_surface() {
        let a = CGRect::new(100.0, 100.0, 1200.0, 900.0);
        let b = CGRect::new(300.0, 300.0, 1160.0, 140.0);
        assert!(!is_same_window_surface(a, b));
    }

    #[test]
    fn intersection_area_is_zero_without_overlap() {
        let a = CGRect::new(100.0, 100.0, 200.0, 200.0);
        let b = CGRect::new(400.0, 400.0, 200.0, 200.0);
        assert_eq!(intersection_area(a, b), 0.0);
    }

    #[test]
    fn same_surface_prefers_larger_bounds() {
        let strip = CGRect::new(114.0, 105.0, 1160.0, 140.0);
        let outer = CGRect::new(100.0, 100.0, 1200.0, 900.0);
        assert_eq!(
            surface_preference(strip, outer),
            Some(SurfacePreference::ReplaceExisting)
        );
    }

    #[test]
    fn small_windows_remain_trackable() {
        let small = CGRect::new(100.0, 100.0, 12.0, 18.0);
        assert!(is_trackable_window(small, 4.0));
    }

    #[test]
    fn suitable_window_metadata_matches_document_windows() {
        let metadata = WindowMetadata {
            parent_wid: 0,
            tags: super::WINDOW_TAG_DOCUMENT,
            attributes: super::WINDOW_ATTRIBUTE_REAL,
        };
        assert!(is_suitable_window_metadata(metadata));
    }

    #[test]
    fn attached_windows_are_not_suitable_targets() {
        let metadata = WindowMetadata {
            parent_wid: 7,
            tags: super::WINDOW_TAG_DOCUMENT | super::WINDOW_TAG_ATTACHED,
            attributes: super::WINDOW_ATTRIBUTE_REAL,
        };
        assert!(!is_suitable_window_metadata(metadata));
    }
}
