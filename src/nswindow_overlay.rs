//! NSWindow-backed overlay border.
//!
//! Replaces the SLS-only window approach. The reason is screenshot
//! exclusion on macOS Tahoe: `screencaptureui` enumerates windows via
//! `_SLSCopyWindowsWithOptionsAndTagsAndSpaceOptions` +
//! `_CGSGetWindowTags` and ignores the sharing-state of raw SLS-only
//! windows. NSWindow.sharingType = .none is the only documented and
//! verified-honored exclusion mechanism (verified empirically on Tahoe
//! with `screencapture -l <wid>`: SLS overlays capture, NSWindows with
//! `.none` sharingType return "could not create image from window").
//!
//! We use a CAShapeLayer for the rounded-rect border so updates stay
//! declarative — no NSView subclassing required.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{ClassType, MainThreadMarker, MainThreadOnly, msg_send};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSColor, NSScreen, NSWindow,
    NSWindowCollectionBehavior, NSWindowOrderingMode, NSWindowSharingType, NSWindowStyleMask,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_quartz_core::{CALayer, CAShapeLayer};
use std::ptr;

const NS_FLOATING_WINDOW_LEVEL: isize = 3;

/// Top-left Y in CG global coordinates becomes bottom-left Y in Cocoa
/// global coordinates by subtracting from the primary screen height.
///
/// The "primary" screen is the one whose Cocoa frame origin is (0, 0) —
/// by definition the bottom-left of the menu-bar screen, which is the
/// shared anchor between CG (top-left) and Cocoa (bottom-left) global
/// coordinates. `[NSScreen screens][0]` is documented as the primary
/// but isn't reliable on every macOS version, so we look it up by
/// origin instead.
fn primary_screen_height(mtm: MainThreadMarker) -> f64 {
    let screens = NSScreen::screens(mtm);
    let count = screens.count();
    for i in 0..count {
        let s = screens.objectAtIndex(i);
        let f = s.frame();
        if f.origin.x.abs() < 0.5 && f.origin.y.abs() < 0.5 {
            return f.size.height;
        }
    }
    if count > 0 {
        screens.objectAtIndex(0).frame().size.height
    } else {
        0.0
    }
}

fn cg_to_cocoa_frame(cg: CGRect, mtm: MainThreadMarker) -> CGRect {
    let primary_height = primary_screen_height(mtm);
    let cocoa_y = primary_height - cg.origin.y - cg.size.height;
    CGRect::new(
        CGPoint::new(cg.origin.x, cocoa_y),
        CGSize::new(cg.size.width, cg.size.height),
    )
}

/// Log all NSScreens and which one we'll treat as primary. Helps diagnose
/// multi-monitor coordinate issues.
pub fn log_screens(mtm: MainThreadMarker) {
    let screens = NSScreen::screens(mtm);
    let primary_h = primary_screen_height(mtm);
    tracing::debug!(
        primary_height = primary_h,
        count = screens.count(),
        "NSScreen layout"
    );
    for i in 0..screens.count() {
        let s = screens.objectAtIndex(i);
        let f = s.frame();
        tracing::debug!(
            index = i,
            cocoa_x = f.origin.x,
            cocoa_y = f.origin.y,
            w = f.size.width,
            h = f.size.height,
            "screen"
        );
    }
}

