# ers

Window border renderer for macOS. Draws colored overlay borders around application windows using private SkyLight framework APIs.

Built as a companion to [tarmac](https://github.com/gardesk/tarmac), but runs standalone.

## Usage

```
ers [OPTIONS] [WINDOW_ID]

OPTIONS:
  -w, --width <PX>       Border width in pixels (default: 4.0)
  -r, --radius <PX>      Corner radius (default: 10.0)
  -c, --color <HEX>      Active border color (default: #5294e2)
  -i, --inactive <HEX>   Inactive border color (default: #59595980)
      --active-only      Only show border on focused window
      --list             List on-screen windows and exit
  -h, --help             Show this help
```

Run with no arguments to border all windows. Ctrl-C to stop (overlays are cleaned up).

Debug logging: `RUST_LOG=debug ers`

## With tarmac

tarmac manages ers as a child process. Set `border_width > 0` in your `~/.config/tarmac/init.lua`:

```lua
gar.set("border_width", "4")
gar.set("border_color_focused", "#5294e2")
gar.set("border_color_unfocused", "#59595980")
gar.set("border_radius", "10")
```

tarmac spawns ers automatically with `--active-only`. Config reloads restart ers.

## Install

```
cargo install --path .
```

## Requirements

- macOS (Apple Silicon or Intel)
- Accessibility permissions (System Settings → Privacy & Security → Accessibility)

## Limitations

- Uses private macOS APIs (SkyLight/CGS). These are undocumented and may break across macOS versions.
- Tested on macOS Tahoe. Should work on Monterey and later but no guarantees.

## License

MIT
