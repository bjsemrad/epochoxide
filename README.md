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
- Screenshots -- region, window, monitor, or the whole layout -- saved, copied, and announced.
- OCR capture: read the text out of part of the screen and put it on the clipboard.
- Screen recording of a region, a window, or a monitor, with the daemon owning the recorder.
- Nix flake update awareness: what could move, checked without writing to your flake.
- CPU power state -- profile, governor, energy preference, turbo -- read from sysfs.
- Machine identity, battery wear, and the firmware updates fwupd is offering.
- Versioned Epoch API for normalized compositor state and Tailscale, independent of the launcher.

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
If the query looks like a shell command, runner also returns a `Run: ...` fallback item that executes
the raw query in a terminal.

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
screenshot_dir = "~/Pictures/Screenshots"
screenshot_filename = "screenshot-%Y%m%d-%H%M%S.png"
screenshot_copy = true
screenshot_save = true
screenshot_notify = true
ocr_language = "eng"
recording_dir = "~/Videos/Recordings"
recording_filename = "recording-%Y%m%d-%H%M%S.mp4"
recording_notify = true
recording_framerate = 30
nix_flake = "~/nixconfig"
nix_check_interval_minutes = 60
nix_update_command = "nix flake update"
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
carry one. Icons are passed through as written, so what a name may be is the launcher's business:
EpochShell resolves freedesktop names against the icon theme and also draws a Nerd Font glyph.

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
name = "capture"
name_pretty = "Capture"
icon = "camera-photo"
action = "epochctl capture screenshot %VALUE%"

[[entries]]
text = "Region"
value = "region"

[[entries]]
text = "Region to clipboard"
value = "region --no-save"

[[entries]]
text = "Shrug"
copy = "¯\\_(ツ)_/¯"
```

`examples/menus/capture.toml` is the full version of that menu: every screenshot mode, a delayed
one, and an entry that opens the folder. Copy it into `menus_dir` and give it a prefix to have the
launcher offer capture modes as you type.

### Entry fields

| Field | Meaning |
|-------|---------|
| `text` | What the entry is called. Required. |
| `subtext` | Second line. Defaults to `copy`, then `value`, then the keywords. |
| `value` | Substituted into `action` as `%VALUE%`. Defaults to `text`. |
| `copy` | Enter copies this instead of running anything. |
| `icon` | Icon name for the launcher to resolve. Falls back to the menu's `icon`. |
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

## Epoch API

Alongside the launcher protocol, EpochOxide exposes a versioned API for everything that is *not*
a launcher result: compositor state, service integrations, and system information. It is what
`epochctl` and the shell call so neither has to know whether the session is Hyprland or niri.

Three rules hold across the whole surface:

1. **Normalized, not raw.** No caller ever sees a `hyprctl` payload or a `tailscale status` blob.
2. **Discoverable.** `api.describe` reports the version, every group and method, and whether each
   group is usable on this machine, so a shell can feature-detect instead of parsing error text.
3. **Typed failures.** Errors carry a machine-readable `code`.

### Discovery

```bash
epochoxide api api.describe
```

```text
contract version: 1.0
  compositor   available    7 methods
  tailscale    available    3 methods
  capture      available    6 methods
  localsend    available    9 methods
  dev          planned      0 methods   not implemented in this build
  nix          available    5 methods
  system       available    5 methods
