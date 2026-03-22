# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

ers is a macOS window border renderer for the tarmac window manager. It draws colored overlay borders around application windows using private SkyLight framework APIs. macOS Tahoe only.

## Build & Run

```bash
cargo build              # debug build
cargo build --release    # release build
cargo run                # run (borders all windows)
cargo run -- --list      # list on-screen windows with IDs and bounds
cargo run -- -w 6.0      # custom border width (default: 4.0)
cargo run -- <wid>       # border a specific window ID
```

No tests — verification is visual. Use `RUST_LOG=debug` for tracing output.

## Architecture

Three source files (~1300 lines total):

- **`src/main.rs`** — `BorderMap` struct manages overlay lifecycle. Event loop batches window events with 150ms debounce, then processes creates/destroys/moves/resizes. Focus detection recolors borders (active=white, inactive=gray). Main thread runs CFRunLoop; events dispatch from a background thread via mpsc.

- **`src/skylight.rs`** — FFI bindings for private macOS frameworks: SkyLight (CGS window creation, event registration), CoreGraphics (drawing), CoreFoundation (collections, RunLoop). All types `repr(C)`.

- **`src/events.rs`** — Event enum and SLSRegisterNotifyProc callbacks. Filters out the renderer's own windows to prevent feedback loops. Sends events over mpsc channel.

- **`build.rs`** — Links SkyLight (private framework), CoreGraphics, CoreFoundation.

## Critical macOS Tahoe constraints

These are hard-won discoveries from debugging undocumented APIs:

1. **SLSCopyManagedDisplaySpaces poisons SLSNewWindow** — calling it on ANY connection corrupts window creation on ALL connections. Use `CGWindowListCopyWindowInfo` instead.

2. **Fresh SLS connection per border** — each overlay needs its own `SLSNewConnection`. Required for reliable rendering.

3. **Create windows at final size** — the 1×1-then-reshape pattern breaks on Tahoe. Create at correct position/size immediately.

4. **Draw before setting tags** — CGContext from `SLWindowContextCreate` must be used to draw BEFORE setting window tags/shadow. Re-obtaining context later for redraws uses the border's own connection.

## Dependencies

Only `serde`/`serde_json` (JSON parsing of window info) and `tracing`/`tracing-subscriber` (logging). No external runtime dependencies beyond macOS frameworks.
