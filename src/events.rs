//! SLS event registration and dispatch.
//!
//! Uses SLSRegisterNotifyProc to receive WindowServer events and forwards
//! them through a channel for processing on the main thread.

use crate::skylight::*;
use std::sync::mpsc;

// WindowServer event IDs (from JankyBorders events.h)
pub const EVENT_WINDOW_UPDATE: u32 = 723;
pub const EVENT_WINDOW_CLOSE: u32 = 804;
pub const EVENT_WINDOW_MOVE: u32 = 806;
pub const EVENT_WINDOW_RESIZE: u32 = 807;
pub const EVENT_WINDOW_REORDER: u32 = 808;
pub const EVENT_WINDOW_LEVEL: u32 = 811;
pub const EVENT_WINDOW_UNHIDE: u32 = 815;
pub const EVENT_WINDOW_HIDE: u32 = 816;
pub const EVENT_WINDOW_TITLE: u32 = 1322;
pub const EVENT_WINDOW_CREATE: u32 = 1325;
pub const EVENT_WINDOW_DESTROY: u32 = 1326;
pub const EVENT_SPACE_CHANGE: u32 = 1401;
pub const EVENT_FRONT_CHANGE: u32 = 1508;

/// Events dispatched from SLS callbacks to the main thread.
#[derive(Debug, Clone)]
pub enum WmEvent {
    WindowMove(u32),
    WindowResize(u32),
    WindowClose(u32),
    WindowReorder(u32),
    WindowHide(u32),
    WindowUnhide(u32),
    WindowLevel(u32),
    WindowCreate { wid: u32, sid: u64 },
    WindowDestroy { wid: u32, sid: u64 },
    SpaceChange,
    FrontAppChange,
    FocusCheck,
}

// Global sender — set once at startup, never mutated after.
static mut EVENT_TX: Option<mpsc::Sender<WmEvent>> = None;
static mut OWN_PID: i32 = 0;

/// Initialize the event system. Must be called before `register`.
pub fn init(tx: mpsc::Sender<WmEvent>) {
    unsafe {
        EVENT_TX = Some(tx);
        let mut pid: i32 = 0;
        pid_for_task(mach_task_self(), &mut pid);
        OWN_PID = pid;
    }
}

fn send(event: WmEvent) {
    unsafe {
        if let Some(ref tx) = EVENT_TX {
            let _ = tx.send(event);
        }
    }
}

fn is_own_window(cid: CGSConnectionID, wid: u32) -> bool {
    unsafe {
        let mut wid_cid: CGSConnectionID = 0;
        SLSGetWindowOwner(cid, wid, &mut wid_cid);
        let mut pid: i32 = 0;
        SLSConnectionGetPID(wid_cid, &mut pid);
        pid == OWN_PID
    }
}

// --- SLS callback functions ---
// These are called by WindowServer on the main thread via CFRunLoop.
// The callback signature is: fn(event_id: u32, data: *const u8, data_len: usize, context: *mut c_void)

/// Handler for window modify events (move, resize, close, etc.)
/// Data payload starts with the window ID as a u32.
unsafe extern "C" fn window_modify_handler(
    event: u32,
    data: *const u8,
    _data_len: usize,
    context: *mut std::ffi::c_void,
) {
    unsafe {
    let wid = std::ptr::read_unaligned(data as *const u32);
    let cid = context as isize as CGSConnectionID;

    if is_own_window(cid, wid) {
        return;
    }

    match event {
        EVENT_WINDOW_MOVE => send(WmEvent::WindowMove(wid)),
        EVENT_WINDOW_RESIZE => send(WmEvent::WindowResize(wid)),
        EVENT_WINDOW_CLOSE => send(WmEvent::WindowClose(wid)),
        EVENT_WINDOW_REORDER => {
            send(WmEvent::WindowReorder(wid));
            send(WmEvent::FocusCheck);
        }
        EVENT_WINDOW_LEVEL => send(WmEvent::WindowLevel(wid)),
        EVENT_WINDOW_UNHIDE => send(WmEvent::WindowUnhide(wid)),
        EVENT_WINDOW_HIDE => send(WmEvent::WindowHide(wid)),
        EVENT_WINDOW_TITLE | EVENT_WINDOW_UPDATE => send(WmEvent::FocusCheck),
        _ => {}
    }
    }
}

/// Handler for window create/destroy events.
/// Data payload: { sid: u64, wid: u32 } — may be unaligned.
unsafe extern "C" fn window_spawn_handler(
    event: u32,
    data: *const u8,
    _data_len: usize,
    context: *mut std::ffi::c_void,
) {
    unsafe {
        // Read fields with unaligned reads — SLS event data is not guaranteed aligned
        let sid = std::ptr::read_unaligned(data as *const u64);
        let wid = std::ptr::read_unaligned(data.add(8) as *const u32);
        let cid = context as isize as CGSConnectionID;

        if wid == 0 || is_own_window(cid, wid) {
            return;
        }

        match event {
            EVENT_WINDOW_CREATE => send(WmEvent::WindowCreate { wid, sid }),
            EVENT_WINDOW_DESTROY => send(WmEvent::WindowDestroy { wid, sid }),
            _ => {}
        }
    }
}

unsafe extern "C" fn space_handler(
    _event: u32,
    _data: *const std::ffi::c_void,
    _data_len: usize,
    _context: *mut std::ffi::c_void,
) {
    send(WmEvent::SpaceChange);
}

unsafe extern "C" fn front_app_handler(
    _event: u32,
    _data: *const std::ffi::c_void,
    _data_len: usize,
    _context: *mut std::ffi::c_void,
) {
    send(WmEvent::FrontAppChange);
    send(WmEvent::FocusCheck);
}

/// Register all SLS event handlers.
pub fn register(cid: CGSConnectionID) {
    let ctx = cid as isize as *mut std::ffi::c_void;

    unsafe {
        // Window modify events
        SLSRegisterNotifyProc(window_modify_handler as *const _, EVENT_WINDOW_CLOSE, ctx);
        SLSRegisterNotifyProc(window_modify_handler as *const _, EVENT_WINDOW_MOVE, ctx);
        SLSRegisterNotifyProc(window_modify_handler as *const _, EVENT_WINDOW_RESIZE, ctx);
        SLSRegisterNotifyProc(window_modify_handler as *const _, EVENT_WINDOW_LEVEL, ctx);
        SLSRegisterNotifyProc(window_modify_handler as *const _, EVENT_WINDOW_UNHIDE, ctx);
        SLSRegisterNotifyProc(window_modify_handler as *const _, EVENT_WINDOW_HIDE, ctx);
        SLSRegisterNotifyProc(window_modify_handler as *const _, EVENT_WINDOW_TITLE, ctx);
        SLSRegisterNotifyProc(window_modify_handler as *const _, EVENT_WINDOW_REORDER, ctx);
        SLSRegisterNotifyProc(window_modify_handler as *const _, EVENT_WINDOW_UPDATE, ctx);

        // Window lifecycle events
        SLSRegisterNotifyProc(window_spawn_handler as *const _, EVENT_WINDOW_CREATE, ctx);
        SLSRegisterNotifyProc(window_spawn_handler as *const _, EVENT_WINDOW_DESTROY, ctx);

        // Space change
        SLSRegisterNotifyProc(space_handler as *const _, EVENT_SPACE_CHANGE, ctx);

        // Front app change
        SLSRegisterNotifyProc(front_app_handler as *const _, EVENT_FRONT_CHANGE, ctx);
    }
}
