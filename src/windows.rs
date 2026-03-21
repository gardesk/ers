//! Window tracking: maps target window IDs to their border overlays.
//!
//! Handles window discovery, creation, destruction, focus changes,
//! and notification subscription.

use std::collections::HashMap;

use crate::border::BorderWindow;
use crate::config::Config;
use crate::skylight::*;

// Window tag constants (from JankyBorders window.h)
const WINDOW_TAG_DOCUMENT: u64 = 1 << 0;
const WINDOW_TAG_FLOATING: u64 = 1 << 1;
const WINDOW_TAG_ATTACHED: u64 = 1 << 7;
const WINDOW_TAG_STICKY: u64 = 1 << 11;
const WINDOW_TAG_IGNORES_CYCLE: u64 = 1 << 18;
const WINDOW_TAG_MODAL: u64 = 1 << 31;

pub struct WindowTracker {
    borders: HashMap<u32, BorderWindow>,
    focused_wid: u32,
    cid: CGSConnectionID,
    own_pid: i32,
}

impl WindowTracker {
    pub fn new(cid: CGSConnectionID) -> Self {
        let mut pid: i32 = 0;
        unsafe { pid_for_task(mach_task_self(), &mut pid) };

        Self {
            borders: HashMap::new(),
            focused_wid: 0,
            cid,
            own_pid: pid,
        }
    }

    /// Check if a window iterator entry represents a "suitable" application window.
    fn window_suitable(iterator: CFTypeRef) -> bool {
        let tags = unsafe { SLSWindowIteratorGetTags(iterator) };
        let attributes = unsafe { SLSWindowIteratorGetAttributes(iterator) };
        let parent_wid = unsafe { SLSWindowIteratorGetParentID(iterator) };

        parent_wid == 0
            && ((attributes & 0x2) != 0 || (tags & 0x400000000000000) != 0)
            && (tags & WINDOW_TAG_ATTACHED) == 0
            && (tags & WINDOW_TAG_IGNORES_CYCLE) == 0
            && ((tags & WINDOW_TAG_DOCUMENT) != 0
                || ((tags & WINDOW_TAG_FLOATING) != 0 && (tags & WINDOW_TAG_MODAL) != 0))
    }

    /// Is this window owned by our process?
    fn is_own_window(&self, wid: u32) -> bool {
        unsafe {
            let mut wid_cid: CGSConnectionID = 0;
            SLSGetWindowOwner(self.cid, wid, &mut wid_cid);
            let mut pid: i32 = 0;
            SLSConnectionGetPID(wid_cid, &mut pid);
            pid == self.own_pid
        }
    }

    /// Discover and add borders for all existing windows on all spaces.
    pub fn add_existing_windows(&mut self, config: &Config) {
        let cid = self.cid;

        // Get all spaces across all displays
        let mut all_sids: Vec<u64> = Vec::new();
        unsafe {
            let display_spaces = SLSCopyManagedDisplaySpaces(cid);
            if !display_spaces.is_null() {
                let display_count = CFArrayGetCount(display_spaces);
                for i in 0..display_count {
                    let display_ref = CFArrayGetValueAtIndex(display_spaces, i);
                    // Get "Spaces" key from display dict
                    let spaces_key_bytes = b"Spaces\0";
                    let spaces_key =
                        CFStringCreateWithCString(std::ptr::null(), spaces_key_bytes.as_ptr(), 0x0600_0100);
                    let spaces_ref = CFDictionaryGetValue(display_ref, spaces_key as CFTypeRef);
                    CFRelease(spaces_key as CFTypeRef);

                    if !spaces_ref.is_null() {
                        let spaces_count = CFArrayGetCount(spaces_ref);
                        for j in 0..spaces_count {
                            let space_ref = CFArrayGetValueAtIndex(spaces_ref, j);
                            let id_key_bytes = b"id64\0";
                            let id_key = CFStringCreateWithCString(
                                std::ptr::null(),
                                id_key_bytes.as_ptr(),
                                0x0600_0100,
                            );
                            let sid_ref = CFDictionaryGetValue(space_ref, id_key as CFTypeRef);
                            CFRelease(id_key as CFTypeRef);

                            if !sid_ref.is_null() {
                                let mut sid: u64 = 0;
                                let num_type = CFNumberGetType(sid_ref);
                                CFNumberGetValue(
                                    sid_ref,
                                    num_type,
                                    &mut sid as *mut _ as *mut _,
                                );
                                all_sids.push(sid);
                            }
                        }
                    }
                }
                CFRelease(display_spaces);
            }
        }

        if all_sids.is_empty() {
            return;
        }

        // Get all windows on those spaces
        unsafe {
            let space_list = cfarray_of_cfnumbers(
                all_sids.as_ptr() as *const _,
                std::mem::size_of::<u64>(),
                all_sids.len() as i32,
                kCFNumberSInt64Type,
            );

            let mut set_tags: u64 = 1;
            let mut clear_tags: u64 = 0;
            let window_list = SLSCopyWindowsWithOptionsAndTags(
                cid, 0, space_list, 0x2, &set_tags, &clear_tags,
            );

            if !window_list.is_null() {
                let count = CFArrayGetCount(window_list);
                if count > 0 {
                    let query = SLSWindowQueryWindows(cid, window_list, 0x0);
                    if !query.is_null() {
                        let iterator = SLSWindowQueryResultCopyWindows(query);
                        if !iterator.is_null() {
                            while SLSWindowIteratorAdvance(iterator) {
                                if Self::window_suitable(iterator) {
                                    let wid = SLSWindowIteratorGetWindowID(iterator);
                                    if !self.is_own_window(wid) {
                                        self.create_border(wid, config);
                                    }
                                }
                            }
                            CFRelease(iterator);
                        }
                        CFRelease(query);
                    }
                }
                CFRelease(window_list);
            }
            CFRelease(space_list);
        }

        self.update_notifications();
    }