```

Groups marked `planned` are part of the contract but not implemented; they answer with
`code: "unavailable"` rather than an empty result, so a caller can tell "not built yet" from
"nothing to report". A group can also be `unavailable` at runtime — `tailscale` is, when the CLI
is not installed.

### Compositor

```bash
epochoxide api compositor.windows
epochoxide api compositor.activeWindow
epochoxide api compositor.workspaces
epochoxide api compositor.monitors
epochoxide api compositor.focusWindow    --params '{"id":"hypr:0x5a6557dd9b40"}'
epochoxide api compositor.closeWindow    --params '{"id":"hypr:0x5a6557dd9b40"}'
epochoxide api compositor.focusWorkspace --params '{"id":"3"}'
```

`focusWorkspace` takes either the qualified `id` from `compositor.workspaces` (`hypr:3`) or a bare
name or number, so a keybinding does not have to know which compositor it is on. A workspace that
does not exist yet is created, matching the compositors' own behaviour.

Windows, workspaces, and monitors come back in one shape whatever the compositor:

```json
{
  "id": "hypr:0x5a6557dd9b40",
  "app_id": "com.mitchellh.ghostty",
  "title": "thor: omarchy",
  "workspace": "4",
  "monitor": "eDP-1",
  "focused": true,
  "floating": false,
  "x": 6,
  "y": 46
}
```

Both lists come back ordered so a caller can render them directly. Workspaces sort by displayed
name, numerically where it is a number (`2` before `10`, not after) and alphabetically after those
where it is not. Windows sort by workspace, then left to right and top to bottom -- the order they
appear on screen, rather than the focus or creation order compositors return internally.

`id` is backend-qualified and round-trips: pass it straight back to `focusWindow`. Hyprland
reports a window's monitor as an index and niri reports workspaces by id — both are resolved to
names here so the shell never has to.

### Live updates

```bash
epochoxide api compositor.subscribe
```

Streams a full state snapshot: once on connect, then again whenever anything changes. The daemon
watches Hyprland's `.socket2.sock` or niri's event stream and re-reads normalized state; backends
with no event source fall back to polling. Compositor events are never forwarded -- a raw
`workspace>>3` line is exactly the detail this layer exists to absorb, so an event only triggers a
re-read. Snapshots that match the previous one are not sent, so the several events a compositor
emits for one action arrive as a single update.

Being a streaming method, it holds the connection open and needs a running daemon.

Backends live one per file under `src/compositor/`, each an implementation of the `Compositor`
trait. Hyprland, niri, and sway are supported, with wmctrl as an X11 fallback that can only list
and focus windows. The backend that answers is detected at call time, so a compositor restart
does not need a daemon restart. Adding one means adding a module and a line in `backends()`.

### Capture

```bash
epochoxide api capture.screenshot                                     # drag out a region
epochoxide api capture.screenshot --params '{"mode":"window"}'        # the focused window
epochoxide api capture.screenshot --params '{"mode":"window","select":true}'
epochoxide api capture.screenshot --params '{"mode":"fullscreen"}'    # the focused monitor
epochoxide api capture.screenshot --params '{"mode":"fullscreen","output":"DP-3"}'
epochoxide api capture.screenshot --params '{"mode":"all"}'           # every monitor, one image
epochoxide api capture.screenshot --params '{"save":false,"cursor":true,"delay":3}'
epochoxide api capture.status
```

```json
{
  "cancelled": false,
  "mode": "window",
  "path": "/home/you/Pictures/Screenshots/screenshot-20260112-144233.png",
  "saved": true,
  "copied": true,
  "notified": true,
  "geometry": "6,46 2148x1388",
  "output": null,
  "window": "thor: epochoxide",
  "width": 2864,
  "height": 1850,
  "bytes": 305481
}
```

A shot is saved to `screenshot_dir`, copied to the clipboard, and announced with `notify-send` --
which EpochShell answers, since it is the session's notification server. The notification carries
the file path as its image hint, so the shell shows the shot itself rather than a camera icon.
`copy`, `save`, `notify`, and `directory` override those defaults per call; leaving one out keeps
the configured behaviour rather than this API's opinion of it.

Cancelling a selection answers `{"cancelled": true}` with `ok: true`. Pressing Escape is how
people change their mind, and a keybinding should not report a failure for it.

The pixels come from `grim` and the selection from `slurp`, which are wlroots screencopy tools
rather than compositor-specific ones, so the same path serves Hyprland, niri, and sway. What *is*
compositor-specific -- where a window is on screen, and which monitor is focused -- is asked of the
normalized compositor layer, so no `hyprctl` payload reaches capture. A compositor that does not
report screen-space geometry (niri lays windows out in scrolling columns) reports
`window_capture: false` from `capture.status`, and `mode: "window"` there says so rather than
capturing the wrong rectangle.

`capture.status` answers even when the group is unavailable: it is how a caller finds out that
grim is not installed, so it would be useless if a missing grim silenced it.

```bash
epochoxide api capture.ocr                                          # read a region
epochoxide api capture.ocr --params '{"mode":"window"}'
epochoxide api capture.ocr --params '{"language":"eng+deu"}'
epochoxide api capture.ocr --params '{"save":true}'                 # keep the image too
```

```json
{
  "cancelled": false,
  "mode": "region",
  "text": "the text that was on screen",
  "characters": 27,
  "lines": 1,
  "copied": true,
  "notified": true,
  "language": "eng",
  "geometry": "980,420 640x180",
  "path": null,
  "saved": false
}
```

`ocr` captures the same way `screenshot` does and hands the frame to `tesseract`, then copies the
text rather than the picture. The image is a means to an end, so it is not kept unless `save` asks:
what the user wanted is on the clipboard, and a screenshots folder filling up with pictures of text
is not a feature. `ocr_language` sets the default language, and several can be joined with `+` as
long as the data files are installed.

Text that comes back empty is a result, not a failure -- a region with nothing legible in it is a
thing that happens -- so it answers `ok` with `characters: 0`, and the notification says "No text
found" rather than claiming a copy that did not happen.

```bash
epochoxide api capture.record                                     # record a region
epochoxide api capture.record --params '{"mode":"fullscreen"}'
epochoxide api capture.recording                                  # what is running, if anything
epochoxide api capture.stopRecording
```

```json
{
  "recording": false,
  "cancelled": false,
  "mode": "fullscreen",
  "path": "/home/you/Videos/Recordings/recording-20260112-144233.mp4",
  "geometry": null,
  "output": "eDP-1",
  "seconds": 42,
  "bytes": 6815744,
  "notified": true
}
```

A recording is the one stateful thing in the capture group: it outlives the request that started
it, so the daemon holds the recorder rather than the connection, and only one runs at a time.
Starting a second one is refused with how long the first has been going rather than quietly
replacing it. All three methods answer in the shape above, so a shell polling `capture.recording`
for its indicator and a caller that just pressed stop read the same fields.

`stopRecording` sends SIGINT, which is what makes `wf-recorder` finalize the file instead of
abandoning it, and waits for the recorder to exit before reporting a size -- a recording announced
before it is written is a file the user opens to find truncated. A recorder that will not stop
within ten seconds is killed. Stopping when nothing is recording is not an error: a key bound to
"stop" pressed twice should say so quietly.

`capture.recording` also answers where the group is unavailable, and notices a recorder that died
on its own -- the disk filled, the output was unplugged -- so an indicator polling it stops
counting up against a dead process.

The filename's extension picks the container, so `recording_filename = "recording-%H%M%S.mkv"`
writes Matroska with no other change.

Recording happens at a constant `recording_framerate` (30 by default), which is a compatibility
setting rather than a quality one. Left to time itself, wf-recorder writes a stream declaring
90000fps, and x264 derives H.264 level 6.2 from that -- above what players will decode. The frames
inside are perfectly good; the video simply opens and shows black. Setting it to zero hands the
timing back to wf-recorder.

### Tailscale

```bash
epochoxide api tailscale.status
epochoxide api tailscale.machines
epochoxide api tailscale.up
epochoxide api tailscale.down
epochoxide api tailscale.pendingFiles
epochoxide api tailscale.receive --params '{"directory":"/home/you/Downloads"}'
epochoxide api tailscale.send --params '{"peer":"phone","files":["/home/you/notes.pdf"]}'
```

`status` normalizes backend state, tailnet, exit node, and health warnings. `machines` lists this
device first, then peers by name. `up` and `down` change the Tailscale backend state. `pendingFiles`
checks the Taildrop inbox without consuming it; `receive` moves waiting files into the selected
directory, renaming conflicts instead of overwriting.

One deliberate divergence from what Tailscale reports: for *this* device, `online` follows the
backend state rather than `Self.Online`, which Tailscale sets false whenever it cannot reach the
coordination server — even with the tailnet up. Showing the local machine as offline next to a
status of `Running` would be a contradiction, so the normalization resolves it.

### Nix

```bash
epochoxide api nix.status     # what the last check found; answers from memory
epochoxide api nix.check      # resolve every input now
epochoxide api nix.hosts
epochoxide api nix.update
epochoxide api nix.rebuild --params '{"host":"thor"}'
```

```json
{
  "flake": "/home/you/nixconfig",
  "available": true,
  "locked_at": 1788921488,
  "checked_at": 1788972912,
  "checking": false,
  "updates": 1,
  "inputs": [
    {
      "name": "nixpkgs",
      "kind": "github",
      "source": "github:NixOS/nixpkgs/nixos-unstable",
      "current_rev": "6aefcda940c5cbc9bce364fef6cd8fbb32e1e0d3",
      "current_date": 1788921488,
      "latest_rev": "d6524aaca2ff07876657ae2b323f24be4874944b",
      "latest_date": 1788881743,
      "update_available": true
    }
  ],
  "hosts": [{ "name": "thor", "rebuild": "rebuild-thor", "configured": true }]
}
```

**Checking never writes to your flake.** `nix flake update --output-lock-file` resolves every input
to what it would lock to today and writes that candidate lock to a temp file, which is deleted once
it has been read; `flake.lock` is left exactly as it was. The alternative -- copying the flake
somewhere and updating the copy -- is worse: the copy goes stale, and a `path:` input inside it
stops resolving.

Only the root flake's own inputs are compared. Those are what a person updates; a transitive input
moving on its own is invisible to `nix flake update` at this level too, and an input written as a
`follows` has no revision of its own to compare.

`status` answers from memory and is cheap enough to poll; `check` costs a network round trip per
input, so a timer in the daemon does it every `nix_check_interval_minutes` and the shell reads the
result. A notification goes out only when an input gains an update it did not have at the previous
check -- saying "23 updates available" every hour is how a notifier teaches people to ignore it.

Every host in a flake shares one `flake.lock`, so "which hosts have updates" has the same answer for
all of them. What differs per host is the command that rebuilds it, which is why `nix_hosts` pairs
each name with its own command:

```toml
[[nix_hosts]]
name = "thor"
rebuild = "rebuild-thor"
```

A rebuild is usually an alias or a script that already knows its target, so the command is stored
per host rather than derived from one template. Hosts read out of the flake's `nixosConfigurations`
that config did not name fall back to `nix_rebuild_command` with `%HOST%` substituted, and are
offered no action at all when that is empty. Nothing here has a default that changes a system.

### System

```bash
epochoxide api system.power
```

```json
{
  "available": true,
  "profile": "performance",
  "governor": "performance",
  "energy_preference": "performance",
  "turbo": false,
  "driver": "intel_pstate",
  "manager": "auto-cpufreq",
  "platform_profile": null,
  "can_switch": false
}
```

Everything comes from sysfs, so no daemon has to be installed and nothing needs root. `profile`
reduces the governor and the energy preference to one word: `performance` is a CPU that will not
clock down, while `powersave` is the governor every laptop idles at and says nothing on its own --
there, the energy preference is what separates `balanced` from `power-saver`. The raw knobs come
along because "balanced" explains nothing to someone who opened the panel because the fans are
loud.

`manager` names the daemon deciding it -- `auto-cpufreq`, `power-profiles-daemon`, `tuned`, or
nothing. It is reported rather than depended on: the numbers are true whether or not anything is
managing them.

```bash
epochoxide api system.hardware
epochoxide api system.firmware
epochoxide api system.firmware --params '{"refresh":true}'
epochoxide api system.stayAwake
epochoxide api system.setStayAwake --params '{"enabled":true,"reason":"presentation"}'
```

`hardware` names the machine from DMI and reports how the battery has worn -- full charge now
against full charge when new, plus the cycle count. That is the number nobody's desktop shows and
everybody wants, and it needs no vendor support: `framework: true` exists so a caller can say
"Framework Laptop 13" rather than to gate anything behind it.

Charge thresholds are reported when a machine exposes them through sysfs
(`charge_control_end_threshold`). Framework's live in the embedded controller behind
`/dev/cros_ec`, which is root-only, so they come back null there rather than behind a polkit prompt
for reading a number.

`firmware` asks fwupd what it is offering, cached for ten minutes because `fwupdmgr` talks to a
daemon and firmware does not change faster than that. It never downloads metadata and never
installs anything: flashing firmware is the user's own `fwupdmgr update`.

`setStayAwake` holds the machine out of idle and sleep by keeping a `systemd-inhibit` process
alive; omitting `enabled` toggles. Everything that idles a session watches logind, so that is where
the lock is taken -- and the shell adds a Wayland idle-inhibit against its own surface, which the
daemon cannot do because it has no surface.

`can_switch` is false, and this build never changes a governor. Switching is a different problem:
whatever daemon is managing the CPU puts its own decision back within seconds unless it is asked
through its own override, so a switch that writes sysfs directly would appear to work and then
quietly undo itself.

### LocalSend

```bash
epochoxide api localsend.devices
epochoxide api localsend.send --params '{"device":"Energetic Lettuce","files":["/home/you/notes.pdf"]}'
```

```json
{
  "alias": "Energetic Lettuce",
  "fingerprint": "7D73C6BF…",
  "device_model": "Linux",
  "device_type": "desktop",
  "ip": "10.0.10.13",
  "port": 53317,
  "protocol": "https",
  "download": false
}
```

LocalSend has no CLI to shell out to, so this speaks the v2 protocol directly. Discovery announces
this machine to the multicast group `224.0.0.167:53317` and collects the devices that answer; a
device's IP comes from the datagram it sent, never from the payload, so a device cannot claim to be
somewhere it is not.

The discovery socket sets `SO_REUSEADDR`/`SO_REUSEPORT`, because multicast has to be received on
the group's own port and the LocalSend desktop app is usually already bound to it -- which is
exactly when discovery needs to work.

Sharing that port has a consequence worth knowing: multicast is delivered to every socket joined to
the group, but a **unicast** reply goes to only one of them. With the receiver running there are two
sockets on that port, so a peer answering our announcement directly could be handed to the receiver
and never reach the scan. The receiver therefore records every device it hears, and `devices`
merges that in -- along with peers that announce over HTTP `register` rather than multicast, which a
scan never sees at all. Without this, other devices could see this machine while it saw none of
them.

**Trust.** LocalSend uses self-signed certificates and no CA: a device announces the SHA-256
fingerprint of its certificate, and identity is that fingerprint. Sending therefore pins it -- a
TLS connection is accepted only when the certificate presented hashes to the value the device
announced. Accepting any certificate, which is the easy path, would let anything on the LAN
impersonate a device and receive the files.

Sending is two steps: `prepare-upload` registers the transfer and returns a session plus a token
per file, then each file is uploaded with its token. `prepare-upload` does not answer until someone
accepts the transfer on the receiving device, so a send can sit waiting for a while; a timeout
there says so rather than reporting a bare network error.

### Receiving

```bash
epochoxide api localsend.status     # receiving?, alias, port, fingerprint, download directory
epochoxide api localsend.pending    # transfers waiting on a decision
epochoxide api localsend.accept  --params '{"session":"..."}'
epochoxide api localsend.accept  --params '{"session":"...","directory":"~/Pictures"}'
epochoxide api localsend.decline --params '{"session":"..."}'
epochoxide api localsend.received   # files accepted since the daemon started
epochoxide api localsend.stopReceiving   # release the port
epochoxide api localsend.startReceiving  # take it back
```

**Sharing the port with the LocalSend app.** Only one process can hold 53317, so running the
desktop app and this receiver at once means one of them loses -- and the loser still announces
itself, leaving other devices able to see a machine they cannot reach. Rather than negotiating for
the port, receiving can simply be switched off: `stopReceiving` drops the listener and frees 53317
for the app, `startReceiving` takes it back. The shell exposes this as a toggle in the LocalSend
panel. A port chosen by the OS was the alternative, but it changes on every restart, which no
firewall rule can follow.

The daemon runs an HTTPS server and answers discovery, so other devices can send to this machine
without the LocalSend app running here (the *sending* device still uses LocalSend, or anything else
speaking the v2 protocol). It is controlled by `localsend_receive`.

Files land in `localsend_download_dir` by default. `accept` takes an optional `directory` to send
one transfer somewhere else, so a UI can ask where it should go. A directory that does not exist is
refused rather than created, so a typo cannot quietly drop files somewhere nobody looks.

**Consent.** `prepare-upload` is held open until someone accepts through the shell, or until the
request expires after two minutes and is refused. Nothing reaches the disk before that: the file
name and size are known from the offer, but no bytes are requested until the transfer is accepted.
Only one transfer waits at a time; a second sender is told `Blocked by another session` rather than
being queued behind a prompt nobody has seen.

**Identity.** The certificate is generated once and kept under the data directory, because the
fingerprint a device announces *is* its identity in LocalSend and peers pin it -- regenerating per
run would look like a new device every time. The private key is written `0600`.

**File names come from the sender**, so they are reduced to their final path component before use:
a name like `../../.ssh/authorized_keys` lands in the download directory as `authorized_keys`. An
existing file is never overwritten; a counter is appended instead. An upload is refused unless its
token matches the one issued when the transfer was accepted.

**Port.** 53317 by default, falling back to whatever the OS gives if something already holds it --
the announcement carries the port actually in use, and senders honour it.

### Over the socket

```json
{"type":"api","method":"compositor.windows","params":{},"version":1}
```

`params` and `version` are optional. Responses use the usual envelope; a failure carries the
structured error in `data`:

```json
{"ok":false,"data":{"code":"unavailable","message":"...","detail":null},"error":"..."}
```

Error codes: `unknown_group`, `unknown_method`, `unavailable`, `invalid_params`, `backend_error`,
`version_mismatch`.

API calls are answered without waiting on the provider registry, so a cold daemon still building
its file index can answer `compositor.windows` immediately.

### Versioning

`version` asserts the contract major the caller was built against; a mismatch is refused rather
than served a shape the caller may not understand. The major changes when an existing method's
shape changes incompatibly — adding a group, method, or field is a minor bump.

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

- `wl-clipboard` for clipboard text/image capture, and for putting screenshots on the clipboard.
- `grim` and `slurp` for screenshots and region selection.
- `wf-recorder` for screen recording.
- `nix` for flake update checking, and a terminal for the update and rebuild actions.
- `libnotify` for `notify-send`, which announces a finished capture.
- `tesseract` for OCR, both on clipboard images and on `capture.ocr`.
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
