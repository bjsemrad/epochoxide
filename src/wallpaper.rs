//! The desktop wallpaper: what is on it, what could be, and switching between them.
//!
//! This lives in the daemon rather than the shell for the same reason night mode and stay-awake
//! do: it is system state, and it has to be answerable and settable with the shell closed. It also
//! has to be restored before anything else draws, which only something started at session time can
//! do.
//!
//! Applied through `hyprctl hyprpaper wallpaper ",<path>"`, which hyprpaper 0.8 switches on with no
//! reload. Two things about hyprpaper shape the rest of this file:
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
use std::sync::{Mutex, OnceLock};

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

pub fn configure(config: &Config) {
    let _ = DIRECTORIES.set(config.wallpaper_dirs.clone());
    let _ = FIT_MODE.set(config.wallpaper_fit_mode.clone());
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

fn hyprctl_available() -> bool {
    which::which("hyprctl").is_ok()
}

/// Why this group might not work here. Switching is done by asking hyprpaper through hyprctl, so
/// that is the whole dependency -- a machine without it can still be asked what images exist, which
/// is why `wallpaper.status` answers regardless.
pub fn available() -> Result<(), String> {
    if hyprctl_available() {
        Ok(())
    } else {
        Err("hyprctl is not installed".to_string())
    }
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
    let output = Command::new("hyprctl")
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

pub fn status() -> Wallpaper {
    let wallpapers = scan();
    let available = hyprctl_available();
    Wallpaper {
        current: current(),
        directories: directories(),
        fit_mode: fit_mode(),
        unavailable_reason: (!available).then(|| "hyprctl is not installed".to_string()),
        available,
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
    if apply(&saved).is_ok() {
        *CURRENT.lock().unwrap() = Some(saved);
    }
}
