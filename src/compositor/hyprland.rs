//! Hyprland, over its IPC socket with a `hyprctl` fallback.

use super::ipc::{
    hypr_ipc, hypr_output, hypr_watch, hypr_window_dispatch, hypr_workspace_dispatch,
};
use super::{Compositor, Monitor, Window, WindowRegion, Workspace};
use anyhow::Result;
use serde_json::Value;

pub struct Hyprland;

impl Compositor for Hyprland {
    fn name(&self) -> &'static str {
        "hypr"
    }

    fn responds(&self) -> bool {
        json("j/monitors", &["monitors", "-j"]).is_some()
    }

    fn windows(&self) -> Option<Vec<Window>> {
        let clients = json("j/clients", &["clients", "-j"])?;
        let active = json("j/activewindow", &["activewindow", "-j"])
            .and_then(|value| string(&value, "address"));
        // A window's monitor comes back as an index into the monitor list, so the names have to be
        // fetched to keep the normalized shape backend-neutral.
        let monitor_names: Vec<String> = self
            .monitors()
            .unwrap_or_default()
            .into_iter()
            .map(|monitor| monitor.name)
            .collect();
        Some(
            clients
                .as_array()?
                .iter()
                .map(|client| window(client, active.as_deref(), &monitor_names))
                .collect(),
        )
    }

    fn workspaces(&self) -> Option<Vec<Workspace>> {
        let workspaces = json("j/workspaces", &["workspaces", "-j"])?;
        // "Active" is per monitor: each monitor names the workspace currently shown on it.
        let active: Vec<i64> = json("j/monitors", &["monitors", "-j"])
            .and_then(|value| {
                Some(
                    value
                        .as_array()?
                        .iter()
                        .filter_map(|monitor| {
                            monitor
                                .get("activeWorkspace")
                                .and_then(|ws| ws.get("id"))
                                .and_then(Value::as_i64)
                        })
                        .collect(),
                )
            })
            .unwrap_or_default();
        Some(
            workspaces
                .as_array()?
                .iter()
                .map(|workspace| {
                    let id = workspace.get("id").and_then(Value::as_i64).unwrap_or(0);
                    Workspace {
                        id: format!("hypr:{id}"),
                        name: string(workspace, "name").unwrap_or_default(),
                        monitor: string(workspace, "monitor").unwrap_or_default(),
                        active: active.contains(&id),
                        // Hyprland does not report per-workspace urgency in this payload.
                        urgent: false,
                        windows: workspace
                            .get("windows")
                            .and_then(Value::as_u64)
                            .unwrap_or(0) as u32,
                    }
                })
                .collect(),
        )
    }

    fn monitors(&self) -> Option<Vec<Monitor>> {
        let monitors = json("j/monitors", &["monitors", "-j"])?;
        Some(
            monitors
                .as_array()?
                .iter()
                .map(|monitor| Monitor {
                    id: format!(
                        "hypr:{}",
                        monitor.get("id").and_then(Value::as_i64).unwrap_or(0)
                    ),
                    name: string(monitor, "name").unwrap_or_default(),
                    width: monitor.get("width").and_then(Value::as_i64).unwrap_or(0),
                    height: monitor.get("height").and_then(Value::as_i64).unwrap_or(0),
                    scale: monitor.get("scale").and_then(Value::as_f64).unwrap_or(1.0),
                    focused: monitor
                        .get("focused")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                })
                .collect(),
        )
    }

    fn window_regions(&self) -> Option<Vec<WindowRegion>> {
        let clients = json("j/clients", &["clients", "-j"])?;
        let monitors = json("j/monitors", &["monitors", "-j"])?;
        let active = json("j/activewindow", &["activewindow", "-j"])
            .and_then(|value| string(&value, "address"));
        let visible = visible_workspaces(&monitors);
        let monitor_names: Vec<String> = self
            .monitors()
            .unwrap_or_default()
            .into_iter()
            .map(|monitor| monitor.name)
            .collect();
        Some(
            clients
                .as_array()?
                .iter()
                .filter(|client| on_screen(client, &visible))
                .map(|client| region(client, active.as_deref(), &monitor_names))
                .filter(|region| region.width > 0 && region.height > 0)
                .collect(),
        )
    }

    fn focus_window(&self, handle: &str) -> Result<()> {
        hypr_window_dispatch("focuswindow", "hl.dsp.focus({ window = w })", handle)
    }

    fn close_window(&self, handle: &str) -> Result<()> {
        hypr_window_dispatch("closewindow", "hl.dsp.window.close({ window = w })", handle)
    }

    fn focus_workspace(&self, handle: &str) -> Result<()> {
        hypr_workspace_dispatch(handle)
    }

    fn watch(&self, on_event: &mut dyn FnMut() -> Result<()>) -> Result<()> {
        hypr_watch(on_event)
    }
}

/// Prefer the IPC socket, fall back to the CLI.
fn json(endpoint: &str, args: &[&str]) -> Option<Value> {
    let raw = hypr_ipc(endpoint).or_else(|| hypr_output(args))?;
    serde_json::from_str(&raw).ok()
}

fn string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Map one `hyprctl clients` entry onto the normalized shape.
fn window(client: &Value, active: Option<&str>, monitor_names: &[String]) -> Window {
    let address = string(client, "address").unwrap_or_default();
    let monitor_index = client.get("monitor").and_then(Value::as_i64).unwrap_or(-1);
    Window {
        focused: active == Some(address.as_str()),
        id: format!("hypr:{address}"),
        app_id: string(client, "class").unwrap_or_default(),
        title: string(client, "title").unwrap_or_default(),
        workspace: client
            .get("workspace")
            .and_then(|ws| string(ws, "name"))
            .unwrap_or_default(),
        monitor: usize::try_from(monitor_index)
            .ok()
            .and_then(|index| monitor_names.get(index).cloned())
            .unwrap_or_default(),
        floating: client
            .get("floating")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        x: at(client, 0),
        y: at(client, 1),
    }
}

