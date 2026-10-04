//! The desktop wallpaper: what is on it, what could be, and switching between them.
//!
//! This lives in the daemon rather than the shell for the same reason night mode and stay-awake
//! do: it is system state, and it has to be answerable and settable with the shell closed. It also
//! has to be restored before anything else draws, which only something started at session time can
//! do.
//!
//! Two things can draw it, chosen by `wallpaper_backend`:
//!
//!   * The shell itself, on a background layer surface. Nothing is applied here at all: a switch is
//!     remembered and announced over `wallpaper.subscribe`, and the shell draws whatever was
//!     announced. This works the same under every compositor and switches with no restart.
//!   * hyprpaper. Under Hyprland, applied through `hyprctl hyprpaper wallpaper ",<path>"`, which
//!     hyprpaper 0.8 switches on with no reload. Anywhere else -- niri -- hyprpaper still draws, but
//!     it turns its IPC off at startup ("not running under hyprland, IPC will be disabled"), so
//!     nothing can switch it live. There the switch goes through hyprpaper's config instead: when
//!     that config points at a symlink, the link is repointed and hyprpaper restarted to read it
//!     again.
//!
//! "auto", the default, uses hyprpaper when it is running and the shell otherwise -- so moving to
//! the shell is a matter of no longer starting hyprpaper. See `Backend`.
//!
//! Two things about hyprpaper shape the rest of this file, whichever backend is in use:
//!
//!   * Its `listactive` reports what was loaded at startup and does not follow a live switch, so it
//!     cannot answer "what is set now". This module remembers instead.
//!   * Its config lives wherever the session manager put it -- on a home-manager machine a
//!     read-only symlink into the nix store -- so the choice cannot be written back there. It goes
//!     in the daemon's own state file.
//!
//! Paths are used exactly as found rather than canonicalised. hyprpaper matches on the path it was
//! given, and a wallpaper directory full of symlinks into the store resolves to paths it will not
//! recognise.

use crate::config::Config;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Extensions worth offering. Deliberately short: these are things hyprpaper can actually load.
const EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp"];

/// How deep a wallpaper directory is searched. One level of subdirectory is enough to let someone
/// keep `wallpapers/nature` and `wallpapers/abstract` without turning this into a filesystem crawl.
const MAX_DEPTH: usize = 2;

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Wallpaper {
    /// Every image found, sorted, so a picker's tile positions stay put between openings.
    pub wallpapers: Vec<String>,
    /// What is on screen, as far as this daemon knows -- which is the only thing that does.
    pub current: String,
    pub directories: Vec<String>,
    pub fit_mode: String,
    /// What is drawing it: "shell" or "hyprpaper", or empty when neither can be used here. The
    /// shell draws only when this says "shell", so it never paints over a running hyprpaper.
    pub backend: String,
    pub available: bool,
    pub unavailable_reason: Option<String>,
}

/// The current wallpaper, remembered because nothing else can be asked.
static CURRENT: Mutex<Option<String>> = Mutex::new(None);

/// Installed once at startup, the way night mode's temperature is: `dispatch` has no registry to
/// read a config out of, so the settings this module needs are put somewhere it can reach.
static DIRECTORIES: OnceLock<Vec<String>> = OnceLock::new();
static FIT_MODE: OnceLock<String> = OnceLock::new();
static BACKEND: OnceLock<String> = OnceLock::new();

/// Values `wallpaper_backend` accepts.
const BACKENDS: &[&str] = &["auto", "shell", "hyprpaper"];

/// Fit modes the shell draws. Qt has a fill mode for each, stretch included, so none of
/// hyprpaper's caveats below apply when the shell is drawing.
const SHELL_FIT_MODES: &[&str] = &["cover", "contain", "tile", "stretch"];

/// Fit modes that reach the screen as themselves. Everything else hyprpaper renders as "cover"
/// without saying so, which makes a wrong value indistinguishable from the setting having no
/// effect -- worth one line at startup rather than an afternoon with a screenshot and a hash.
const FIT_MODES: &[&str] = &["cover", "contain", "tile"];

/// Modes hyprctl accepts and hyprpaper 0.8.4 then quietly turns into "cover". They are worth
/// naming separately: `fit_mode = fill` in a hyprpaper.conf really does stretch, so the reasonable
/// assumption is that a switch can be made to match it, and it cannot.
const FIT_MODES_LOST: &[&str] = &["stretch", "fit", "fill"];

