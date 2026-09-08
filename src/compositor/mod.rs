//! Normalized compositor state.
//!
//! Each supported compositor is an implementation of [`Compositor`] in its own module; this one
//! owns the shapes they all map onto, decides which implementation is answering, and exposes the
//! result. Nothing outside this module sees a `hyprctl` payload or a niri window id.
//!
//! Adding a compositor means adding a module with an `impl Compositor` and one line in
//! [`backends`] — no other file changes.

mod hyprland;
pub mod ipc;
mod niri;
mod sway;
mod wmctrl;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// A window, as every supported compositor is made to describe it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Window {
    /// Backend-qualified handle, e.g. `hypr:0x5a6557dd9b40`. Pass it back to [`focus_window`].
    pub id: String,
    pub app_id: String,
    pub title: String,
    /// Workspace name, empty when the compositor does not report one.
    pub workspace: String,
    /// Monitor name, empty when the compositor does not report one.
    pub monitor: String,
    pub focused: bool,
    pub floating: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Workspace {
    pub id: String,
    pub name: String,
    pub monitor: String,
    pub active: bool,
    pub urgent: bool,
    pub windows: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Monitor {
    pub id: String,
    pub name: String,
    pub width: i64,
    pub height: i64,
    pub scale: f64,
    pub focused: bool,
}

/// What one compositor can answer.
///
/// Every method returning `Option` uses `None` for "this compositor is not here or did not
/// answer", which is what backend selection keys on — not for "there are none", which is an empty
/// vector. Workspaces and monitors default to unsupported so a minimal backend (wmctrl) only has
/// to implement what it actually has.
pub trait Compositor: Send + Sync {
    /// Short name, and the prefix of every id this backend hands out.
    fn name(&self) -> &'static str;

    /// Cheap liveness probe, used to pick the backend that is actually running.
    fn responds(&self) -> bool;

    fn windows(&self) -> Option<Vec<Window>>;

    fn workspaces(&self) -> Option<Vec<Workspace>> {
        None
    }

    fn monitors(&self) -> Option<Vec<Monitor>> {
        None
    }

    /// `handle` is the part of [`Window::id`] after the backend prefix.
    fn focus_window(&self, handle: &str) -> Result<()>;

    fn close_window(&self, handle: &str) -> Result<()>;

    fn focus_workspace(&self, _handle: &str) -> Result<()> {
        anyhow::bail!("{} cannot switch workspaces", self.name())
    }
}

/// Every backend, in the order they should be tried.
///
/// Environment hints put the advertised compositor first; the rest still follow, because under a
/// systemd user service none of those variables are necessarily set and the only reliable test is
/// whether a backend answers.
fn backends() -> Vec<Box<dyn Compositor>> {
    let hypr = || Box::new(hyprland::Hyprland) as Box<dyn Compositor>;
    let niri = || Box::new(niri::Niri) as Box<dyn Compositor>;
    let sway = || Box::new(sway::Sway) as Box<dyn Compositor>;
    let wmctrl = || Box::new(wmctrl::Wmctrl) as Box<dyn Compositor>;
    match preferred_from_env() {
        Some("hypr") => vec![hypr(), niri(), sway(), wmctrl()],
        Some("sway") => vec![sway(), hypr(), niri(), wmctrl()],
        Some("niri") => vec![niri(), hypr(), sway(), wmctrl()],
        _ => vec![hypr(), niri(), sway(), wmctrl()],
    }
}

/// The compositor the environment advertises, if it advertises one.
pub fn preferred_from_env() -> Option<&'static str> {
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        return Some("hypr");
    }
    if std::env::var_os("SWAYSOCK").is_some() {
        return Some("sway");
    }
    if std::env::var_os("NIRI_SOCKET").is_some() {
        return Some("niri");
    }
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").ok()?.to_lowercase();
    for (needle, name) in [("hypr", "hypr"), ("sway", "sway"), ("niri", "niri")] {
        if desktop.contains(needle) {
            return Some(name);
        }
    }
    None
}

/// The backend that is actually answering, which is not always the one the environment
/// advertises.
pub fn active() -> Option<Box<dyn Compositor>> {
    backends().into_iter().find(|backend| backend.responds())
}

