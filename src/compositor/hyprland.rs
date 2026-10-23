use super::ipc::{
    hypr_ipc, hypr_output, hypr_watch, hypr_window_dispatch, hypr_workspace_dispatch,
};
use super::{Compositor, Monitor, Window, WindowRegion, Workspace};
use anyhow::Result;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Mutex;

/// Hyprland only reports urgency as it happens (`urgent>>ADDRESS`), so it is tracked here until the
/// window is focused, closed, or its workspace shown.
static URGENT: Mutex<Option<HashSet<String>>> = Mutex::new(None);

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
        let urgent = urgent_workspaces(&active);
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
                        urgent: urgent.contains(&id),
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
        hypr_watch(&mut |line| {
            note_event(line);
            on_event()
        })
    }
}

fn note_event(line: &str) {
    let Some((event, payload)) = line.split_once(">>") else {
        return;
    };
    let Ok(mut urgent) = URGENT.lock() else {
        return;
    };
    apply_event(urgent.get_or_insert_with(HashSet::new), event, payload);
}

fn apply_event(urgent: &mut HashSet<String>, event: &str, payload: &str) {
    match event {
        "urgent" => {
            urgent.insert(bare_address(payload));
        }
        "activewindowv2" | "closewindow" => {
            urgent.remove(&bare_address(payload));
        }
        _ => {}
    }
}

fn urgent_workspaces(active: &[i64]) -> HashSet<i64> {
    let Ok(mut guard) = URGENT.lock() else {
        return HashSet::new();
    };
    let Some(urgent) = guard.as_mut().filter(|set| !set.is_empty()) else {
        return HashSet::new();
    };
    let Some(clients) = json("j/clients", &["clients", "-j"]) else {
        return HashSet::new();
    };
    let placed: Vec<(String, i64)> = clients
        .as_array()
        .map(|clients| {
            clients
                .iter()
                .map(|client| {
                    let address = bare_address(&string(client, "address").unwrap_or_default());
                    let workspace = client
                        .get("workspace")
                        .and_then(|ws| ws.get("id"))
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    (address, workspace)
                })
                .collect()
        })
        .unwrap_or_default();
    settle_urgency(urgent, &placed, active)
}

fn settle_urgency(
    urgent: &mut HashSet<String>,
    placed: &[(String, i64)],
    active: &[i64],
) -> HashSet<i64> {
    urgent.retain(|address| {
        placed
            .iter()
            .any(|(other, workspace)| other == address && !active.contains(workspace))
    });
    placed
        .iter()
        .filter(|(address, _)| urgent.contains(address))
        .map(|(_, workspace)| *workspace)
        .collect()
}

/// `clients` says `0x55d4...`, events say `55d4...`.
fn bare_address(address: &str) -> String {
    address.trim().trim_start_matches("0x").to_string()
}

fn json(endpoint: &str, args: &[&str]) -> Option<Value> {
    let raw = hypr_ipc(endpoint).or_else(|| hypr_output(args))?;
    serde_json::from_str(&raw).ok()
}

fn string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

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
        width: 0,
    }
}

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
            // A monitor with no special workspace open reports id 0.
            if let Some(id) = id.filter(|id| *id != 0) {
                ids.push(id);
            }
        }
    }
    ids
}

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

fn size(client: &Value, index: usize) -> i64 {
    client
        .get("size")
        .and_then(Value::as_array)
        .and_then(|pair| pair.get(index))
        .and_then(Value::as_i64)
        .unwrap_or(0)
}

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
    fn urgency_is_set_by_an_urgent_event_and_cleared_by_focus_or_closing() {
        let mut urgent = HashSet::new();
        apply_event(&mut urgent, "urgent", "55d4a0");
        apply_event(&mut urgent, "urgent", "0x77aa10");
        assert!(urgent.contains("55d4a0") && urgent.contains("77aa10"));
        apply_event(&mut urgent, "activewindowv2", "55d4a0");
        apply_event(&mut urgent, "closewindow", "77aa10");
        apply_event(&mut urgent, "workspace", "3");
        assert!(urgent.is_empty());
    }

    #[test]
    fn a_workspace_is_urgent_until_it_has_been_on_screen() {
        let mut urgent: HashSet<String> = ["55d4a0".to_string()].into();
        let placed = vec![("55d4a0".to_string(), 3), ("77aa10".to_string(), 1)];
        assert_eq!(settle_urgency(&mut urgent, &placed, &[1]), [3].into());
        assert!(settle_urgency(&mut urgent, &placed, &[3]).is_empty());
        assert!(settle_urgency(&mut urgent, &placed, &[1]).is_empty());
    }

    #[test]
    fn an_urgent_window_that_has_gone_is_forgotten() {
        let mut urgent: HashSet<String> = ["deadbeef".to_string()].into();
        assert!(settle_urgency(&mut urgent, &[], &[1]).is_empty());
        assert!(urgent.is_empty());
    }

    #[test]
    fn focus_is_decided_by_the_active_address_not_a_field() {
        assert!(!window(&client(), None, &[]).focused);
        assert!(window(&client(), Some("0xabc"), &[]).focused);
        assert!(!window(&client(), Some("0xdef"), &[]).focused);
    }
}
