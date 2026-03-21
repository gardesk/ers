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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("ers=info".parse().unwrap()),
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

    events::init(tx);
    events::register(cid);
    setup_event_port(cid);

    let mut tracker = WindowTracker::new(cid);

    if let Some(test_wid) = config.test_wid {
        tracing::info!(wid = test_wid, "test mode: drawing border on specific window");
        tracker.test_wid(test_wid, &config);
    } else {
        tracker.add_existing_windows(&config);
        tracker.determine_focus(&config);
    }

    // Process events from the channel on a background thread.
    // SLS transactions are thread-safe so border updates can happen off main.
    let config_clone = config.clone();
    std::thread::spawn(move || {
        event_loop(rx, &mut tracker, &config_clone);
    });

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