pub fn configure(config: &Config) {
    let _ = DIRECTORIES.set(config.wallpaper_dirs.clone());

    let mut backend = config.wallpaper_backend.clone();
    if !BACKENDS.contains(&backend.as_str()) {
        eprintln!("wallpaper: backend {backend:?}: not one of {BACKENDS:?}, using \"auto\"");
        backend = "auto".into();
    }

    // Warned about once, for the backend that will actually see the value. The shell's list is a
    // superset of hyprpaper's, so "shell" only complains about a word nothing understands.
    let wanted = config.wallpaper_fit_mode.clone();
    if !SHELL_FIT_MODES.contains(&wanted.as_str()) {
        eprintln!("wallpaper: {wanted:?}: not a fit mode, it will render as \"cover\"");
    } else if backend != "shell" && !FIT_MODES.contains(&wanted.as_str()) {
        let why = if FIT_MODES_LOST.contains(&wanted.as_str()) {
            "hyprpaper 0.8.4 cannot stretch over IPC"
        } else {
            "not a fit mode"
        };
        eprintln!("wallpaper: {wanted:?}: {why}, hyprpaper will render \"cover\"");
    }
    let _ = FIT_MODE.set(wanted);
    let _ = BACKEND.set(backend);
}

fn backend_setting() -> String {
    BACKEND
        .get()
        .cloned()
        .unwrap_or_else(|| Config::default().wallpaper_backend)
}

fn directories() -> Vec<String> {
    DIRECTORIES
        .get()
        .cloned()
        .unwrap_or_else(|| Config::default().wallpaper_dirs)
}

fn fit_mode() -> String {
    FIT_MODE
        .get()
        .cloned()
        .unwrap_or_else(|| Config::default().wallpaper_fit_mode)
}

fn state_path() -> Option<PathBuf> {
    dirs::state_dir().map(|d| d.join("epochshell/wallpaper"))
}

fn remembered() -> Option<String> {
    let path = state_path()?;
    let saved = std::fs::read_to_string(path).ok()?;
    let saved = saved.trim();
    if saved.is_empty() {
        return None;
    }
    Some(saved.to_string())
}

/// What is on screen. The state file comes first: it is what a one-shot CLI call writes, so a
/// `epochctl wallpaper set` that ran with no daemon is still the truth once the daemon is back.
/// Memory only answers when the file cannot be read -- no state directory, say -- so a switch
/// still holds for the life of the process.
fn current() -> String {
    remembered()
        .or_else(|| CURRENT.lock().unwrap().clone())
        .unwrap_or_default()
}

/// Write the choice down, atomically: `wallpaper.subscribe` watches this file, and a plain write
/// truncates before it writes, which a watcher would read as the wallpaper being unset.
fn remember(path: &str) {
    let Some(target) = state_path() else { return };
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let staging = target.with_file_name(".wallpaper.epochoxide");
    if std::fs::write(&staging, format!("{path}\n")).is_ok() {
        let _ = std::fs::rename(&staging, &target);
    }
}

/// Walk one directory for images, following symlinks: a home-manager wallpaper directory is
/// entirely links into the nix store, and a scan that does not follow them finds nothing.
fn scan_dir(dir: &Path, depth: usize, found: &mut Vec<String>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // metadata() follows symlinks where symlink_metadata() would not.
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            scan_dir(&path, depth + 1, found);
            continue;
        }
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        if EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()) {
            found.push(path.display().to_string());
        }
    }
}

fn scan() -> Vec<String> {
    let mut found = Vec::new();
    for dir in &directories() {
        scan_dir(Path::new(dir), 1, &mut found);
    }
    found.sort();
    found.dedup();
    found
}

/// The unit hyprpaper runs as. Restarting it is how a switch lands when its IPC is off.
const HYPRPAPER_UNIT: &str = "hyprpaper.service";

/// How long switching has to go quiet before hyprpaper is restarted. Clicking through the picker
/// otherwise restarts it once per click, each one a flicker -- and enough of them inside systemd's
/// start limit (5 in 10s) leaves the unit failed and the desktop with no wallpaper at all.
const RESTART_SETTLE: Duration = Duration::from_millis(500);

/// Whether restarts may be deferred. Only the daemon lives long enough to do one later; a one-shot
/// CLI call that deferred would exit first and never restart anything.
static DEFER_RESTARTS: AtomicBool = AtomicBool::new(false);

