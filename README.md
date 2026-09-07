# EpochOxide

EpochOxide is a fast Rust data provider for Linux desktop shells, launchers, panels, and productivity tools.

It runs as a small user daemon, keeps common desktop data warm in memory, and exposes one query/activation interface for applications, files, clipboard history, windows, calculations, and custom menus. EpochOxide is designed around a lean Rust implementation and a simple socket protocol that shell UIs can integrate quickly.

## Features

- Desktop application search from XDG `.desktop` files.
- File search from configured roots with file previews and open/copy actions.
- Clipboard history for text and images.
- Optional OCR for image clipboard entries through `tesseract`.
- Clipboard edit actions for text and image clips.
- Window search and focus for Hyprland, Sway, Niri, and X11/wmctrl environments.
- Calculator results through `qalc` when available, with local arithmetic fallback.
- Custom TOML menus.
- User systemd service for warm, low-latency queries.
- Nix flake package, Home Manager module, and NixOS module.
- JSON-over-Unix-socket protocol for easy shell integration.

## Why

Desktop shells usually need the same data sources: apps, windows, files, clipboard, commands, menus, and actions. Rebuilding those indexes in every UI process is slow and wasteful.

EpochOxide centralizes that work in one long-lived user daemon. A launcher can keep one socket open, send a query on every keystroke, render returned items, and activate the selected action.

## Status

EpochOxide is early but usable. The current focus is building a fast, practical Linux desktop-shell backend with a stable enough local protocol for launcher experimentation.

EpochOxide currently uses newline-delimited JSON for simplicity and ease of integration. A binary protocol can be added later if a shell frontend needs lower overhead or stronger schema guarantees.

## Quick Start

Build from source:

```bash
cargo build --release
```

Run the daemon manually:

```bash
./target/release/epochoxide serve --socket "$XDG_RUNTIME_DIR/epochoxide.sock"
```

Query from another terminal:

```bash
./target/release/epochoxide query --providers apps --query firefox --limit 10
```

The CLI automatically talks to the daemon socket when it is available. If no daemon is running, it falls back to a cold local query.

## Recommended Runtime

For daily use, run EpochOxide as a user systemd service. This keeps indexes and provider state warm.

```bash
./target/release/epochoxide service install
systemctl --user enable --now epochoxide.service
```

Check the service:

```bash
systemctl --user status epochoxide.service
```

Query through the warm daemon:

```bash
epochoxide query --providers apps,files,clipboard,windows,calc,menus --query fire --limit 20
```

## Socket Location

EpochOxide defaults to:

```text
$XDG_RUNTIME_DIR/epochoxide.sock
```

On most systemd desktops this resolves to:

```text
/run/user/$UID/epochoxide.sock
```

`/tmp/epochoxide.sock` is only a fallback. `$XDG_RUNTIME_DIR` is preferred because it is per-user, has safer permissions, is cleaned up with the session, and matches the convention used by Wayland, PipeWire, D-Bus user services, and other desktop IPC.

## CLI Usage

List providers:

```bash
epochoxide list-providers
```

Query apps:

```bash
epochoxide query --providers apps --query firefox --limit 10
```

Query files:

```bash
epochoxide query --providers files --query README --limit 10
```

Query clipboard:

```bash
epochoxide query --providers clipboard --query copied --limit 10
```

Query windows:

```bash
epochoxide query --providers windows --query terminal --limit 10
```

Calculate:

```bash
epochoxide query --providers calc --query "1+2*3" --limit 5
```

Open a menu:

```bash
epochoxide menu bookmarks
```

Activate an item:

```bash
epochoxide activate --provider apps --identifier firefox.desktop --action open
```

Manage the service:

```bash
epochoxide service install
epochoxide service enable
epochoxide service start
epochoxide service status
epochoxide service restart
epochoxide service stop
```

## Providers

### Apps

The `apps` provider parses XDG `.desktop` files from user and system application directories.

Returned items include app name, generic name/comment, icon, fuzzy match metadata, and launch actions.

```bash
epochoxide query --providers apps --query browser --limit 10
```

### Files

The `files` provider indexes configured directories and returns file items with preview metadata.

Actions:

- `open`
- `open_dir`
- `copy_path`
- `copy_file`
- `reindex`

```bash
epochoxide query --providers files --query invoice --limit 20
```

### Clipboard

The `clipboard` provider captures text and image clipboard history while the daemon is running.

Text clips are searchable by content. Image clips are saved to disk and returned with `preview_type = "file"` so launchers can render them directly.

Actions:

- `copy`
- `edit`
- `ocr`
- `pin`
- `unpin`
- `remove`
- `remove_all`

OCR is optional and requires `tesseract`:

```toml
clipboard_ocr = true
```

### Windows

The `windows` provider discovers open windows and focuses selected windows.

Supported backends:

- Hyprland through `hyprctl`
- Sway through `swaymsg`
- Niri through `niri msg`
- X11 through `wmctrl`

```bash
epochoxide query --providers windows --query code --limit 10
```

### Calculator

The `calc` provider evaluates calculations. It uses `qalc` when available and falls back to local arithmetic evaluation.

```bash
epochoxide query --providers calc --query "sqrt(144)" --limit 5
```

### Menus

The `menus` provider loads TOML menu definitions from the configured menu directory.

```bash
epochoxide query --providers menus --query bookmarks --limit 10
epochoxide menu bookmarks
```

