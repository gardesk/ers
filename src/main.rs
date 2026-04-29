//! ers — window border renderer

mod events;
mod skylight;
mod nswindow_overlay;

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

/// Per-overlay state: an NSWindow drawing the rounded-rect border via
/// CAShapeLayer. Replaces the old SLS-only overlay window — see
/// nswindow_overlay.rs for the rationale (screencaptureui on Tahoe
/// only honors NSWindow.sharingType, not SLS sharing-state nor tag
/// bits, for raw SLS-only windows).
struct Overlay {
    window: nswindow_overlay::OverlayWindow,
}

impl Overlay {
    fn wid(&self) -> u32 {
        self.window.wid()
    }
    fn bounds(&self) -> CGRect {
        CGRect {
            origin: CGPoint {
                x: self.window.bounds_cg_x,
                y: self.window.bounds_cg_y,
            },
            size: CGSize {
                width: self.window.bounds_cg_w,
                height: self.window.bounds_cg_h,
            },
        }
    }
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
    mtm: objc2::MainThreadMarker,
}

impl BorderMap {
    fn new(
        cid: CGSConnectionID,
        own_pid: i32,
        border_width: f64,
        mtm: objc2::MainThreadMarker,
    ) -> Self {
        Self {
            overlays: HashMap::new(),
            mtm,
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
        self.overlays.values().any(|o| o.wid() == wid)
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
        if let Some(window) = nswindow_overlay::OverlayWindow::new(
            bounds.origin.x,
            bounds.origin.y,
            bounds.size.width,
            bounds.size.height,
            self.border_width,
            self.radius,
            color,
            self.mtm,
        ) {
            window.order_above(target_wid);
            self.overlays.insert(target_wid, Overlay { window });
        }
    }

    fn remove_all(&mut self) {
        // OverlayWindow's Drop closes the NSWindow.
        self.overlays.clear();
    }

    fn remove(&mut self, target_wid: u32) {
        if let Some(overlay) = self.overlays.remove(&target_wid) {
            debug!(
                "[remove] target={} overlay_wid={} dropping NSWindow",
                target_wid,
                overlay.wid()
            );
            // OverlayWindow's Drop runs orderOut + close.
            drop(overlay);
        } else {
            debug!("[remove] target={} not tracked", target_wid);
        }
    }

    /// Reconcile a tracked overlay against its target window.
    fn sync_overlay(&mut self, target_wid: u32) -> bool {
        if !self.overlays.contains_key(&target_wid) {
            return false;
        }

        let mut bounds = CGRect::default();
        unsafe {
            if SLSGetWindowBounds(self.main_cid, target_wid, &mut bounds) != kCGErrorSuccess {
                // Window is gone (destroyed). Reap the overlay.
                debug!(
                    "[sync_overlay] target={} SLSGetWindowBounds failed — reaping overlay",
                    target_wid
                );
                self.remove(target_wid);
                return true;
            }

            if !is_suitable_window(self.main_cid, target_wid) {
                self.remove(target_wid);
                return true;
            }

            if !is_trackable_window(bounds, self.border_width) {
                self.remove(target_wid);
                return true;
            }
        }

        if let Some(overlay) = self.overlays.get_mut(&target_wid) {
            let prev = overlay.bounds();
            if size_changed(prev, bounds) || origin_changed(prev, bounds) {
                debug!(
                    "[sync_overlay] target={} geometry ({:.1},{:.1},{:.1},{:.1}) -> ({:.1},{:.1},{:.1},{:.1})",
                    target_wid,
                    prev.origin.x,
                    prev.origin.y,
                    prev.size.width,
                    prev.size.height,
                    bounds.origin.x,
                    bounds.origin.y,
                    bounds.size.width,
                    bounds.size.height
                );
                overlay.window.set_bounds(
                    bounds.origin.x,
                    bounds.origin.y,
                    bounds.size.width,
                    bounds.size.height,
                );
                overlay.window.order_above(target_wid);
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

    /// With NSWindow.setFrame_display we no longer need a destroy-and-
    /// recreate path on resize. Kept as a thin alias so existing call
    /// sites keep working.
    fn recreate(&mut self, target_wid: u32) {
        self.sync_overlay(target_wid);
    }

    fn hide(&self, target_wid: u32) {
        if let Some(o) = self.overlays.get(&target_wid) {
            debug!("[hide] target={} overlay_wid={}", target_wid, o.wid());
            o.window.order_out();
        }
    }

    fn unhide(&self, target_wid: u32) {
        if let Some(o) = self.overlays.get(&target_wid) {
            debug!("[unhide] target={} overlay_wid={}", target_wid, o.wid());
            o.window.order_above(target_wid);
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
            overlay.window.set_color(self.color_for(target_wid));
        }
    }

    /// Detect focused window and update border colors if focus changed.
    fn update_focus(&mut self) {
        let front = get_front_window(self.own_pid);
        if front == 0 {
            return;
        }
        if front == self.focused_wid {
            // Same focus as last poll. But a freshly-spawned window may
            // have been focused before its SLS state was complete enough
            // to pass the add_fresh filter — retry on every poll until
            // it sticks.
            if !self.overlays.contains_key(&front) {
                debug!("[focus-retry] front={} still untracked, retrying add_fresh", front);
                self.add_fresh(front);
                if self.overlays.contains_key(&front) {
                    self.subscribe_target(front);
                    if self.active_only {
                        self.unhide(front);
                    }
                }
            }
            return;
        }

        let old = self.focused_wid;
        self.focused_wid = front;

        // tarmac-style workspace switching can swap focus to a window
        // that wasn't visible (and therefore not discovered) at ers
        // startup. Discover_windows only enumerates on-current-space
        // windows; tarmac stages other workspaces' windows in a hidden
        // state ers never picked up. If focus lands on such a wid,
        // create an overlay for it on demand.
        let new_target = !self.overlays.contains_key(&front);
        debug!(
            "[focus] {} -> {} {}(tracked targets: {:?})",
            old,
            front,
            if new_target { "[NEW] " } else { "" },
            self.overlays.keys().collect::<Vec<_>>()
        );
        if new_target {
            self.add_fresh(front);
            self.subscribe_target(front);
        }

        // Pull both overlays' positions to the targets' current SLS bounds
        // before un/hiding. AX-driven moves during a stack cycle frequently
        // don't fire SLS WINDOW_MOVE notifications, so a stored overlay
        // can be at stale coordinates. SLSGetWindowBounds (inside
        // sync_overlay) is real-time and doesn't wait for a notification.
        self.sync_overlay(old);
        self.sync_overlay(front);

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
                o.window.order_above(target_wid);
            } else {
                o.window.order_out();
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
    // On the screenshot-exclusion research branch, default to file
    // logging at debug level so we can diagnose the NSWindow refactor
    // even when ers is spawned by tarmac (which inherits ers's stderr
    // to wherever tarmac was launched, often invisibly).
    let log_path = std::path::PathBuf::from("/tmp/ers-debug.log");
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .ok();
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("ers=debug"));
    if let Some(file) = log_file {
        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_writer(std::sync::Mutex::new(file))
            .with_ansi(false)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_writer(std::io::stderr)
            .init();
    }
    debug!("[main] ers starting, pid={}", std::process::id());

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

    // Initialize NSApplication on the main thread before we touch any
    // AppKit APIs. NSWindow operations (used by nswindow_overlay) all
    // require a main-thread context.
    let mtm = nswindow_overlay::init_application();

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
    let mut borders = BorderMap::new(cid, own_pid, border_width, mtm);
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

    // Process events on the main thread via a CFRunLoopTimer.
    // BorderMap holds Retained<NSWindow> handles, which are
    // !Send/!Sync — AppKit calls must originate from the main thread.
    // Stash state in thread_local for the C callback to access.
    MAIN_STATE.with(|cell| {
        *cell.borrow_mut() = Some(MainState {
            borders,
            rx,
            pending: HashMap::new(),
            batch_events: Vec::new(),
            batch_first_seen: None,
        });
    });

    unsafe {
        let mut ctx = CFRunLoopTimerContext {
            version: 0,
            info: ptr::null_mut(),
            retain: None,
            release: None,
            copy_description: None,
        };
        let timer = CFRunLoopTimerCreate(
            ptr::null(),
            CFAbsoluteTimeGetCurrent() + 0.05,
            0.016,
            0u64,
            0i64,
            timer_callback,
            &mut ctx,
        );
        CFRunLoopAddTimer(CFRunLoopGetMain(), timer, kCFRunLoopDefaultMode);
    }

    unsafe { CFRunLoopRun() };

    // Drop everything on the main thread (NSWindow.close in Drop).
    MAIN_STATE.with(|cell| cell.borrow_mut().take());

    SIGNAL_STOP_REQUESTED.store(true, Ordering::Relaxed);
    let _ = signal_watcher.join();
    drop(running);
}

struct MainState {
    borders: BorderMap,
    rx: mpsc::Receiver<Event>,
    pending: HashMap<u32, std::time::Instant>,
    batch_events: Vec<Event>,
    batch_first_seen: Option<std::time::Instant>,
}

thread_local! {
    static MAIN_STATE: std::cell::RefCell<Option<MainState>> = const { std::cell::RefCell::new(None) };
}

extern "C" fn timer_callback(_timer: *mut std::ffi::c_void, _info: *mut std::ffi::c_void) {
    use std::time::{Duration, Instant};
    use std::sync::atomic::AtomicUsize;
    static TICK_COUNT: AtomicUsize = AtomicUsize::new(0);
    let tick = TICK_COUNT.fetch_add(1, Ordering::Relaxed);
    if tick == 0 {
        debug!("[timer] first fire — main-thread event loop is alive");
    } else if tick % 600 == 0 {
        // every ~10s if interval is 16ms
        debug!("[timer] tick {}", tick);
    }
    MAIN_STATE.with(|cell| {
        let mut state_opt = cell.borrow_mut();
        let s = match state_opt.as_mut() {
            Some(s) => s,
            None => return,
        };
        let mut received = 0usize;
        loop {
            match s.rx.try_recv() {
                Ok(e) => {
                    if s.batch_events.is_empty() {
                        s.batch_first_seen = Some(Instant::now());
                    }
                    s.batch_events.push(e);
                    received += 1;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }
        if received > 0 {
            debug!(
                "[timer] received {} new events; batch size now {}",
                received,
                s.batch_events.len()
            );
        }
        // Process the accumulated batch after a 16ms quiet window
        // (matches the old bg-thread behavior where it slept 16ms after
        // the first event then drained). Events keep arriving, the batch
        // grows; once 16ms passes without new events we flush.
        let should_flush = s.batch_first_seen.is_some_and(|t| {
            t.elapsed() >= Duration::from_millis(16) && received == 0
        }) || s
            .batch_first_seen
            .is_some_and(|t| t.elapsed() >= Duration::from_millis(120));
        if should_flush {
            let events = std::mem::take(&mut s.batch_events);
            s.batch_first_seen = None;
            debug!("[timer] processing batch of {}", events.len());
            process_event_batch(&mut s.borders, &mut s.pending, events);
        } else {
            // Even with no events, poll focus periodically so a missed
            // FrontChange notification doesn't strand the active border.
            // Cheap operation when focus hasn't changed.
            s.borders.update_focus();
            // Once per second, reconcile tracked overlays against
            // current SLS state. Catches missed Close/Destroy events
            // that would otherwise leave a dead border on screen.
            if tick % 60 == 0 && tick > 0 {
                let removed = s.borders.reconcile_tracked();
                if removed {
                    debug!("[timer] periodic reconcile removed stale overlays");
                }
            }
        }
    });
}

fn process_event_batch(
    borders: &mut BorderMap,
    pending: &mut HashMap<u32, std::time::Instant>,
    events: Vec<Event>,
) {
    use std::collections::HashSet;
    use std::time::{Duration, Instant};

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
                    debug!("[event] Close/Destroy target_wid={}", wid);
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
                if !borders.is_overlay(wid) {
                    if !borders.overlays.contains_key(&wid) {
                        borders.add_fresh(wid);
                        borders.subscribe_target(wid);
                    }
                    if !borders.active_only || wid == borders.focused_wid {
                        borders.unhide(wid);
                    }
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

    for wid in &destroyed {
        borders.remove(*wid);
    }

    let now = Instant::now();
    let ready: Vec<u32> = pending
        .iter()
        .filter(|(wid, seen_at)| {
            !destroyed.contains(wid) && now.duration_since(**seen_at) >= Duration::from_millis(100)
        })
        .map(|(wid, _)| *wid)
        .collect();

    let mut bounds_map: Vec<(u32, CGRect)> = Vec::new();
    for &wid in &ready {
        unsafe {
            let mut b = CGRect::default();
            SLSGetWindowBounds(borders.main_cid, wid, &mut b);
            bounds_map.push((wid, b));
        }
    }

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

    for wid in &moved {
        if !resized.contains(wid) && !ready.contains(wid) && borders.sync_overlay(*wid) {
            needs_resubscribe = true;
        }
    }

    for wid in &resized {
        if !ready.contains(wid)
            && borders.overlays.contains_key(wid)
            && borders.sync_overlay(*wid)
        {
            needs_resubscribe = true;
        }
    }

    if needs_resubscribe {
        borders.discover_untracked();
    }

    needs_resubscribe |= borders.reconcile_tracked();

    borders.update_focus();

    if needs_resubscribe || !destroyed.is_empty() {
        borders.subscribe_all();
    }

    borders.enforce_active_only();
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

/// Look up an overlay window in CGWindowListCopyWindowInfo and dump the
/// keys that the screenshot picker / ScreenCaptureKit care about. Lets
/// us tell whether SLSSetWindowSharingState(0) propagates through to
/// the CG window list (the layer SCWindow filters on) or stops at SLS.
fn probe_cg_window_info(target_wid: u32) {
    unsafe {
        let list = CGWindowListCopyWindowInfo(kCGWindowListOptionAll, kCGNullWindowID);
        if list.is_null() {
            debug!("[probe_cg_window_info] wid={target_wid} list is null");
            return;
        }
        let count = CFArrayGetCount(list);
        let wid_key = cf_string_from_static(c"kCGWindowNumber");
        let sharing_key = cf_string_from_static(c"kCGWindowSharingState");
        let layer_key = cf_string_from_static(c"kCGWindowLayer");
        let alpha_key = cf_string_from_static(c"kCGWindowAlpha");
        let on_screen_key = cf_string_from_static(c"kCGWindowIsOnscreen");
        let mut found = false;

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
            if wid != target_wid {
                continue;
            }

            let mut sharing: i32 = -1;
            if CFDictionaryGetValueIfPresent(dict, sharing_key as CFTypeRef, &mut v) {
                CFNumberGetValue(v, kCFNumberSInt32Type, &mut sharing as *mut _ as *mut _);
            }
            let mut layer: i32 = i32::MIN;
            if CFDictionaryGetValueIfPresent(dict, layer_key as CFTypeRef, &mut v) {
                CFNumberGetValue(v, kCFNumberSInt32Type, &mut layer as *mut _ as *mut _);
            }
            let mut alpha: f64 = -1.0;
            if CFDictionaryGetValueIfPresent(dict, alpha_key as CFTypeRef, &mut v) {
                CFNumberGetValue(v, 13 /* kCFNumberDoubleType */, &mut alpha as *mut _ as *mut _);
            }
            let on_screen_present =
                CFDictionaryGetValueIfPresent(dict, on_screen_key as CFTypeRef, &mut v);

            debug!(
                "[probe_cg_window_info] wid={target_wid} cg_sharing={sharing} layer={layer} alpha={alpha:.3} on_screen_present={on_screen_present}"
            );
            found = true;
            break;
        }

        if !found {
            debug!("[probe_cg_window_info] wid={target_wid} NOT FOUND in CGWindowList");
        }
        CFRelease(list as CFTypeRef);
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

        // Empty hit-test shape: an SLS window with an empty opaque_shape
        // is click-through at the compositor level (no input region).
        let empty = CGRect::new(0.0, 0.0, 0.0, 0.0);
        let mut empty_region: CFTypeRef = ptr::null();
        if CGSNewRegionWithRect(&empty, &mut empty_region) != kCGErrorSuccess
            || empty_region.is_null()
        {
            debug!("[create_overlay] CGSNewRegionWithRect (empty) failed for wid={target_wid}");
            CFRelease(region);
            return None;
        }

        // Create the overlay via SLSNewWindowWithOpaqueShapeAndContext
        // and bake tag bit 1 (click-through) and tag bit 9 (screenshot
        // exclusion) into the window at birth. Tahoe classifies windows
        // for capture/picker based on tags observed at creation time;
        // post-creation tag mutation lands too late and the picker keeps
        // including the overlay. Mirrors the JankyBorders unmanaged
        // create path (.refs/JankyBorders/src/misc/window.h:239).
        // options 13|(1<<18): documentation-window | ignores-cycle.
        let mut tags: u64 = (1u64 << 1) | (1u64 << 9);
        let mut wid: u32 = 0;
        SLSNewWindowWithOpaqueShapeAndContext(
            cid,
            2,
            region,
            empty_region,
            13 | (1 << 18),
            &mut tags as *mut u64,
            ox as f32,
            oy as f32,
            64,
            &mut wid,
            ptr::null_mut(),
        );
        CFRelease(region);
        CFRelease(empty_region);
        if wid == 0 {
            debug!(
                "[create_overlay] SLSNewWindowWithOpaqueShapeAndContext returned 0 for target={target_wid} cid={cid}"
            );
            return None;
        }

        debug!(
            "[create_overlay] created overlay wid={wid} for target={target_wid} scale={scale:.2} color=({:.2},{:.2},{:.2},{:.2})",
            color.0, color.1, color.2, color.3
        );

        if let Some(metadata) = query_window_metadata(cid, wid) {
            debug!(
                "[create_overlay] post-create overlay wid={wid} tags={:#x} attributes={:#x} parent={}",
                metadata.tags, metadata.attributes, metadata.parent_wid
            );
        } else {
            debug!("[create_overlay] post-create wid={wid} metadata query failed");
        }

        SLSSetWindowSharingState(cid, wid, 0);
        let mut sharing_state: u32 = u32::MAX;
        let rc = SLSGetWindowSharingState(cid, wid, &mut sharing_state);
        debug!("[create_overlay] sharing_state wid={wid} get_rc={rc} sls_state={sharing_state}");

        // Probe what CGWindowListCopyWindowInfo (which the screenshot
        // picker / SCWindow use) reports for our overlay. If
        // kCGWindowSharingState comes back != 0 here, then SLS-side
        // sharing state is not propagated to the CG window list and
        // we'll need a different exclusion mechanism.
        probe_cg_window_info(wid);

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

        // Post-creation tag mutation matching JankyBorders' pattern
        // at .refs/JankyBorders/src/misc/window.h:266-267. Verified
        // ineffective on Tahoe: tags set on windows owned by a
        // SLSNewConnection-created cid do NOT propagate to the global
        // server-side tag store, regardless of which cid issues the
        // SLSSetWindowTags call (tested both fresh and main cid).
        // The screencaptureui picker queries via _CGSGetWindowTags
        // from its own connection (otool confirmed) and reads 0x0 for
        // our overlays. Kept here aligned with JB so the diff is
        // legible; the actual fix requires creating overlays on the
        // process main cid (conflicts with the per-border fresh-cid
        // requirement in ers/CLAUDE.md) or backing them with NSWindow.
        let mut set_tags: u64 = (1u64 << 1) | (1u64 << 9);
        let mut clear_tags: u64 = 0;
        SLSSetWindowTags(cid, wid, &mut set_tags as *mut u64, 64);
        SLSClearWindowTags(cid, wid, &mut clear_tags as *mut u64, 64);

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