/// Ask each backend in turn and take the first that answers.
fn first<T>(query: impl Fn(&dyn Compositor) -> Option<T>) -> Option<T> {
    backends()
        .iter()
        .find_map(|backend| query(backend.as_ref()))
}

pub fn windows() -> Vec<Window> {
    first(|backend| backend.windows()).unwrap_or_default()
}

pub fn active_window() -> Option<Window> {
    windows().into_iter().find(|window| window.focused)
}

pub fn workspaces() -> Vec<Workspace> {
    first(|backend| backend.workspaces()).unwrap_or_default()
}

pub fn monitors() -> Vec<Monitor> {
    first(|backend| backend.monitors()).unwrap_or_default()
}

/// Look up the backend named by a `backend:handle` id.
fn resolve(id: &str) -> Result<(Box<dyn Compositor>, String)> {
    let (name, handle) = id
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("expected a backend-qualified id like \"hypr:0x1234\""))?;
    let backend = backends()
        .into_iter()
        .find(|backend| backend.name() == name)
        .ok_or_else(|| anyhow::anyhow!("unknown compositor backend \"{name}\""))?;
    Ok((backend, handle.to_string()))
}

pub fn focus_window(id: &str) -> Result<()> {
    let (backend, handle) = resolve(id)?;
    backend.focus_window(&handle)
}

pub fn close_window(id: &str) -> Result<()> {
    let (backend, handle) = resolve(id)?;
    backend.close_window(&handle)
}

/// Switch workspaces by qualified id, or by a bare name/number against the running compositor so
/// a keybinding does not have to know which backend it is on.
pub fn focus_workspace(id: &str) -> Result<()> {
    match resolve(id) {
        Ok((backend, handle)) => backend.focus_workspace(&handle),
        Err(_) => {
            let backend =
                active().ok_or_else(|| anyhow::anyhow!("no supported compositor is responding"))?;
            backend.focus_workspace(id)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// These tests mutate process env vars, which are global. Serialize them.
    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    fn lock_env() -> MutexGuard<'static, ()> {
        ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    fn set(vars: &[(&str, &str)]) {
        for (key, value) in vars {
            std::env::set_var(key, value);
        }
        for key in [
            "SWAYSOCK",
            "NIRI_SOCKET",
            "HYPRLAND_INSTANCE_SIGNATURE",
            "XDG_CURRENT_DESKTOP",
        ] {
            if !vars.iter().any(|(name, _)| *name == key) {
                std::env::remove_var(key);
            }
        }
    }

    #[test]
    fn detects_hyprland_from_instance_signature() {
        let _guard = lock_env();
        set(&[("HYPRLAND_INSTANCE_SIGNATURE", "s")]);
        assert_eq!(preferred_from_env(), Some("hypr"));
    }

    #[test]
    fn detects_niri_from_xdg_desktop() {
        let _guard = lock_env();
        set(&[("XDG_CURRENT_DESKTOP", "niri")]);
        assert_eq!(preferred_from_env(), Some("niri"));
    }

    #[test]
    fn without_any_signal_no_backend_is_preferred() {
        let _guard = lock_env();
        set(&[]);
        assert_eq!(preferred_from_env(), None);
    }

    #[test]
    fn the_preferred_backend_is_tried_first() {
        let _guard = lock_env();
        set(&[("SWAYSOCK", "/run/sway.sock")]);
        assert_eq!(
            backends().first().map(|backend| backend.name()),
            Some("sway")
        );
    }

    #[test]
    fn every_backend_is_tried_whatever_the_environment_says() {
        let names: Vec<&str> = backends().iter().map(|backend| backend.name()).collect();
        for expected in ["hypr", "niri", "sway", "wmctrl"] {
            assert!(
                names.contains(&expected),
                "{expected} missing from {names:?}"
            );
        }
        assert_eq!(names.len(), 4, "a backend is listed twice: {names:?}");
    }

    #[test]
    fn an_unqualified_id_is_refused() {
        assert!(resolve("nonsense").is_err());
    }

    #[test]
    fn an_unknown_backend_prefix_is_refused() {
        assert!(resolve("mystery:0x1").is_err());
    }
}