## Configuration

EpochOxide reads configuration from:

```text
~/.config/epochoxide/config.toml
```

Example:

```toml
socket = "/run/user/1000/epochoxide.sock"
file_roots = ["~/Documents", "~/Downloads"]
ignored_dirs = ["~/.cache", ".git", "node_modules", "target"]
menus_dir = "~/.config/epochoxide/menus"
launch_prefix = ""
terminal_cmd = ""
clipboard_max_items = 100
clipboard_image_dir = "~/.cache/epochoxide/clipboard/images"
clipboard_text_editor = "xdg-open"
clipboard_image_editor = ""
clipboard_ocr = false
clipboard_capture_interval_ms = 250
```

See `config.example.toml` for a starter file.

## Custom Menus

Menu files are TOML files placed in `menus_dir`.

Example:

```toml
name = "bookmarks"
name_pretty = "Bookmarks"
icon = "bookmark"
action = "xdg-open %VALUE%"

[[entries]]
text = "Rust"
value = "https://www.rust-lang.org"
keywords = ["language", "systems"]

[[entries]]
text = "EpochOxide"
value = "https://github.com/"
keywords = ["desktop", "provider"]
```

Run:

```bash
epochoxide menu bookmarks
```

## Socket Protocol

The daemon accepts one JSON request per line over a Unix socket and returns one JSON response per line.

Query request:

```json
{"type":"query","providers":["apps","files"],"query":"fire","limit":10,"exact":false}
```

Activation request:

```json
{"type":"activate","provider":"apps","identifier":"firefox.desktop","action":"open","query":"","arguments":""}
```

Provider list request:

```json
{"type":"providers"}
```

Menu request:

```json
{"type":"menu","menu":"bookmarks"}
```

Response shape:

```json
{"ok":true,"data":[],"error":null}
```

For the lowest latency shell integration, keep a persistent socket connection open while the launcher is visible and send a new query request for each input change.

## Nix

EpochOxide ships as a Nix flake.

Build:

```bash
nix build
```

Run:

```bash
nix run . -- query --providers apps --query firefox --limit 10
```

### Home Manager

```nix
{
  inputs.epochoxide.url = "github:your-user/epochoxide";

  outputs = { self, nixpkgs, home-manager, epochoxide, ... }: {
    homeConfigurations.brian = home-manager.lib.homeManagerConfiguration {
      pkgs = nixpkgs.legacyPackages.x86_64-linux;
      modules = [
        epochoxide.homeManagerModules.default
        {
          programs.epochoxide = {
            enable = true;
            enableService = true;
            settings = {
              file_roots = [ "~/Documents" "~/Downloads" ];
              clipboard_ocr = true;
              clipboard_text_editor = "xdg-open";
            };
          };
        }
      ];
    };
  };
}
```

### NixOS

```nix
{
  inputs.epochoxide.url = "github:your-user/epochoxide";

  outputs = { self, nixpkgs, epochoxide, ... }: {
    nixosConfigurations.host = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        epochoxide.nixosModules.default
        { services.epochoxide.enable = true; }
      ];
    };
  };
}
```

The Nix modules run EpochOxide as a systemd user service with:

```text
--socket %t/epochoxide.sock
```

In a user systemd unit, `%t` expands to the user runtime directory.

## Shell Integration

There are two integration modes.

CLI mode is easiest:

```bash
epochoxide query --providers apps,files,clipboard --query "$QUERY" --limit 20
```

Direct socket mode is fastest:

1. Connect to `$XDG_RUNTIME_DIR/epochoxide.sock`.
2. Keep the connection open while the shell UI is open.
3. Send a query JSON line for each input update.
4. Render returned `data` items.
5. Send an activation request when the user selects an item.

Direct persistent socket integration avoids process startup per keystroke and is the recommended approach for serious shell UI work.

## Runtime Tools

Some providers call common desktop tools when available:

- `wl-clipboard` for clipboard text/image capture.
- `tesseract` for OCR.
- `xdg-utils` for opening files/apps.
- `wmctrl` for X11 window focus.
- `hyprctl`, `swaymsg`, or `niri` for compositor windows.
- `qalc` for advanced calculations and unit conversion.

The Nix modules expose common runtime tools to the systemd service through its `PATH`.

## Performance Notes

EpochOxide is designed around warm in-memory state.

Current optimizations:

- Long-lived systemd user daemon.
- CLI automatically uses the daemon socket when available.
- Precomputed searchable strings for apps and files.
- Clipboard capture throttling to avoid shelling out on every keystroke.
- Persistent socket support for very low-latency frontend integration.

Future performance work:

- Incremental file indexing through `inotify`.
- Persistent on-disk indexes through `redb` or `sled`.
- Cached compositor window state from event streams.
- Streaming query responses.
- Binary socket framing through MessagePack or protobuf.
- Trigram/BM25 indexes for very large file collections.

## Roadmap

- Persistent history and ranking.
- Pins and aliases for apps, files, clipboard items, and menus.
- Rich icon/theme resolution.
- Thumbnail cache.
- Lua or WASM dynamic menus.
- Provider subscriptions and frontend update events.
- Optional binary protocol compatibility for lower-latency frontends.
- More providers: bookmarks, snippets, symbols, media, Bluetooth, package managers, secrets, and commands.

## License

MIT