/// Bumped by every switch. A deferred restart only goes ahead if no switch came after it.
static RESTART_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Called by the daemon at startup: from here on, restarts wait for switching to settle.
pub fn defer_restarts() {
    DEFER_RESTARTS.store(true, Ordering::SeqCst);
}

/// How a switch reaches the screen. Decided per call rather than once at startup, because the
/// daemon outlives a logout and the next session may be the other compositor -- or may not have
/// started hyprpaper at all.
enum Backend {
    /// The shell draws it. Nothing to apply; the switch is announced and the shell follows.
    Shell,
    /// Hyprland is running, so hyprpaper's IPC is up and a switch is instant.
    Ipc,
    /// hyprpaper's IPC is off, but its config draws from this symlink: repoint it and restart.
    Link(PathBuf),
}

fn hyprpaper_config() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("hypr/hyprpaper.conf"))
}

/// The image path hyprpaper.conf names, from either dialect: 0.8's `wallpaper { path = ... }`
/// block, or the older one-line `wallpaper = monitor,path`. The first one wins -- a config with a
/// wallpaper per monitor cannot be switched as a whole through one link anyway.
fn configured_path(conf: &str) -> Option<String> {
    for line in conf.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        let path = match key.trim() {
            "path" => value,
            "wallpaper" => value.split_once(',').map(|(_, p)| p.trim()).unwrap_or(""),
            _ => continue,
        };
        // Old-style values may carry a fit mode as a prefix: `contain:/path`.
        let path = path.rsplit_once(':').map(|(_, p)| p).unwrap_or(path);
        if !path.is_empty() {
            return Some(shellexpand::tilde(path).into_owned());
        }
    }
    None
}

/// The symlink hyprpaper.conf draws from, if it draws from one. A plain file cannot be used: the
/// image itself would have to be overwritten, and that is the user's picture, not this daemon's.
fn config_link() -> Result<PathBuf, String> {
    let conf = hyprpaper_config().ok_or("no config directory")?;
    let text = std::fs::read_to_string(&conf)
        .map_err(|_| format!("hyprpaper's IPC is off outside Hyprland and {} cannot be read", conf.display()))?;
    let path = configured_path(&text)
        .ok_or_else(|| format!("{} names no wallpaper path", conf.display()))?;
    let path = PathBuf::from(path);
    let is_link = std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink());
    if !is_link {
        return Err(format!(
            "hyprpaper's IPC is off outside Hyprland, and the path in hyprpaper.conf ({}) is not a symlink that can be repointed",
            path.display()
        ));
    }
    Ok(path)
}

fn backend() -> Result<Backend, String> {
    match backend_setting().as_str() {
        "shell" => Ok(Backend::Shell),
        "hyprpaper" => hyprpaper_backend(),
        // Whichever is actually here. hyprpaper running means someone wants it drawing, and the
        // shell painting a second wallpaper under it would only waste memory.
        _ if hyprpaper_running() => hyprpaper_backend(),
        _ => Ok(Backend::Shell),
    }
}

/// Whether a hyprpaper process exists, however it was started: a Hyprland `exec-once` is as
/// common as the systemd unit, so asking systemd alone would miss half of them.
fn hyprpaper_running() -> bool {
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return false;
    };
    procs.flatten().any(|entry| {
        std::fs::read_to_string(entry.path().join("comm"))
            .is_ok_and(|comm| comm.trim() == "hyprpaper")
    })
}

fn hyprpaper_backend() -> Result<Backend, String> {
    if crate::compositor::ipc::hypr_running() {
        return if which::which("hyprctl").is_ok() {
            Ok(Backend::Ipc)
        } else {
            Err("hyprctl is not installed".to_string())
        };
    }
    if which::which("systemctl").is_err() {
        return Err("hyprpaper's IPC is off outside Hyprland, and systemctl is not here to restart it".into());
    }
    // The shell is the easy way out of every refusal below, so say so.
    config_link()
        .map(Backend::Link)
        .map_err(|why| format!("{why} (or set wallpaper_backend = \"shell\" and stop hyprpaper)"))
}

/// Why this group might not work here. A machine where it cannot still be asked what images
/// exist, which is why `wallpaper.status` answers regardless.
pub fn available() -> Result<(), String> {
    backend().map(|_| ())
}

