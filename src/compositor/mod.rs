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
    /// Position on its workspace. Used to order windows the way they are laid out on screen; a
    /// backend that does not report geometry leaves both at zero.
    pub x: i64,
    pub y: i64,
}

/// Where a window sits on screen, in output-layout pixels.
///
/// Kept apart from [`Window`] because not every compositor reports it, and because the two mean
/// different things: `Window::x` orders windows for the bar, while this names a rectangle a
/// capture tool can grab. niri lays windows out in scrolling columns and reports a position in
/// that layout rather than on screen, so it answers `None` here instead of handing a caller
/// coordinates that do not describe a rectangle on a display.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WindowRegion {
    /// The same backend-qualified handle [`Window::id`] carries.
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
    /// The geometry as slurp and grim spell it: `x,y WxH`.
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

    /// Where each window a viewer can currently see sits on screen.
    ///
    /// `None` means this backend cannot say -- either it reports no screen-space geometry, or it
    /// is not running. Callers fall back to asking the user to draw a region rather than guessing.
    /// Windows on a workspace nobody is looking at are left out: their coordinates name a
    /// rectangle that is on screen, but showing something else entirely.
    fn window_regions(&self) -> Option<Vec<WindowRegion>> {
        None
    }

    /// `handle` is the part of [`Window::id`] after the backend prefix.
    fn focus_window(&self, handle: &str) -> Result<()>;

    fn close_window(&self, handle: &str) -> Result<()>;

    fn focus_workspace(&self, _handle: &str) -> Result<()> {
        anyhow::bail!("{} cannot switch workspaces", self.name())
    }

    /// Block, calling `on_event` whenever compositor state may have changed.
    ///
    /// Backends with an event socket implement this so a change reaches the shell immediately.
    /// The default polls, which is what a backend with no event source (wmctrl) is left with.
    /// Implementations pass on whatever `on_event` returns: an error means the consumer has gone
    /// away, and the watch should stop rather than spin.
    fn watch(&self, on_event: &mut dyn FnMut() -> Result<()>) -> Result<()> {
        loop {
            std::thread::sleep(POLL_INTERVAL);
            on_event()?;
        }
    }
}

/// How often a backend with no event source re-checks. Only wmctrl and sway use this.
pub const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// Everything the shell needs to render compositor state, in one payload.
///
/// State is pushed as a whole snapshot rather than as deltas: a compositor event says something
/// changed but not reliably what, and the whole payload is a few hundred bytes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct State {
    pub backend: String,
    pub windows: Vec<Window>,
    pub workspaces: Vec<Workspace>,
    pub monitors: Vec<Monitor>,
    pub active_window: Option<Window>,
}

/// Read the whole of the current state from one backend.
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

/// Watch the running compositor, calling `emit` with a fresh snapshot whenever one differs from
/// the last. Blocks until `emit` fails, which is how a disconnected client stops the watch.
///
/// Events are deliberately not forwarded: a raw `workspace>>3` line is exactly the sort of
/// backend detail this module exists to keep out of the shell. An event only triggers a re-read.
pub fn watch(mut emit: impl FnMut(&State) -> Result<()>) -> Result<()> {
    let backend =
        active().ok_or_else(|| anyhow::anyhow!("no supported compositor is responding"))?;
    let mut last = state_of(backend.as_ref());
    emit(&last)?;
    backend.watch(&mut || {
        let current = state_of(backend.as_ref());
        // Compositors emit several events for one user action, and some (cursor moves, focus
        // churn) change nothing the shell renders. Only differences are worth a wake-up.
        if current != last {
            last = current;
            emit(&last)?;
        }
        Ok(())
    })
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
    let mut windows = first(|backend| backend.windows()).unwrap_or_default();
    sort_windows(&mut windows);
    windows
}

/// Left to right, then top to bottom, which is the order they appear on screen and so the order
/// the bar should draw them in. Compositors return windows in their own internal order -- most
/// recently focused, or creation order -- which is not what a viewer sees.
pub fn sort_windows(windows: &mut [Window]) {
    windows.sort_by(|a, b| {
        workspace_order(&a.workspace)
            .cmp(&workspace_order(&b.workspace))
            .then(a.x.cmp(&b.x))
            .then(a.y.cmp(&b.y))
            .then(a.id.cmp(&b.id))
    });
}

/// Order a workspace by its displayed name: numerically when it is a number, which is the usual
/// case on both Hyprland and niri, and alphabetically after those when it is not.
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

/// Visible windows and their on-screen rectangles, or `None` when the running compositor does not
/// report them. Used by capture to grab a window without the user drawing a box around it.
pub fn window_regions() -> Option<Vec<WindowRegion>> {
    first(|backend| backend.window_regions())
}

/// The monitor the compositor says is focused, by name.
pub fn focused_monitor() -> Option<String> {
    monitors()
        .into_iter()
        .find(|monitor| monitor.focused)
        .map(|monitor| monitor.name)
        .filter(|name| !name.is_empty())
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
        }
    }

    #[test]
    fn windows_are_ordered_left_to_right_then_top_to_bottom() {
        // Compositors hand back windows in focus or creation order; the bar wants the order they
        // sit on screen.
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
        // "10" must not sort between "1" and "2", which a plain string sort would do.
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
        // Two windows can report the same position (a stack, or a backend with no geometry at
        // all), and the strip should not reshuffle between snapshots.
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