    /// Create a border for a window if it passes suitability checks.
    pub fn create_border(&mut self, wid: u32, config: &Config) -> bool {
        if self.borders.contains_key(&wid) || self.is_own_window(wid) {
            return false;
        }

        // Check suitability via query
        let suitable = unsafe {
            let target_ref = cfarray_of_cfnumbers(
                &wid as *const _ as *const _,
                std::mem::size_of::<u32>(),
                1,
                kCFNumberSInt32Type,
            );
            let mut result = false;
            if !target_ref.is_null() {
                let query = SLSWindowQueryWindows(self.cid, target_ref, 0x0);
                if !query.is_null() {
                    let iter = SLSWindowQueryResultCopyWindows(query);
                    if !iter.is_null() {
                        if SLSWindowIteratorGetCount(iter) > 0 && SLSWindowIteratorAdvance(iter) {
                            result = Self::window_suitable(iter);
                        }
                        CFRelease(iter);
                    }
                    CFRelease(query);
                }
                CFRelease(target_ref);
            }
            result
        };

        if !suitable {
            return false;
        }

        if let Some(border) = BorderWindow::new(self.cid, wid, config.hidpi) {
            self.borders.insert(wid, border);
            self.update_notifications();
            true
        } else {
            false
        }
    }

    /// Remove a window's border.
    pub fn destroy_border(&mut self, wid: u32) -> bool {
        if self.borders.remove(&wid).is_some() {
            self.update_notifications();
            true
        } else {
            false
        }
    }

    /// Update all borders (full redraw).
    pub fn update_all(&mut self, config: &Config) {
        for border in self.borders.values_mut() {
            border.needs_redraw = true;
            border.update(
                &config.active_color,
                &config.inactive_color,
                config.border_width,
                config.radius,
                config.border_order,
            );
        }
    }

    /// Update a single window's border.
    pub fn update_window(&mut self, wid: u32, config: &Config) {
        if let Some(border) = self.borders.get_mut(&wid) {
            border.update(
                &config.active_color,
                &config.inactive_color,
                config.border_width,
                config.radius,
                config.border_order,
            );
        }
    }

    /// Fast move-only update for a window.
    pub fn move_window(&mut self, wid: u32, config: &Config) {
        if let Some(border) = self.borders.get_mut(&wid) {
            border.reposition_only(config.border_width);
        }
    }

    /// Hide a window's border.
    pub fn hide_window(&mut self, wid: u32) {
        if let Some(border) = self.borders.get(&wid) {
            border.hide();
        }
    }

    /// Unhide a window's border.
    pub fn unhide_window(&mut self, wid: u32, config: &Config) {
        if let Some(border) = self.borders.get(&wid) {
            border.unhide(config.border_order);
        }
    }

