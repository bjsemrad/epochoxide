use crate::config::Config;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

const EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp"];

const MAX_DEPTH: usize = 2;

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Wallpaper {
    pub wallpapers: Vec<String>,
    pub current: String,
    pub directories: Vec<String>,
    pub fit_mode: String,
    pub backend: String,
    pub available: bool,
    pub unavailable_reason: Option<String>,
}

static CURRENT: Mutex<Option<String>> = Mutex::new(None);

static DIRECTORIES: OnceLock<Vec<String>> = OnceLock::new();
static FIT_MODE: OnceLock<String> = OnceLock::new();
static BACKEND: OnceLock<String> = OnceLock::new();

const BACKENDS: &[&str] = &["auto", "shell", "hyprpaper"];

const SHELL_FIT_MODES: &[&str] = &["cover", "contain", "tile", "stretch"];

/// Everything else hyprpaper silently renders as cover.
const FIT_MODES: &[&str] = &["cover", "contain", "tile"];

/// Accepted by hyprctl but rendered as cover by hyprpaper 0.8.4, though `fit_mode = fill` in
/// hyprpaper.conf does stretch.
const FIT_MODES_LOST: &[&str] = &["stretch", "fit", "fill"];

pub fn configure(config: &Config) {
    let _ = DIRECTORIES.set(config.wallpaper_dirs.clone());

    let mut backend = config.wallpaper_backend.clone();
    if !BACKENDS.contains(&backend.as_str()) {
        eprintln!("wallpaper: backend {backend:?}: not one of {BACKENDS:?}, using \"auto\"");
        backend = "auto".into();
    }

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

fn current() -> String {
    remembered()
        .or_else(|| CURRENT.lock().unwrap().clone())
        .unwrap_or_default()
}

/// Atomic: `wallpaper.subscribe` watches this file, and a truncating write reads as the wallpaper
/// being unset.
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

/// Follows symlinks: a home-manager wallpaper directory is all links into the store.
fn scan_dir(dir: &Path, depth: usize, found: &mut Vec<String>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
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

const HYPRPAPER_UNIT: &str = "hyprpaper.service";

/// Restarts wait for clicking to settle: one per click flickers, and 5 in 10s trips systemd's start
/// limit, leaving no wallpaper.
const RESTART_SETTLE: Duration = Duration::from_millis(500);

/// Only the daemon lives long enough to restart later; a one-shot CLI call would exit first.
static DEFER_RESTARTS: AtomicBool = AtomicBool::new(false);

static RESTART_GENERATION: AtomicU64 = AtomicU64::new(0);

pub fn defer_restarts() {
    DEFER_RESTARTS.store(true, Ordering::SeqCst);
}

enum Backend {
    Shell,
    Ipc,
    Link(PathBuf),
}

fn hyprpaper_config() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("hypr/hyprpaper.conf"))
}

/// Reads both hyprpaper.conf dialects: 0.8's `wallpaper { path = ... }` and the older `wallpaper =
/// monitor,path`.
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
        let path = path.rsplit_once(':').map(|(_, p)| p).unwrap_or(path);
        if !path.is_empty() {
            return Some(shellexpand::tilde(path).into_owned());
        }
    }
    None
}

fn config_link() -> Result<PathBuf, String> {
    let conf = hyprpaper_config().ok_or("no config directory")?;
    let text = std::fs::read_to_string(&conf).map_err(|_| {
        format!(
            "hyprpaper's IPC is off outside Hyprland and {} cannot be read",
            conf.display()
        )
    })?;
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
        _ if hyprpaper_running() => hyprpaper_backend(),
        _ => Ok(Backend::Shell),
    }
}

/// Any hyprpaper process, not just the unit: Hyprland `exec-once` is as common.
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
        return Err(
            "hyprpaper's IPC is off outside Hyprland, and systemctl is not here to restart it"
                .into(),
        );
    }
    config_link()
        .map(Backend::Link)
        .map_err(|why| format!("{why} (or set wallpaper_backend = \"shell\" and stop hyprpaper)"))
}

pub fn available() -> Result<(), String> {
    backend().map(|_| ())
}

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
        .args(["hyprpaper", "wallpaper", &format!(",{path},{}", fit_mode())])
        .output()?;

    // hyprctl exits 0 and prints nothing on success; anything on stdout is the failure.
    let said = String::from_utf8_lossy(&output.stdout);
    let said = said.trim();
    if !said.is_empty() {
        bail!("hyprpaper refused: {said}");
    }
    Ok(())
}

/// Atomic: a new link renamed over the old, so hyprpaper never finds the path missing.
fn relink(link: &Path, target: &str) -> Result<()> {
    let name = link
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("wallpaper");
    let staging = link.with_file_name(format!(".{name}.epochoxide"));
    let _ = std::fs::remove_file(&staging);
    std::os::unix::fs::symlink(target, &staging)?;
    std::fs::rename(&staging, link)?;
    Ok(())
}

/// "failed" counts: it is what a run of restarts leaves behind. Only "inactive" is left alone.
fn unit_running() -> bool {
    let state = Command::new("systemctl")
        .args([
            "--user",
            "show",
            "-p",
            "ActiveState",
            "--value",
            HYPRPAPER_UNIT,
        ])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    matches!(
        state.as_str(),
        "active" | "activating" | "reloading" | "failed"
    )
}

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

/// The heartbeat is the only way a stream notices a client that has gone (the write fails), and
/// doubles as a rescan.
const WATCH_HEARTBEAT: Duration = Duration::from_secs(120);

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
    // The directory, not the file: `remember` renames over the file, which a file watch loses track
    // of.
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

pub fn step(step: i64) -> Result<Wallpaper> {
    let list = scan();
    if list.is_empty() {
        bail!("no images in {}", directories().join(", "));
    }

    let at = list.iter().position(|p| *p == current()).unwrap_or(0) as i64;
    let len = list.len() as i64;
    let to = (at + step).rem_euclid(len) as usize;

    set(&list[to])
}

pub fn restore() {
    let Some(saved) = remembered() else { return };
    if !Path::new(&saved).exists() {
        return;
    }
    let restored = match backend() {
        Ok(Backend::Shell) => true,
        Ok(Backend::Ipc) => apply_ipc(&saved).is_ok(),
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
