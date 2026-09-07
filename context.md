# EpochOxide — Project Context

EpochOxide is a fast Rust data provider daemon for Linux desktop shells, launchers, and panels. This document records the work completed so far, the current architecture, how to verify it, and what remains.

## Goal (original ask)

| # | Feature | Status |
|---|---------|--------|
| 1 | Provider capability metadata so UIs can adapt | Done — `ProviderCapability`/`ActionCapability` surfaced via `Providers` request. **Protocol change:** `Item` no longer carries a duplicated `action_capabilities` map (was cloned into every item, same data for every item from a provider); clients join `item.actions` (names) against the cached `Providers` capability list by `item.provider` |
| 2 | Streaming results so fast providers render before slower ones | Done, and now genuinely concurrent — `query_batches` runs each selected provider on its own thread and streams `(provider, items)` back over an `mpsc` channel in **completion order**, not registration order. Verified live: `files` (450k-entry scan, ~150ms) registered before `calc`/`runner` but arrives last in the streamed output because it finishes last |
| 3 | Subscriptions/events so UIs get updates without polling | Done — `Subscribe` request, 250ms poll loop, `index_changed` event verified |
| 4 | Incremental file indexing via inotify | Done — `notify` watcher, incremental updates, `events()` + `changed` flag |
| 5 | Persistent index DB for instant cold start | Done — JSON cache with lazy background load (see known issues) |
| 6 | Icon/theme resolution and thumbnail cache | Done — `src/icons.rs`, `convert`/`rsvg-convert` thumbnails |
| 7 | Per-provider enable/disable and ranking weights | Done and verified — `provider_enabled`, `provider_weights` |
| 8 | Query prefixes (`>`, `?`, `:`, `/`, `#`, `@`) | Done and verified — **changeable in Nix** (HM + NixOS modules) |

All build clean (`cargo build`, zero `clippy` warnings), 12/13 tests pass (1 `#[ignore]`d perf test), `nix build` succeeds.

## Perf/idiom pass (this session)