/// Ask hyprpaper to switch.
///
/// The request is `monitor,path,fit_mode` -- three comma-separated fields. An empty monitor means
/// every output. The fit mode has to travel with the switch: hyprpaper's config carries one per
/// declared wallpaper, so an image set over IPC inherits nothing and would be fitted by whatever
/// the default happens to be.
///
/// Worth knowing if this ever looks wrong: the mode is NOT a prefix on the path. `contain:/foo.jpg`
/// is rejected as a bad path, which is an easy thing to reach for and an easy error to misread.
fn apply(path: &str) -> Result<()> {
    match backend().map_err(anyhow::Error::msg)? {
        Backend::Shell => Ok(()),
        Backend::Ipc => apply_ipc(path),
        Backend::Link(link) => {
            relink(&link, path)?;
            schedule_restart()
        }
    }
}

fn apply_ipc(path: &str) -> Result<()> {
    let output = crate::compositor::ipc::hyprctl()
        .args([
            "hyprpaper",
            "wallpaper",
            &format!(",{path},{}", fit_mode()),
        ])
        .output()?;

    // hyprctl exits 0 and says nothing on success; anything on stdout is the failure.
    let said = String::from_utf8_lossy(&output.stdout);
    let said = said.trim();
    if !said.is_empty() {
        bail!("hyprpaper refused: {said}");
    }
    Ok(())
}

/// Point `link` at `target`, atomically: a new link beside it renamed over the old one, so a
/// hyprpaper starting at the wrong moment never finds the path missing.
fn relink(link: &Path, target: &str) -> Result<()> {
    let name = link.file_name().and_then(|n| n.to_str()).unwrap_or("wallpaper");
    let staging = link.with_file_name(format!(".{name}.epochoxide"));
    let _ = std::fs::remove_file(&staging);
    std::os::unix::fs::symlink(target, &staging)?;
    std::fs::rename(&staging, link)?;
    Ok(())
}

/// Whether the unit is one a restart should touch. "failed" counts: that is exactly the state a
/// run of restarts leaves behind, and restarting is what gets it out. Only "inactive" -- nobody
/// started it this session -- is left alone.
fn unit_running() -> bool {
    let state = Command::new("systemctl")
        .args(["--user", "show", "-p", "ActiveState", "--value", HYPRPAPER_UNIT])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    matches!(state.as_str(), "active" | "activating" | "reloading" | "failed")
}

/// Restart hyprpaper once switching settles, or straight away when nothing will be around later.
/// Whether the unit is running is checked now, so that refusal still reaches the caller.
fn schedule_restart() -> Result<()> {
    if !unit_running() {
        bail!("{HYPRPAPER_UNIT} is not running; the wallpaper will show when it starts");
    }
    let mine = RESTART_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    if !DEFER_RESTARTS.load(Ordering::SeqCst) {
        return restart_hyprpaper();
    }
    std::thread::spawn(move || {
        std::thread::sleep(RESTART_SETTLE);
        if RESTART_GENERATION.load(Ordering::SeqCst) != mine {
            return;
        }
        if let Err(err) = restart_hyprpaper() {
            eprintln!("wallpaper: {err:#}");
        }
    });
    Ok(())
}

