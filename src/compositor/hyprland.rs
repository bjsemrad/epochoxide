//! Hyprland, over its IPC socket with a `hyprctl` fallback.

use super::ipc::{hypr_ipc, hypr_output, hypr_window_dispatch, hypr_workspace_dispatch};
use super::{Compositor, Monitor, Window, Workspace};
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

    fn focus_window(&self, handle: &str) -> Result<()> {
        hypr_window_dispatch("focuswindow", "hl.dsp.focus({ window = w })", handle)
    }

    fn close_window(&self, handle: &str) -> Result<()> {
        hypr_window_dispatch("closewindow", "hl.dsp.window.close({ window = w })", handle)
    }

    fn focus_workspace(&self, handle: &str) -> Result<()> {
        hypr_workspace_dispatch(handle)
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
    }
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
    fn focus_is_decided_by_the_active_address_not_a_field() {
        assert!(!window(&client(), None, &[]).focused);
        assert!(window(&client(), Some("0xabc"), &[]).focused);
        assert!(!window(&client(), Some("0xdef"), &[]).focused);
    }
}
