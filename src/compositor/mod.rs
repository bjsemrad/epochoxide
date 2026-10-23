mod hyprland;
pub mod ipc;
mod mango;
mod niri;
mod sway;
mod wmctrl;

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Window {
    pub id: String,
    pub app_id: String,
    pub title: String,
    pub workspace: String,
    pub monitor: String,
    pub focused: bool,
    pub floating: bool,
    pub x: i64,
    pub y: i64,
    #[serde(default)]
    pub width: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WindowRegion {
    pub id: String,
    pub app_id: String,
    pub title: String,
    pub monitor: String,
    pub focused: bool,
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
}

impl WindowRegion {
    pub fn geometry(&self) -> String {
        format!("{},{} {}x{}", self.x, self.y, self.width, self.height)
    }
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

/// `None` means this compositor is absent or did not answer (backend selection keys on it), not
/// "there are none".
pub trait Compositor: Send + Sync {
    fn name(&self) -> &'static str;

    fn responds(&self) -> bool;

    fn windows(&self) -> Option<Vec<Window>>;

    fn workspaces(&self) -> Option<Vec<Workspace>> {
        None
    }

    fn monitors(&self) -> Option<Vec<Monitor>> {
        None
    }

    fn window_regions(&self) -> Option<Vec<WindowRegion>> {
        None
    }

    fn focus_window(&self, handle: &str) -> Result<()>;

    fn close_window(&self, handle: &str) -> Result<()>;

    fn focus_workspace(&self, _handle: &str) -> Result<()> {
        anyhow::bail!("{} cannot switch workspaces", self.name())
    }

    fn watch(&self, on_event: &mut dyn FnMut() -> Result<()>) -> Result<()> {
        loop {
            std::thread::sleep(POLL_INTERVAL);
            on_event()?;
        }
    }
}

pub const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct State {
    pub backend: String,
    pub windows: Vec<Window>,
    pub workspaces: Vec<Workspace>,
    pub monitors: Vec<Monitor>,
    pub active_window: Option<Window>,
}

pub fn state_of(backend: &dyn Compositor) -> State {
    let mut windows = backend.windows().unwrap_or_default();
    sort_windows(&mut windows);
    let mut workspaces = backend.workspaces().unwrap_or_default();
    workspaces.sort_by_key(|workspace| workspace_order(&workspace.name));
    State {
        backend: backend.name().to_string(),
        active_window: windows.iter().find(|window| window.focused).cloned(),
        windows,
        workspaces,
        monitors: backend.monitors().unwrap_or_default(),
    }
}

pub fn watch(mut emit: impl FnMut(&State) -> Result<()>) -> Result<()> {
    let backend =
        active().ok_or_else(|| anyhow::anyhow!("no supported compositor is responding"))?;
    let mut last = state_of(backend.as_ref());
    emit(&last)?;
    backend.watch(&mut || {
        let current = state_of(backend.as_ref());
        if current != last {
            last = current;
            emit(&last)?;
        }
        Ok(())
    })
}

/// Environment hints go first, but every backend is tried: under a systemd user service those
/// variables may be unset.
fn backends() -> Vec<Box<dyn Compositor>> {
    let hypr = || Box::new(hyprland::Hyprland) as Box<dyn Compositor>;
    let niri = || Box::new(niri::Niri) as Box<dyn Compositor>;
    let mango = || Box::new(mango::Mango) as Box<dyn Compositor>;
    let sway = || Box::new(sway::Sway) as Box<dyn Compositor>;
    let wmctrl = || Box::new(wmctrl::Wmctrl) as Box<dyn Compositor>;
    match preferred_from_env() {
        Some("hypr") => vec![hypr(), niri(), mango(), sway(), wmctrl()],
        Some("sway") => vec![sway(), hypr(), niri(), mango(), wmctrl()],
        Some("niri") => vec![niri(), hypr(), mango(), sway(), wmctrl()],
        Some("mango") => vec![mango(), hypr(), niri(), sway(), wmctrl()],
        _ => vec![hypr(), niri(), mango(), sway(), wmctrl()],
    }
}

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
    if std::env::var_os("MANGO_INSTANCE_SIGNATURE").is_some() {
        return Some("mango");
    }
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").ok()?.to_lowercase();
    for (needle, name) in [
        ("hypr", "hypr"),
        ("sway", "sway"),
        ("niri", "niri"),
        ("mango", "mango"),
    ] {
        if desktop.contains(needle) {
            return Some(name);
        }
    }
    None
}

pub fn active() -> Option<Box<dyn Compositor>> {
    backends().into_iter().find(|backend| backend.responds())
}

fn first<T>(query: impl Fn(&dyn Compositor) -> Option<T>) -> Option<T> {
    backends()
        .iter()
        .find_map(|backend| query(backend.as_ref()))
}

pub fn windows() -> Vec<Window> {
    let mut windows = first(|backend| backend.windows()).unwrap_or_default();
    sort_windows(&mut windows);
    windows
}

pub fn sort_windows(windows: &mut [Window]) {
    windows.sort_by(|a, b| {
        workspace_order(&a.workspace)
            .cmp(&workspace_order(&b.workspace))
            .then(a.x.cmp(&b.x))
            .then(a.y.cmp(&b.y))
            .then(a.id.cmp(&b.id))
    });
}

fn workspace_order(name: &str) -> (u8, i64, String) {
    match name.parse::<i64>() {
        Ok(number) => (0, number, String::new()),
        Err(_) => (1, 0, name.to_lowercase()),
    }
}

pub fn active_window() -> Option<Window> {
    windows().into_iter().find(|window| window.focused)
}

pub fn workspaces() -> Vec<Workspace> {
    let mut workspaces = first(|backend| backend.workspaces()).unwrap_or_default();
    workspaces.sort_by_key(|workspace| workspace_order(&workspace.name));
    workspaces
}

pub fn monitors() -> Vec<Monitor> {
    first(|backend| backend.monitors()).unwrap_or_default()
}

pub fn window_regions() -> Option<Vec<WindowRegion>> {
    first(|backend| backend.window_regions())
}

pub fn focused_monitor() -> Option<String> {
    monitors()
        .into_iter()
        .find(|monitor| monitor.focused)
        .map(|monitor| monitor.name)
        .filter(|name| !name.is_empty())
}

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
            "MANGO_INSTANCE_SIGNATURE",
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
    fn detects_mango_from_instance_signature() {
        let _guard = lock_env();
        set(&[("MANGO_INSTANCE_SIGNATURE", "/run/user/1000/mango-1.sock")]);
        assert_eq!(preferred_from_env(), Some("mango"));
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
        for expected in ["hypr", "niri", "mango", "sway", "wmctrl"] {
            assert!(
                names.contains(&expected),
                "{expected} missing from {names:?}"
            );
        }
        assert_eq!(names.len(), 5, "a backend is listed twice: {names:?}");
    }

    fn window(workspace: &str, x: i64, y: i64, id: &str) -> Window {
        Window {
            id: id.to_string(),
            app_id: String::new(),
            title: String::new(),
            workspace: workspace.to_string(),
            monitor: String::new(),
            focused: false,
            floating: false,
            x,
            y,
            width: 0,
        }
    }

    #[test]
    fn windows_are_ordered_left_to_right_then_top_to_bottom() {
        let mut windows = vec![
            window("1", 900, 0, "c"),
            window("1", 100, 500, "b"),
            window("1", 100, 0, "a"),
        ];
        sort_windows(&mut windows);
        let ids: Vec<&str> = windows.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
    }

    #[test]
    fn windows_group_by_workspace_in_numeric_order() {
        let mut windows = vec![
            window("10", 0, 0, "ten"),
            window("2", 0, 0, "two"),
            window("1", 0, 0, "one"),
        ];
        sort_windows(&mut windows);
        let ids: Vec<&str> = windows.iter().map(|w| w.id.as_str()).collect();
        // "10" must not sort between "1" and "2".
        assert_eq!(ids, ["one", "two", "ten"]);
    }

    #[test]
    fn named_workspaces_sort_after_numbered_ones() {
        assert!(workspace_order("2") < workspace_order("10"));
        assert!(workspace_order("10") < workspace_order("web"));
        assert!(workspace_order("web") < workspace_order("zed"));
    }

    #[test]
    fn ordering_is_stable_for_windows_in_the_same_place() {
        let mut windows = vec![window("1", 0, 0, "b"), window("1", 0, 0, "a")];
        sort_windows(&mut windows);
        let ids: Vec<&str> = windows.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
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