/// Make hyprpaper read its config again. Only the systemd unit is restarted: a hyprpaper started
/// some other way is not this daemon's to kill, and one it respawned itself would die with the
/// daemon's cgroup.
///
/// `reset-failed` first clears systemd's start-rate counter, so steady switching -- each one past
/// the settle time -- cannot add up to a unit systemd refuses to start again.
fn restart_hyprpaper() -> Result<()> {
    let _ = Command::new("systemctl")
        .args(["--user", "reset-failed", HYPRPAPER_UNIT])
        .status();
    let output = Command::new("systemctl")
        .args(["--user", "restart", HYPRPAPER_UNIT])
        .output()?;
    if !output.status.success() {
        bail!(
            "restarting {HYPRPAPER_UNIT} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

pub fn status() -> Wallpaper {
    let wallpapers = scan();
    let backend = backend();
    let name = match &backend {
        Ok(Backend::Shell) => "shell",
        Ok(_) => "hyprpaper",
        Err(_) => "",
    };
    Wallpaper {
        current: current(),
        directories: directories(),
        fit_mode: fit_mode(),
        backend: name.into(),
        available: backend.is_ok(),
        unavailable_reason: backend.err(),
        wallpapers,
    }
}

/// How often `watch` sends the status whether or not anything changed. It is the only way a
/// stream notices a client that has gone -- a write is what fails -- and it doubles as the rescan
/// that picks up images dropped into a wallpaper directory.
const WATCH_HEARTBEAT: Duration = Duration::from_secs(120);

/// Stream the status: once now, again whenever the choice changes, and on the heartbeat.
///
/// Changes are noticed by watching the state file rather than by hooking `set`, because the file
/// is written by more than this process: a one-shot `epochctl wallpaper set` with no daemon
/// writes it too, and the shell drawing the wallpaper has to follow that just the same.
pub fn watch(mut emit: impl FnMut(&Wallpaper) -> Result<()>) -> Result<()> {
    use notify::Watcher;

    let Some(state) = state_path() else {
        bail!("no state directory to watch");
    };
    let Some(dir) = state.parent() else {
        bail!("no state directory to watch");
    };
    std::fs::create_dir_all(dir)?;

    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(tx)?;
    // The directory, not the file: `remember` renames a new file over the old one, which a watch
    // on the file itself would lose track of after the first switch.
    watcher.watch(dir, notify::RecursiveMode::NonRecursive)?;

    let mut last = status();
    emit(&last)?;
    loop {
        match rx.recv_timeout(WATCH_HEARTBEAT) {
            Ok(Ok(event)) => {
                if !event.paths.iter().any(|p| p == &state) {
                    continue;
                }
                let now = status();
                if now != last {
                    last = now;
                    emit(&last)?;
                }
            }
            Ok(Err(err)) => eprintln!("wallpaper: watching {}: {err}", dir.display()),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                last = status();
                emit(&last)?;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => bail!("the state file watcher stopped"),
        }
    }
}

pub fn set(path: &str) -> Result<Wallpaper> {
    let wanted = path.trim();
    if wanted.is_empty() {
        bail!("no wallpaper path given");
    }
    if !Path::new(wanted).exists() {
        bail!("no such image: {wanted}");
    }

    apply(wanted)?;
    *CURRENT.lock().unwrap() = Some(wanted.to_string());
    remember(wanted);
    Ok(status())
}

/// Step through the list. `step` is signed, so -1 goes back.
pub fn step(step: i64) -> Result<Wallpaper> {
    let list = scan();
    if list.is_empty() {
        bail!("no images in {}", directories().join(", "));
    }

    let at = list.iter().position(|p| *p == current()).unwrap_or(0) as i64;
    let len = list.len() as i64;
    // rem_euclid so a negative step wraps to the end rather than panicking on a negative index.
    let to = (at + step).rem_euclid(len) as usize;

    set(&list[to])
}

/// Put back whatever was chosen last session. Called once at startup: hyprpaper starts from its own
/// config and knows nothing about the choice, so without this every reboot silently reverts it.
///
/// Quiet about everything. A missing state file is the ordinary case on a fresh install, and an
/// image that has since been deleted is not worth refusing to start over.
pub fn restore() {
    let Some(saved) = remembered() else { return };
    if !Path::new(&saved).exists() {
        return;
    }
    let restored = match backend() {
        // The shell asks for the status when it connects, which is all the restoring it needs.
        Ok(Backend::Shell) => true,
        Ok(Backend::Ipc) => apply_ipc(&saved).is_ok(),
        // A link already pointing at the choice needs no restart -- hyprpaper drew it at startup.
        // Otherwise repoint it; restarting is best-effort, since hyprpaper may simply not have
        // started yet, and then it reads the new link when it does.
        Ok(Backend::Link(link)) => {
            if std::fs::read_link(&link).is_ok_and(|t| t == Path::new(&saved)) {
                true
            } else {
                relink(&link, &saved).is_ok() && {
                    if unit_running() {
                        let _ = restart_hyprpaper();
                    }
                    true
                }
            }
        }
        Err(_) => false,
    };
    if restored {
        *CURRENT.lock().unwrap() = Some(saved);
    }
}

#[cfg(test)]
mod tests {
    use super::configured_path;

    #[test]
    fn reads_block_dialect() {
        let conf = "wallpaper {\n  monitor=\n  path=/a/current # chosen\n}\nipc=on\n";
        assert_eq!(configured_path(conf).as_deref(), Some("/a/current"));
    }

    #[test]
    fn reads_line_dialect() {
        let conf = "preload = /x.png\nwallpaper = DP-1, contain:/a/b.png\n";
        assert_eq!(configured_path(conf).as_deref(), Some("/a/b.png"));
    }

    #[test]
    fn none_without_a_path() {
        assert_eq!(configured_path("ipc=on\nsplash=false\n"), None);
    }
}