Full read-through for idiomatic Rust + performance, a synthetic-corpus perf harness (`providers::files::perf`, `#[ignore]`d, `EO_PERF_FILES=n cargo test --release providers::files::perf -- --ignored --nocapture`), and fixes. Corpus: 450k files spread across a synthetic nested tree (roughly the scale of the 686k-entry real index that motivated known-issue #1 below).

- **Files indexing was doing O(files × ignored_dirs) allocation** — `expanded_ignored_dirs()` (shellexpand + `format!` calls) was recomputed from scratch for *every file visited*, called from inside `add_path` on every walkdir entry. Precomputed once into a `FilesProvider::ignored_dirs` field; removed the now-redundant check inside `add_path` (both call sites already filter before calling it). Also fixed `is_ignored_path` to match whole path components via `Path::starts_with`/`components()` instead of raw substring `contains()` — that substring check was also a **correctness bug** (a dir literally named e.g. `targeting-app` was wrongly ignored because it contains `"target"`).
- **`remove_path` allocated a `String` per entry in the whole index** on every single removal (`display.clone() + "/"` was inside the `retain` closure). Hoisted out of the closure.
- **`drain_events()` wrote the full JSON index to disk on every inotify batch**, synchronously, while holding what was then a single global registry lock. Added a 3s save debounce (`dirty` flag + `last_saved`); event *notifications* to subscribers stay immediate, only the on-disk persistence batches.
- **Fuzzy query is now bitmask-prefiltered**: each indexed file stores a 64-bit "characters present" mask (`fuzzy::mask`, hashed byte→bit, zero false negatives — pure necessary-condition prefilter, never changes the match set) computed at index time; the query computes its own mask once, and `f.mask & query_mask != query_mask` skips the full character-by-character scan for any candidate missing a required byte.
- **`ProviderCapability` (a `HashMap<String,ActionCapability>` + several `String`s) was rebuilt from scratch on every query call** via `provider.capability()`. Cached once in `Registry::new()`.
- **Icon/thumbnail resolution ran on every per-provider result before the final cross-provider truncate** discarded most of them (up to `limit × providers` did the work, only `limit` survived). Score-affecting work (weight, history bonus) stays pre-truncate; icon/thumbnail decoration moved to after it.

Before/after on the 450k-file corpus (release build):

| Metric | Before | After |
|---|---|---|
| Cold reindex | 3.27s (146k files/sec) | 1.37s (349k files/sec) |
| Mass `remove_path` (drop whole index) | 134ms | ~65-90ms |
| Query `"lib"` (many matches) | 235ms | 151ms |
| Query `"conf-99"` (selective) | 119ms | 24ms |
| Query `"nomatchxyz"` (no match) | 87ms | 3ms |

**Concurrency redesign**: `Registry` was `Arc<Mutex<Registry>>` — one global lock serialized *every* query, activate, menu, providers, and the 250ms subscribe-poll across *all* connected clients, regardless of which provider they touched. Now each provider is `Arc<Mutex<Box<dyn Provider>>>` individually, history is `Arc<RwLock<UsageHistory>>` (concurrent reads for the common `apply()` path, exclusive only for `record()`), config is `Arc<Config>`, and `Registry` methods take `&self` — no outer lock at all. `query()` runs providers in parallel via `thread::scope` and joins; `query_batches()` spawns one thread per provider and streams results back over an `mpsc::Receiver` as each finishes (see goal #2 above). `server.rs`'s `Startup` now holds `Arc<Registry>` directly instead of `Arc<Mutex<Registry>>`.

Not changed (flagged as a real remaining scaling ceiling, not a bug): `FilesProvider::query` is still a linear scan over every indexed entry. The bitmask prefilter helps a lot for selective/no-match queries but a query like `"lib"` that many entries genuinely contain still costs ~150ms at 450k+ entries. Fixing that further means a real search structure (trigram/prefix index) or capping the live in-memory set — a bigger change than this pass.

## Architecture

Binary: `epochoxide` v0.1.0. Unix-socket daemon with plain TCP-less JSON-over-socket protocol.

```
src/
  main.rs        CLI: serve / query / activate / list-providers / menu / subscribe / service
  server.rs      daemon: threads per client, streaming, subscribe, lazy registry init
  client.rs      StreamClient: EAGAIN retry, stream_query(), subscribe(), request()
  config.rs      Config, partial-merge TOML load, defaults, expand_paths
  types.rs       Item, ProviderCapability, ActionCapability, ItemType, FuzzyInfo
  providers/
    mod.rs       Provider trait, Registry (query/sort/route/events/batches; per-provider Arc<Mutex<..>>, no outer lock)
    apps.rs      XDG .desktop files
    files.rs     file index + inotify watcher + persistent cache + events
    runner.rs    $PATH + custom commands
    clipboard.rs text/image history + OCR + edit
    windows.rs   open-window search (Hyprland/Sway/Niri/wmctrl)
    calc.rs      qalc + local arithmetic fallback
    menus.rs     TOML menus
  icons.rs       icon resolution + thumbnail cache
  history.rs     persistent usage history (recency + count ranking)
  fuzzy.rs       fuzzy matcher
  service.rs     systemd user service install
```

## Server protocol

One JSON request per line, one or more JSON responses per line.

- `Query` with `stream: true` → one `query_batch` line per provider, then a `done` line. Non-stream → a single response with all items.
- `Activate` → `{ok:true}` / error.
- `Providers` → full `ProviderCapability` list (including resolved `prefixes`).
- `Menu {menu}` → menu items.
- `Subscribe {providers}` → initial `{"type":"subscribed","providers":[...]}` ack, then `{"type":"event","event":{...}}` lines as events arrive. Server polls `registry.events()` on a 250ms loop. Only files emits events today (`index_changed`); others advertise `supports_subscriptions: false`.
- `Item` does **not** carry `action_capabilities` (removed — was a duplicated `HashMap<String,ActionCapability>` clone per item, identical across every item from a provider). It only carries `actions: Vec<String>` (names). Clients fetch `Providers` once and look up `providers[item.provider].actions[name]` for the full metadata.

Client `StreamClient`:
- `set_read_timeout(1.5s)` and retries on `WouldBlock`/`TimedOut`, so queries issued while the registry is still cold-building do not fail.
- `stream_query()` returns `Vec<(String, Vec<Value>)>`.
- `subscribe()` returns an iterator of events.

## Lazy registry init (important fix)

Originally `serve()` built `Registry::new(config)` *before* binding the socket. The files provider parsed the persistent index synchronously, so a large `~/.cache/epochoxide/file-index.json` (you had 379MB / 686k entries) delayed socket bind by >20s — the daemon appeared hung.

Now:
- `server::serve(socket, build: impl FnOnce() -> Result<Registry> + Send + 'static)`
- Socket binds first (create dirs, remove stale socket) → daemon answers connections in ~200ms.
- `build()` runs in a background thread; result stored in `Startup { registry: Mutex<Option<Result<Arc<Registry>, String>>>, ready: Condvar }`. (No inner `Mutex<Registry>` — `Registry` itself is `Send + Sync` via per-provider `Arc<Mutex<..>>` fields, see the perf/idiom pass above.)
- Each request thread calls `Startup::wait_registry()` and blocks (condvar) until ready. Client EAGAIN-retry makes this transparent.

## Config

`~/config/epochoxide/config.toml`; partial-merge semantics (only keys present are overridden; maps `extend`, not replace).

```toml
[provider_enabled]   # apps files runner clipboard windows calc menus  (all true default)
[provider_weights]   # apps=20000 runner=12000 calc=10000 windows=6000 menus=2000 files=0 clipboard=0
[query_prefixes]     # ">"=runner "/"=files "#"=clipboard "@"=windows ":"=menus "?"=calc
thumbnail_cache_enabled = true   # default true
```

- Prefix routing: longest-prefix match, then `trim_start()`; routed to exactly one provider when no explicit `--providers`. If prefixes collide, longest sorted first in `route_query`.
- Defaults: `socket` = `$XDG_RUNTIME_DIR/epochoxide.sock` (fallback `/tmp/epochoxide.sock`); `persistent_index = true`; `icon_cache_dir` = `~/.cache/epochoxide/icons`.

## Icons / thumbnails (`src/icons.rs`)

- `resolve(icon, config)`: absolute path → itself; else search `~/.local/share/icons` then `icon_cache_dir` then `/usr/share/icons`, `/usr/share/pixmaps`, trying `{icon}.png|svg|xpm`, depth-4 recursive.
- `thumbnail(source, config)`: no-op unless `thumbnail_cache_enabled` and dir exists; checks cached `<stamp>.png` (FNV1a hash); else renders via shell-out — SVG→`rsvg-convert -w 48 -h 48`, others→`convert -thumbnail 48x48 -background none`; graceful `None` on tool/path failure (no hard dependency).
- Registry `query()`/`query_batches()` populate `Item.icon_path` and `Item.thumbnail` **after** the score-affecting work (weight, history bonus) and the truncate to `limit` — so icon/thumbnail resolution only runs on items that actually make it into the response, not every per-provider candidate.

## History ranking (`src/history.rs`)

`~/.cache/epochoxide/history.json` (`provider:identifier` → `{count, last_used_epoch}`).

- On query: `count_bonus = min(count,50) * 250`, `recency_bonus` by age buckets (≤1h:10k, ≤1d:5k, ≤7d:2k, ≤30d:750, else 100), +500 if non-empty query; pushes `"history"` into `Item.state`.
- On activate: `record()` increments count and saves.

## Nix

- `flake.nix` exposes `packages.default`, `homeManagerModules.default`, `nixosModules.default`.
- Both modules write the config via `pkgs.formats.toml`: `tomlFormat.generate "epochoxide-config.toml" (defaultSettings // cfg.settings)`. **`provider_enabled`, `provider_weights`, `query_prefixes`, `thumbnail_cache_enabled` all ship as defaultSettings in both modules**, so `programs.epochoxide.settings.query_prefixes = { ... }` / `services.epochoxide.settings.query_prefixes` fully overrides (whole map replaced; partial-merge only affects direct config.toml loading).
- NixOS module also wires `services.epochoxide.settings` plus systemd user service with `--socket %t/epochoxide.sock` and `PATH` from `runtimePackages` (wl-clipboard, xclip, xdg-utils, wmctrl, tesseract, libqalculate).

## Verification so far (must repeat any time something changes)

- `cargo build` and `cargo test` (12 passed, 1 `#[ignore]`d perf test) pass. `cargo clippy --all-targets` is clean (zero warnings in our code).
- `nix build .` succeeds **only if all source files are git-tracked** — flakes ignore untracked files. We hit exactly this: `src/history.rs`, `src/icons.rs`, `src/providers/runner.rs` were untracked → "file not found for module icons/runner" in the Nix build. Fixing = `git add` them. This is a recurring trap for any new `src/*.rs`.
- Streaming verified: `query --stream` prints per-provider batches; server emits `batched` provider lists; `done` terminates. **Concurrency verified live**: with `files` pointed at ~450k real files (slow, ~150ms) alongside `calc`/`runner` (near-instant) and `files` registered *before* both in `Registry::new`, the streamed output still arrived `calc, runner, files` — genuine completion-order streaming, not registration order.
- Prefix routing verified live: `>…`→runner, `?…`→calc (computed `^^ 2+3*4` → `2+3*4 = 14`), `@…`→windows, `!…`→files (custom), `~…`→runner (custom).
- Weights verified live: `apps=false` in config fully removes apps results; `files=100` vs `files=-10000` flipped files first→last in the merged result order.
- Subscriptions verified live: with fresh `/tmp` config (`persistent_index=false`, `file_roots=["/tmp/ev-root"]`), `subscribe` printed `{"type":"subscribed"}` ack, then `{"event":{"kind":"index_changed","provider":"files"}}` after creating a file. Re-verified after adding the save debounce: a burst of 5 file creates 400ms apart still produced 5 immediate subscribe events, while the on-disk cache was rewritten only once (not 5×).
- Nix-generated TOML verified: `pkgs.formats.toml` round-trip of custom `query_prefixes`/`provider_enabled` produces the exact TOML the daemon consumes.

## Known issues / follow-ups

1. **Cold start / query latency at scale**: the persistent index (379MB/686k entries in your cache) is loaded in the background after bind, so the *first* query can block on registry build (bound by server, client retries hide it). Reindex speed and no-match/selective-query latency improved substantially this session (see perf pass above), but a query that many entries genuinely match (e.g. a common short substring) is still a ~150ms linear scan at 450k+ entries — a real search structure (trigram/prefix index) or capping the live set would be the next step, not attempted here.
2. **Nix untracked-file trap**: any new source file must be `git add`ed or `nix build` breaks.
3. **Thumbnail cache** is only exercised if `convert` or `rsvg-convert` exists; fallback returns `None` silently. Not covered by unit tests.
4. README's "Nix" section doesn't yet document `stream`/`subscribe`; the Socket Protocol section now documents the `action_capabilities` removal (see this session's protocol change above). config.example.toml documents all current keys.
5. `result` symlink is tracked in git status (should be gitignored).
6. **Breaking wire-format change this session**: `Item.action_capabilities` was removed. Any UI/client code already reading that field per-item needs to switch to joining `item.actions` against the `Providers` capability list.

## Useful test setup

- Socket pinned configs live in `/tmp`: e.g. `eo-prefix.toml` (`socket=/tmp/epochoxide.sock`, custom `query_prefixes`), `ev2.toml` (`file_roots=/tmp/ev-root`, `persistent_index=false`, files only) — proven reliably reproducible.
- To kill a stray daemon by socket (avoids `pgrep -f` matching y/pkill/shell itself): `fuser -k /tmp/epochoxide.sock`.
- Diagnostic: tiny `file_roots` and `persistent_index=false` isolate the files provider; any index work is then in-RAM only.

## Project conventions

- No code comments unless asked.
- TOML camel-case keys match struct fields; `PartialConfig` mirrors `Config` with `Option` fields; partial-merge means config maps are `extend`ed (so per-provider overrides are impossible from config.toml — only whole-map replacement).
- Streaming/subscription protocol strives for compatibility: unknown event kinds are ignored by readers; `typeless` `query_batch`/`done` handled by `stream_query`.