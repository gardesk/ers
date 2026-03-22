//! SLS event registration and dispatch.

use crate::skylight::*;
use std::sync::mpsc;

pub const EVENT_WINDOW_CLOSE: u32 = 804;
pub const EVENT_WINDOW_MOVE: u32 = 806;
pub const EVENT_WINDOW_RESIZE: u32 = 807;
pub const EVENT_WINDOW_REORDER: u32 = 808;
pub const EVENT_WINDOW_UNHIDE: u32 = 815;
pub const EVENT_WINDOW_HIDE: u32 = 816;
pub const EVENT_WINDOW_CREATE: u32 = 1325;
pub const EVENT_WINDOW_DESTROY: u32 = 1326;
pub const EVENT_SPACE_CHANGE: u32 = 1401;
pub const EVENT_FRONT_CHANGE: u32 = 1508;

#[derive(Debug)]
pub enum Event {
    Move(u32),
    Resize(u32),
    Close(u32),
    Hide(u32),
    Unhide(u32),
    Create(u32),
    Destroy(u32),
    SpaceChange,
    FrontChange,
}

static mut TX: Option<mpsc::Sender<Event>> = None;
static mut OWN_PID: i32 = 0;

pub fn init(tx: mpsc::Sender<Event>, own_pid: i32) {
    unsafe {
        TX = Some(tx);
        OWN_PID = own_pid;
    }
}

fn send(event: Event) {
    unsafe {
        if let Some(ref tx) = TX {
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

unsafe extern "C" fn window_handler(
    event: u32,
    data: *const u8,
    _data_len: usize,
    context: *mut std::ffi::c_void,
) {
    unsafe {
        let wid = std::ptr::read_unaligned(data as *const u32);
        let cid = context as isize as CGSConnectionID;
        if wid == 0 || is_own_window(cid, wid) { return; }

        match event {
            EVENT_WINDOW_MOVE => send(Event::Move(wid)),
            EVENT_WINDOW_RESIZE => send(Event::Resize(wid)),
            EVENT_WINDOW_CLOSE => send(Event::Close(wid)),
            EVENT_WINDOW_REORDER => {
                send(Event::Move(wid));
                send(Event::FrontChange); // reorder may be intra-app focus change
            }
            EVENT_WINDOW_HIDE => send(Event::Hide(wid)),
            EVENT_WINDOW_UNHIDE => send(Event::Unhide(wid)),
            _ => {}
        }
    }
}

unsafe extern "C" fn spawn_handler(
    event: u32,
    data: *const u8,
    _data_len: usize,
    context: *mut std::ffi::c_void,
) {
    unsafe {
        let _sid = std::ptr::read_unaligned(data as *const u64);
        let wid = std::ptr::read_unaligned(data.add(8) as *const u32);
        let cid = context as isize as CGSConnectionID;
        if wid == 0 || is_own_window(cid, wid) { return; }

        match event {
            EVENT_WINDOW_CREATE => send(Event::Create(wid)),
            EVENT_WINDOW_DESTROY => send(Event::Destroy(wid)),
            _ => {}
        }
    }
}

unsafe extern "C" fn space_handler(
    _event: u32, _data: *const u8, _len: usize, _ctx: *mut std::ffi::c_void,
) {
    send(Event::SpaceChange);
}

unsafe extern "C" fn front_handler(
    _event: u32, _data: *const u8, _len: usize, _ctx: *mut std::ffi::c_void,
) {
    send(Event::FrontChange);
}

pub fn register(cid: CGSConnectionID) {
    let ctx = cid as isize as *mut std::ffi::c_void;
    unsafe {
        for &ev in &[EVENT_WINDOW_CLOSE, EVENT_WINDOW_MOVE, EVENT_WINDOW_RESIZE,
                     EVENT_WINDOW_REORDER, EVENT_WINDOW_HIDE, EVENT_WINDOW_UNHIDE] {
            SLSRegisterNotifyProc(window_handler as *const _, ev, ctx);
        }
        SLSRegisterNotifyProc(spawn_handler as *const _, EVENT_WINDOW_CREATE, ctx);
        SLSRegisterNotifyProc(spawn_handler as *const _, EVENT_WINDOW_DESTROY, ctx);
        SLSRegisterNotifyProc(space_handler as *const _, EVENT_SPACE_CHANGE, ctx);
        SLSRegisterNotifyProc(front_handler as *const _, EVENT_FRONT_CHANGE, ctx);
    }
}
