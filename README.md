# EpochOxide

EpochOxide is a fast Rust data provider for Linux desktop shells, launchers, panels, and productivity tools.

It runs as a small user daemon, keeps common desktop data warm in memory, and exposes one query/activation interface for applications, files, clipboard history, windows, calculations, and custom menus. EpochOxide is designed around a lean Rust implementation and a simple socket protocol that shell UIs can integrate quickly.

## Features

- Desktop application search from XDG `.desktop` files.
- File search from configured roots with file previews and open/copy actions.
- Command runner from `$PATH` and custom configured commands.
- Clipboard history for text and images.
- Optional OCR for image clipboard entries through `tesseract`.
- Clipboard edit actions for text and image clips.
- Window search and focus for Hyprland, Sway, Niri, and X11/wmctrl environments.
- Calculator results through `qalc` when available, with local arithmetic fallback.
- Custom TOML menus.
- User systemd service for warm, low-latency queries.
- Persistent usage history and recency-aware ranking.
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
epochoxide query --providers apps,files,runner,clipboard,windows,calc --query fire --limit 20
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

Query commands:

```bash
epochoxide query --providers runner --query rg --limit 10
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

The `files` provider searches the configured roots and returns file items with preview metadata. Matching is against the whole path, not just the file name, so `epoch` finds everything under `EpochOxide/` and not merely the directory itself; entries whose own name matches are ranked above those that only match an ancestor directory.

Two search backends sit behind it, selected by `file_index`:

| `file_index` | Backend | Query latency | Daemon memory |
| --- | --- | --- | --- |
| `"auto"` (default) | `fd`, falling back to the index if `fd` is missing | ~200ms | ~5-10MB |
| `"always"` | in-memory index | <10ms | ~1.6KB per indexed path (>1GB over a 700k-entry home) |
| `"never"` | `fd` only; provider goes quiet without it | ~200ms | ~5-10MB |

`fd` re-walks the roots on every query, which is why it is slower but flat in memory. The index holds every path under every root and answers from RAM, which is why it is fast but expensive — on a home directory full of `node_modules`, Go module caches and vendored trees, narrowing `file_roots`/`ignored_dirs` matters more than the backend choice.

In daemon mode the index (when built) watches configured roots and applies filesystem changes incrementally before queries. On Linux this uses inotify through the `notify` backend.

Actions:

- `open`
- `open_dir`
- `copy_path`
- `copy_file`
- `reindex`

```bash
epochoxide query --providers files --query invoice --limit 20
```

### Runner

The `runner` provider indexes executable commands from `$PATH` and optional custom commands from config.

Actions:

- `run`
- `reindex`

```bash
epochoxide query --providers runner --query firefox --limit 10
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

Every TOML menu in the configured menu directory registers as a provider of its own, named after
the menu. There is no umbrella `menus` provider to drill through: a menu is queried, prefixed, and
listed like any other provider.

```bash
epochoxide query --providers bookmarks --query rust --limit 10
epochoxide menu bookmarks
```

Give a menu a shortcut by pointing a prefix at its name, the same way the built-in providers get
theirs:

```toml
[query_prefixes]
"?" = "keybinds"
```

`provider_enabled` and `provider_weights` accept a menu name too, so a single menu can be switched
off or reweighted without touching the others. Setting `menus = false` still disables all of them.

## Configuration

EpochOxide reads configuration from:

```text
~/.config/epochoxide/config.toml
```

Example:

```toml
socket = "/run/user/1000/epochoxide.sock"
file_roots = ["~"]
ignored_dirs = ["~/.cache", "~/.local/share/Trash", "~/.cargo/registry", "~/.rustup", "~/.npm", "~/.pnpm-store", "~/.var/app", ".git", "node_modules", "target", "dist", "build", ".direnv"]
menus_dir = "~/.config/epochoxide/menus"
launch_prefix = ""
terminal_cmd = ""
clipboard_max_items = 100
clipboard_image_dir = "~/.cache/epochoxide/clipboard/images"
clipboard_text_editor = "xdg-open"
clipboard_image_editor = ""
clipboard_ocr = false
clipboard_capture_interval_ms = 250
runner_scan_path = true

[[runner_commands]]
name = "Edit Config"
command = "xdg-open ~/.config/epochoxide/config.toml"
keywords = ["epochoxide", "settings"]
icon = "preferences-system"
terminal = false
```

See `config.example.toml` for a starter file.

## Custom Menus

Menu files are TOML files placed in `menus_dir`. Each becomes its own provider: `name` is the
provider name a prefix points at, `name_pretty`, `description` and `icon` are how it presents
itself in a launcher's provider list, and `icon` is also the fallback icon for entries that do not
carry one.