/// Map one `hyprctl clients` entry onto its on-screen rectangle.
fn region(client: &Value, active: Option<&str>, monitor_names: &[String]) -> WindowRegion {
    let address = string(client, "address").unwrap_or_default();
    let monitor_index = client.get("monitor").and_then(Value::as_i64).unwrap_or(-1);
    WindowRegion {
        focused: active == Some(address.as_str()),
        id: format!("hypr:{address}"),
        app_id: string(client, "class").unwrap_or_default(),
        title: string(client, "title").unwrap_or_default(),
        monitor: usize::try_from(monitor_index)
            .ok()
            .and_then(|index| monitor_names.get(index).cloned())
            .unwrap_or_default(),
        x: at(client, 0),
        y: at(client, 1),
        width: size(client, 0),
        height: size(client, 1),
    }
}

/// The workspace ids a viewer can actually see: the one each monitor is showing, plus any special
/// workspace pulled over it.
fn visible_workspaces(monitors: &Value) -> Vec<i64> {
    let Some(monitors) = monitors.as_array() else {
        return Vec::new();
    };
    let mut ids = Vec::new();
    for monitor in monitors {
        for key in ["activeWorkspace", "specialWorkspace"] {
            let id = monitor
                .get(key)
                .and_then(|workspace| workspace.get("id"))
                .and_then(Value::as_i64);
            // A monitor with no special workspace open reports id 0, which is not a workspace.
            if let Some(id) = id.filter(|id| *id != 0) {
                ids.push(id);
            }
        }
    }
    ids
}

/// Whether a window is on screen right now. `mapped`/`hidden` cover minimized and grouped
/// windows; the workspace check covers the far more common case of a window sitting at real
/// coordinates on a workspace nobody is currently looking at.
fn on_screen(client: &Value, visible: &[i64]) -> bool {
    let mapped = client
        .get("mapped")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let hidden = client
        .get("hidden")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let workspace = client
        .get("workspace")
        .and_then(|workspace| workspace.get("id"))
        .and_then(Value::as_i64);
    mapped && !hidden && workspace.is_some_and(|id| visible.contains(&id))
}

/// Hyprland reports a window's size as `size: [width, height]`.
fn size(client: &Value, index: usize) -> i64 {
    client
        .get("size")
        .and_then(Value::as_array)
        .and_then(|pair| pair.get(index))
        .and_then(Value::as_i64)
        .unwrap_or(0)
}

/// Hyprland reports a window's position as `at: [x, y]`.
fn at(client: &Value, index: usize) -> i64 {
    client
        .get("at")
        .and_then(Value::as_array)
        .and_then(|pair| pair.get(index))
        .and_then(Value::as_i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn client() -> Value {
        json!({
            "address": "0xabc",
            "class": "com.mitchellh.ghostty",
            "title": "thor: omarchy",
            "workspace": { "id": 4, "name": "4" },
            "monitor": 1,
            "floating": false
        })
    }

    #[test]
    fn a_monitor_index_becomes_a_monitor_name() {
        let names = vec!["eDP-1".to_string(), "DP-3".to_string()];
        let mapped = window(&client(), None, &names);
        assert_eq!(mapped.monitor, "DP-3");
        assert_eq!(mapped.workspace, "4");
        assert_eq!(mapped.id, "hypr:0xabc");
    }

    #[test]
    fn a_monitor_index_with_no_matching_name_is_left_empty() {
        // A monitor unplugged between the two queries would otherwise panic or mislabel.
        let mapped = window(&client(), None, &["eDP-1".to_string()]);
        assert_eq!(mapped.monitor, "");
    }

    #[test]
    fn a_region_carries_the_on_screen_rectangle() {
        let mut raw = client();
        raw["at"] = json!([1920, 40]);
        raw["size"] = json!([1280, 800]);
        let region = region(&raw, Some("0xabc"), &["eDP-1".into(), "DP-3".into()]);
        assert_eq!(region.geometry(), "1920,40 1280x800");
        assert_eq!(region.monitor, "DP-3");
        assert!(region.focused);
    }

    #[test]
    fn only_windows_on_a_visible_workspace_count_as_on_screen() {
        // Workspace 4 holds the window; the monitors are showing 1 and 7.
        assert!(!on_screen(&client(), &[1, 7]));
        assert!(on_screen(&client(), &[1, 4]));
    }

    #[test]
    fn an_unmapped_or_hidden_window_is_not_on_screen() {
        let mut hidden = client();
        hidden["hidden"] = json!(true);
        assert!(!on_screen(&hidden, &[4]));
        let mut unmapped = client();
        unmapped["mapped"] = json!(false);
        assert!(!on_screen(&unmapped, &[4]));
    }

    #[test]
    fn a_monitor_with_no_special_workspace_contributes_no_id() {
        let monitors = json!([
            { "activeWorkspace": { "id": 4 }, "specialWorkspace": { "id": 0 } },
            { "activeWorkspace": { "id": 7 }, "specialWorkspace": { "id": -98 } },
        ]);
        assert_eq!(visible_workspaces(&monitors), vec![4, 7, -98]);
    }

    #[test]
    fn focus_is_decided_by_the_active_address_not_a_field() {
        assert!(!window(&client(), None, &[]).focused);
        assert!(window(&client(), Some("0xabc"), &[]).focused);
        assert!(!window(&client(), Some("0xdef"), &[]).focused);
    }
}
