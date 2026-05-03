# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

ers is a macOS window border renderer for the tarmac window manager. It draws colored overlay borders around application windows. Borders are NSWindows backed by a CAShapeLayer; window discovery and event subscription still go through private SkyLight (SLS) FFI. macOS Tahoe only, Apple Silicon.

## Build & Run

```bash
cargo build              # debug build
cargo build --release    # release build
cargo run                # run (borders all windows)
cargo run -- --list      # list on-screen windows with IDs and bounds
cargo run -- -w 6.0      # custom border width (default: 4.0)
cargo run -- <wid>       # border a specific window ID
```

Default log level is `info`. Use `RUST_LOG=ers=debug` for the full trace (focus changes, hide/unhide, sync, hotplug, etc.).

## Architecture

Four source files:

- **`src/main.rs`** — `BorderMap` manages overlay lifecycle: discover, add_fresh, sync_overlay, hide/unhide, reconcile. Main thread runs a CFRunLoop with a CFRunLoopTimer; events dispatched from SLS event handlers go through an mpsc channel and get processed in a 16–120ms batch. update_focus polls the front window each tick.

- **`src/nswindow_overlay.rs`** — `OverlayWindow` wraps `NSWindow + CAShapeLayer`. The NSWindow has `sharingType = .none` (the only mechanism Tahoe's screenshot picker honors — see "screenshot exclusion" below). The CAShapeLayer draws a stroked rounded-rect border that matches the target window's bounds plus the configured `border_width`. Coordinate conversion CG→Cocoa uses `CGDisplayBounds(CGMainDisplayID())` for the primary screen height (NSScreen caches and returns stale data after monitor hotplug).

- **`src/skylight.rs`** — FFI bindings: SLS (window discovery, bounds, ordering, events), CoreFoundation (CFArray, CFRunLoop, CFRunLoopTimer), CGDisplayRegisterReconfigurationCallback (hotplug detection).

- **`src/events.rs`** — SLS event registration. Filters out our own NSWindows by owner pid before forwarding via mpsc.

## Critical macOS Tahoe constraints

These are hard-won discoveries from debugging undocumented APIs:

1. **Screenshot exclusion requires NSWindow + sharingType=.none**. Tahoe's `screencaptureui` enumerates windows via `_SLSCopyWindowsWithOptionsAndTagsAndSpaceOptions` + `_CGSGetWindowTags` (verified by `otool` against the binary), and the SLS-side `SLSSetWindowSharingState` / SLS tag bits do NOT propagate to that path for raw SLS-only windows. Verified empirically with `screencapture -l <wid>`: SLS overlays are captured normally; NSWindows with `.none` sharingType return "could not create image from window".

2. **`SLSCopyManagedDisplaySpaces` poisons `SLSNewWindow`** — calling it on ANY connection corrupts window creation on ALL connections, process-wide. Use `CGWindowListCopyWindowInfo` for window discovery instead.

3. **`NSScreen.screens` returns stale data after monitor hotplug** until something internal triggers a refresh. For the CG→Cocoa Y-flip we need the live primary-screen height, so use `CGDisplayBounds(CGMainDisplayID())` directly. Don't rely on `[NSScreen screens][0]`.

4. **`orderWindow:relativeTo:` re-shows an off-screen NSWindow as a side effect.** In active-only mode, `sync_overlay` must NOT call `order_above` on hidden non-focused overlays just because their target moved (e.g. tarmac stack peek-out positions shift on every cycle) — otherwise every stacked window's overlay pops back onto the screen.

5. **CAShapeLayer state can be reset by macOS during display sleep/wake**, leaving the layer at a default tiny frame at the layer origin even though the NSWindow's `frame` survives. `BorderMap::refresh_all_layers` is called once a second from the periodic reconcile (and on hotplug) to re-apply each layer's frame and path.

6. **AX-driven moves don't reliably fire SLS WINDOW_MOVE notifications**, so during stack cycles a stored overlay can be at stale coordinates relative to its target. `update_focus` calls `sync_overlay` on both the old and new focused targets to pull live SLS bounds before un/hiding.

## Dependencies

`serde`/`serde_json` (window-info parsing), `tracing`/`tracing-subscriber` (logging), and the `objc2` family for AppKit/QuartzCore/CoreGraphics/CoreFoundation bindings. No runtime dependencies beyond macOS frameworks.