Example:

```toml
name = "bookmarks"
name_pretty = "Bookmarks"
description = "Sites worth keeping"
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

An entry either runs something or hands back text. `value` is substituted into the menu's `action`
(or an entry's own `[actions]`) and run as a shell command; `copy` instead puts its text on the
clipboard and runs nothing, so command menus and snippet menus can share a file.

```toml
name = "screenshots"
name_pretty = "Screenshots"
icon = "applications-graphics"
action = "%VALUE%"

[[entries]]
text = "Region to clipboard"
value = "grim -g \"$(slurp)\" - | wl-copy"

[[entries]]
text = "Record screen"
value = "wf-recorder -f ~/Videos/$(date +%s).mp4"

[[entries]]
text = "Shrug"
copy = "¯\\_(ツ)_/¯"
```

### Entry fields

| Field | Meaning |
|-------|---------|
| `text` | What the entry is called. Required. |
| `subtext` | Second line. Defaults to `copy`, then `value`, then the keywords. |
| `value` | Substituted into `action` as `%VALUE%`. Defaults to `text`. |
| `copy` | Enter copies this instead of running anything. |
| `icon` | Falls back to the menu's `icon`. |
| `keywords` | Extra words the entry matches on. |
| `actions` | Per-entry `name = "command"` map, overriding the menu's `action`/`actions`. |
| `async` | Command whose output becomes the entry's preview. |

`%ARGS%` in a command is replaced with arguments the client passed to the activation.

### Dynamic menus

`command` replaces `entries` with a generator: any program that prints those same entries as a
JSON array. It is what makes a menu reflect live state rather than a list written by hand — the
keybinds menu below asks the running compositor for its binds every time it is opened.

```toml
name = "keybinds"
name_pretty = "Keybinds"
description = "Search the keybinds of the running compositor"
icon = "input-keyboard"
action = "%VALUE%"
command = "~/.config/epochoxide/menus/keybinds.sh"
```

```json
[
  {"text": "Close window", "subtext": "SUPER+Q", "icon": "input-keyboard",
   "value": "hyprctl repl 'hl.dispatch(hl.dsp.window.close())'", "keywords": ["window"]},
  {"text": "Run: ghostty", "subtext": "SUPER+Return", "icon": "input-keyboard",
   "value": "ghostty", "keywords": []}
]
```

Generators are run through `sh -c` by the daemon, so they inherit the daemon's environment and
PATH — under the Home Manager service that means `runtimePackages` plus the system and user
profiles. A generator that needs a tool no launcher can be assumed to have should bring it itself.

Because a generator can be slow (the keybinds one shells out to the compositor and takes seconds),
its output is generated once in the background at startup and then kept:

- opening the menu regenerates it, behind the answer already on screen, so what is shown is at
  most one open old and the user never waits;
- searching within the menu reuses what that generation returned rather than re-running it per
  keystroke;
- `cache_ms` sets how long that result stands before a search also triggers a regeneration
  (default 10000).

Run a menu directly, whatever its source:

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

Items carry `actions: ["open", ...]` — the action *names* available on that specific item — but not the full `ActionCapability` metadata (label, `destructive`, `needs_args`, etc.) for each one. Fetch the `providers` capability list once per session and join on `item.provider` + action name to get that metadata, rather than expecting it duplicated on every item.

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
        {
          services.epochoxide = {
            enable = true;
            settings.clipboard_ocr = true;
          };
        }
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

The Home Manager module writes practical defaults automatically:

```nix
settings = {
  file_roots = [ "~" ];
  ignored_dirs = [
    "~/.cache"
    "~/.local/share/Trash"
    "~/.cargo/registry"
    "~/.rustup"
    "~/.npm"
    "~/.pnpm-store"
    "~/.var/app"
    ".git"
    "node_modules"
    "target"
    "dist"
    "build"
    ".direnv"
  ];
  runner_scan_path = true;
};
```

You only need to set `file_roots` or `ignored_dirs` if you want to override these defaults.

## Shell Integration

There are two integration modes.

CLI mode is easiest:

```bash
epochoxide query --providers apps,files,runner,clipboard --query "$QUERY" --limit 20
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
- Precomputed searchable strings for apps, files, and commands.
- Persistent usage history and recency boosts across providers.
- Filesystem watcher support for warm incremental file index updates.
- Clipboard capture throttling to avoid shelling out on every keystroke.
- Persistent socket support for very low-latency frontend integration.

Future performance work:

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
