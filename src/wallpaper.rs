//! The desktop wallpaper: what is on it, what could be, and switching between them.
//!
//! This lives in the daemon rather than the shell for the same reason night mode and stay-awake
//! do: it is system state, and it has to be answerable and settable with the shell closed. It also
//! has to be restored before anything else draws, which only something started at session time can
//! do.
//!
//! Under Hyprland, applied through `hyprctl hyprpaper wallpaper ",<path>"`, which hyprpaper 0.8
//! switches on with no reload. Anywhere else -- niri -- hyprpaper still draws, but it turns its IPC
//! off at startup ("not running under hyprland, IPC will be disabled"), so nothing can switch it
//! live. There the switch goes through hyprpaper's config instead: when that config points at a
//! symlink, the link is repointed and hyprpaper restarted to read it again. See `Backend`.
//!
//! Two things about hyprpaper shape the rest of this file:
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
    pub available: bool,
    pub unavailable_reason: Option<String>,
}

/// The current wallpaper, remembered because nothing else can be asked.
static CURRENT: Mutex<Option<String>> = Mutex::new(None);

/// Installed once at startup, the way night mode's temperature is: `dispatch` has no registry to
/// read a config out of, so the settings this module needs are put somewhere it can reach.
static DIRECTORIES: OnceLock<Vec<String>> = OnceLock::new();
static FIT_MODE: OnceLock<String> = OnceLock::new();

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
    let wanted = config.wallpaper_fit_mode.clone();
    if !FIT_MODES.contains(&wanted.as_str()) {
        let why = if FIT_MODES_LOST.contains(&wanted.as_str()) {
            "hyprpaper 0.8.4 cannot stretch over IPC"
        } else {
            "not a fit mode"
        };
        eprintln!("wallpaper: {wanted:?}: {why}, hyprpaper will render \"cover\"");
    }
    let _ = FIT_MODE.set(wanted);
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

/// What is on screen. Falls back to the state file, which is what makes a one-shot CLI call
/// correct: `epochctl wallpaper next` run with no daemon has nothing in memory to step from, and
/// without this it would always step from the first image rather than the current one.
fn current() -> String {
    if let Some(held) = CURRENT.lock().unwrap().clone() {
        return held;
    }
    remembered().unwrap_or_default()
}

fn remember(path: &str) {
    let Some(target) = state_path() else { return };
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(target, format!("{path}\n"));
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
/// daemon outlives a logout and the next session may be the other compositor.
enum Backend {
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
    config_link().map(Backend::Link)
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
    Wallpaper {
        current: current(),
        directories: directories(),
        fit_mode: fit_mode(),
        available: backend.is_ok(),
        unavailable_reason: backend.err(),
        wallpapers,
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