    /// Determine the front window and update focus state.
    pub fn determine_focus(&mut self, config: &Config) {
        let front_wid = get_front_window(self.cid);

        if front_wid == 0 || front_wid == self.focused_wid {
            return;
        }

        let old_focused = self.focused_wid;
        self.focused_wid = front_wid;

        // Unfocus old
        if let Some(border) = self.borders.get_mut(&old_focused) {
            border.focused = false;
            border.needs_redraw = true;
            if !config.active_only {
                border.update(
                    &config.active_color,
                    &config.inactive_color,
                    config.border_width,
                    config.radius,
                    config.border_order,
                );
            } else {
                border.hide();
            }
        }

        // Focus new — create if not tracked
        if !self.borders.contains_key(&front_wid) {
            self.create_border(front_wid, config);
        }
        if let Some(border) = self.borders.get_mut(&front_wid) {
            border.focused = true;
            border.needs_redraw = true;
            border.update(
                &config.active_color,
                &config.inactive_color,
                config.border_width,
                config.radius,
                config.border_order,
            );
        }
    }

    /// Register for per-window notifications.
    fn update_notifications(&self) {
        let wids: Vec<u32> = self.borders.keys().copied().collect();
        if wids.is_empty() {
            return;
        }
        unsafe {
            SLSRequestNotificationsForWindows(self.cid, wids.as_ptr(), wids.len() as i32);
        }
    }

    /// Test mode: draw border on a specific window ID.
    pub fn test_wid(&mut self, wid: u32, config: &Config) {
        if let Some(mut border) = BorderWindow::new(self.cid, wid, config.hidpi) {
            border.focused = true;
            border.update(
                &config.active_color,
                &config.inactive_color,
                config.border_width,
                config.radius,
                config.border_order,
            );
            self.borders.insert(wid, border);
            self.update_notifications();
        } else {
            eprintln!("failed to create border for wid {wid}");
            std::process::exit(1);
        }
    }
}

/// Get the front (focused) window ID using SLS process detection.
fn get_front_window(cid: CGSConnectionID) -> u32 {
    unsafe {
        let active_sid = get_active_space_id(cid);
        if active_sid == 0 {
            return 0;
        }

        let mut psn = ProcessSerialNumber { high: 0, low: 0 };
        _SLPSGetFrontProcess(&mut psn);
        let mut target_cid: CGSConnectionID = 0;
        SLSGetConnectionIDForPSN(cid, &mut psn, &mut target_cid);

        let mut set_tags: u64 = 1;
        let mut clear_tags: u64 = 0;
        let space_list = cfarray_of_cfnumbers(
            &active_sid as *const _ as *const _,
            std::mem::size_of::<u64>(),
            1,
            kCFNumberSInt64Type,
        );

        let window_list = SLSCopyWindowsWithOptionsAndTags(
            cid,
            target_cid as u32,
            space_list,
            0x2,
            &set_tags,
            &clear_tags,
        );

        let mut wid: u32 = 0;
        if !window_list.is_null() {
            let count = CFArrayGetCount(window_list);
            if count > 0 {
                let query = SLSWindowQueryWindows(cid, window_list, 0x0);
                if !query.is_null() {
                    let iterator = SLSWindowQueryResultCopyWindows(query);
                    if !iterator.is_null() && SLSWindowIteratorGetCount(iterator) > 0 {
                        while SLSWindowIteratorAdvance(iterator) {
                            if WindowTracker::window_suitable(iterator) {
                                wid = SLSWindowIteratorGetWindowID(iterator);
                                break;
                            }
                        }
                        CFRelease(iterator);
                    }
                    CFRelease(query);
                }
            }
            CFRelease(window_list);
        }
        CFRelease(space_list);
        wid
    }
}

/// Get the active space ID for the current display.
fn get_active_space_id(cid: CGSConnectionID) -> u64 {
    unsafe {
        let uuid = SLSCopyActiveMenuBarDisplayIdentifier(cid);
        if uuid.is_null() {
            return 0;
        }
        let sid = SLSManagedDisplayGetCurrentSpace(cid, uuid);
        CFRelease(uuid as CFTypeRef);
        sid
    }
}

unsafe extern "C" {
    fn CFStringCreateWithCString(
        alloc: CFAllocatorRef,
        c_str: *const u8,
        encoding: u32,
    ) -> CFStringRef;
}