/// Initialize NSApplication. Must be called once from the main thread.
pub fn init_application() -> MainThreadMarker {
    let mtm = MainThreadMarker::new().expect("init_application must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    mtm
}

/// One NSWindow + CAShapeLayer pair drawing a rounded-rect border.
///
/// `bounds_cg_*` fields are the TARGET window's CG bounds (origin
/// top-left, Y-down) — same coordinate system the rest of ers uses.
pub struct OverlayWindow {
    window: Retained<NSWindow>,
    border_layer: Retained<CAShapeLayer>,
    pub bounds_cg_x: f64,
    pub bounds_cg_y: f64,
    pub bounds_cg_w: f64,
    pub bounds_cg_h: f64,
    pub border_width: f64,
    pub radius: f64,
    mtm: MainThreadMarker,
}

impl OverlayWindow {
    /// Create an NSWindow border overlay around the given target bounds.
    /// Coords are in CG space (origin top-left, Y-down).
    pub fn new(
        bounds_cg_x: f64,
        bounds_cg_y: f64,
        bounds_cg_w: f64,
        bounds_cg_h: f64,
        border_width: f64,
        radius: f64,
        color: (f64, f64, f64, f64),
        mtm: MainThreadMarker,
    ) -> Option<Self> {
        let outer_cg = CGRect::new(
            CGPoint::new(bounds_cg_x - border_width, bounds_cg_y - border_width),
            CGSize::new(
                bounds_cg_w + 2.0 * border_width,
                bounds_cg_h + 2.0 * border_width,
            ),
        );
        let cocoa_frame = cg_to_cocoa_frame(outer_cg, mtm);

        let style = NSWindowStyleMask::Borderless;
        let window: Retained<NSWindow> = unsafe {
            msg_send![
                NSWindow::alloc(mtm),
                initWithContentRect: cocoa_frame,
                styleMask: style,
                backing: NSBackingStoreType::Buffered,
                defer: false
            ]
        };
        window.setOpaque(false);
        window.setHasShadow(false);
        window.setIgnoresMouseEvents(true);
        window.setLevel(NS_FLOATING_WINDOW_LEVEL);
        unsafe { window.setReleasedWhenClosed(false) };
        window.setSharingType(NSWindowSharingType::None);
        // Do NOT set CanJoinAllSpaces: that would draw the overlay on
        // every macOS space simultaneously. tarmac's workspaces are
        // not macOS spaces, but if the user has both, leaking onto
        // every space looks like a "stuck border" bug.
        window.setCollectionBehavior(
            NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle
                | NSWindowCollectionBehavior::FullScreenAuxiliary,
        );
        // Clear background.
        let clear = unsafe { NSColor::clearColor() };
        unsafe { window.setBackgroundColor(Some(&clear)) };

        let content_view = window.contentView()?;
        content_view.setWantsLayer(true);
        let host_layer: Retained<CALayer> = unsafe {
            let layer: Option<Retained<CALayer>> = msg_send![&*content_view, layer];
            layer?
        };

        let border_layer = unsafe { CAShapeLayer::new() };
        let path_rect = inset_for_stroke(outer_cg.size, border_width);
        unsafe {
            let path = objc2_core_graphics::CGPath::with_rounded_rect(
                path_rect, radius, radius, ptr::null(),
            );
            let path_ref: *mut AnyObject =
                objc2_core_foundation::CFRetained::as_ptr(&path).as_ptr() as *mut AnyObject;
            let _: () = msg_send![&*border_layer, setPath: path_ref];

            let _: () = msg_send![&*border_layer, setFillColor: ptr::null::<AnyObject>()];
            let stroke = make_cgcolor(color, mtm);
            let stroke_ref: *mut AnyObject =
                objc2_core_foundation::CFRetained::as_ptr(&stroke).as_ptr() as *mut AnyObject;
            let _: () = msg_send![&*border_layer, setStrokeColor: stroke_ref];
            border_layer.setLineWidth(border_width);
            border_layer.setFrame(CGRect::new(
                CGPoint::new(0.0, 0.0),
                CGSize::new(outer_cg.size.width, outer_cg.size.height),
            ));
            host_layer.addSublayer(&border_layer);
        }

        window.orderFrontRegardless();

        Some(OverlayWindow {
            window,
            border_layer,
            bounds_cg_x,
            bounds_cg_y,
            bounds_cg_w,
            bounds_cg_h,
            border_width,
            radius,
            mtm,
        })
    }

    /// NSWindow's windowNumber, usable as a wid for tracking.
    pub fn wid(&self) -> u32 {
        self.window.windowNumber() as u32
    }

    pub fn set_bounds(&mut self, x: f64, y: f64, w: f64, h: f64) {
        let outer_cg = CGRect::new(
            CGPoint::new(x - self.border_width, y - self.border_width),
            CGSize::new(w + 2.0 * self.border_width, h + 2.0 * self.border_width),
        );
        let cocoa_frame = cg_to_cocoa_frame(outer_cg, self.mtm);
        tracing::debug!(
            cg_x = outer_cg.origin.x,
            cg_y = outer_cg.origin.y,
            cg_w = outer_cg.size.width,
            cg_h = outer_cg.size.height,
            cocoa_x = cocoa_frame.origin.x,
            cocoa_y = cocoa_frame.origin.y,
            cocoa_w = cocoa_frame.size.width,
            cocoa_h = cocoa_frame.size.height,
            "set_bounds"
        );
        self.window.setFrame_display(cocoa_frame, true);
        // Update the border path to match new size.
        unsafe {
            let path = objc2_core_graphics::CGPath::with_rounded_rect(
                inset_for_stroke(outer_cg.size, self.border_width),
                self.radius,
                self.radius,
                ptr::null(),
            );
            let path_ref: *mut AnyObject =
                objc2_core_foundation::CFRetained::as_ptr(&path).as_ptr() as *mut AnyObject;
            let _: () = msg_send![&*self.border_layer, setPath: path_ref];
            self.border_layer.setFrame(CGRect::new(
                CGPoint::new(0.0, 0.0),
                CGSize::new(outer_cg.size.width, outer_cg.size.height),
            ));
        }
        self.bounds_cg_x = x;
        self.bounds_cg_y = y;
        self.bounds_cg_w = w;
        self.bounds_cg_h = h;
    }

    pub fn set_color(&self, color: (f64, f64, f64, f64)) {
        unsafe {
            let stroke = make_cgcolor(color, self.mtm);
            let stroke_ref: *mut AnyObject =
                objc2_core_foundation::CFRetained::as_ptr(&stroke).as_ptr() as *mut AnyObject;
            let _: () = msg_send![&*self.border_layer, setStrokeColor: stroke_ref];
        }
    }

    pub fn order_above(&self, target_wid: u32) {
        self.window
            .orderWindow_relativeTo(NSWindowOrderingMode::Above, target_wid as isize);
    }

    pub fn order_out(&self) {
        self.window.orderOut(None);
    }

    pub fn set_alpha(&self, alpha: f64) {
        self.window.setAlphaValue(alpha);
    }
}

impl Drop for OverlayWindow {
    fn drop(&mut self) {
        // orderOut first so the visual disappears synchronously;
        // close() afterward releases the window. Without orderOut a
        // closed-but-still-onscreen window can briefly linger on
        // Tahoe before Retained drops the last ref.
        self.window.orderOut(None);
        self.window.close();
    }
}

fn inset_for_stroke(size: CGSize, border_width: f64) -> CGRect {
    // CAShapeLayer strokes centered on the path. To get an exactly
    // border_width-thick visible ring sitting inside the layer bounds,
    // inset the path by half the line width and stroke at line_width
    // = border_width.
    let half = border_width / 2.0;
    CGRect::new(
        CGPoint::new(half, half),
        CGSize::new(
            (size.width - 2.0 * half).max(0.0),
            (size.height - 2.0 * half).max(0.0),
        ),
    )
}

fn make_cgcolor(
    rgba: (f64, f64, f64, f64),
    _mtm: MainThreadMarker,
) -> objc2_core_foundation::CFRetained<objc2_core_graphics::CGColor> {
    unsafe { objc2_core_graphics::CGColor::new_srgb(rgba.0, rgba.1, rgba.2, rgba.3) }
}
