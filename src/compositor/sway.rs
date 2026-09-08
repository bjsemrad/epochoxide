//! Sway (and i3-compatible trees), over `swaymsg`.

use super::ipc::{command_output, run};
use super::{Compositor, Monitor, Window, Workspace};
use anyhow::Result;
use serde_json::Value;

pub struct Sway;

impl Compositor for Sway {
    fn name(&self) -> &'static str {
        "sway"
    }

    fn responds(&self) -> bool {
        command_output("swaymsg", &["-t", "get_outputs"]).is_some()
    }

    fn windows(&self) -> Option<Vec<Window>> {
        let tree = json(&["-t", "get_tree"])?;
        let mut out = Vec::new();
        collect(&tree, "", &mut out);
        Some(out)
    }

    fn workspaces(&self) -> Option<Vec<Workspace>> {
        let raw = json(&["-t", "get_workspaces"])?;
        Some(
            raw.as_array()?
                .iter()
                .map(|workspace| Workspace {
                    id: format!(
                        "sway:{}",
                        workspace.get("num").and_then(Value::as_i64).unwrap_or(0)
                    ),
                    name: string(workspace, "name").unwrap_or_default(),
                    monitor: string(workspace, "output").unwrap_or_default(),
                    active: workspace
                        .get("focused")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    urgent: workspace
                        .get("urgent")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    windows: 0,
                })
                .collect(),
        )
    }

    fn monitors(&self) -> Option<Vec<Monitor>> {
        let raw = json(&["-t", "get_outputs"])?;
        Some(
            raw.as_array()?
                .iter()
                .map(|output| {
                    let mode = output.get("current_mode");
                    Monitor {
                        id: format!("sway:{}", string(output, "name").unwrap_or_default()),
                        name: string(output, "name").unwrap_or_default(),
                        width: mode
                            .and_then(|m| m.get("width"))
                            .and_then(Value::as_i64)
                            .unwrap_or(0),
                        height: mode
                            .and_then(|m| m.get("height"))
                            .and_then(Value::as_i64)
                            .unwrap_or(0),
                        scale: output.get("scale").and_then(Value::as_f64).unwrap_or(1.0),
                        focused: output
                            .get("focused")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    }
                })
                .collect(),
        )
    }

    fn focus_window(&self, handle: &str) -> Result<()> {
        run(&format!("swaymsg '[con_id={handle}] focus'"))
    }

    fn close_window(&self, handle: &str) -> Result<()> {
        run(&format!("swaymsg '[con_id={handle}] kill'"))
    }

    fn focus_workspace(&self, handle: &str) -> Result<()> {
        run(&format!("swaymsg workspace {handle}"))
    }
}

fn json(args: &[&str]) -> Option<Value> {
    serde_json::from_str(&command_output("swaymsg", args)?).ok()
}

fn string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Walk the tree, carrying the enclosing workspace name down to the windows inside it.
fn collect(node: &Value, workspace: &str, out: &mut Vec<Window>) {
    let workspace = if string(node, "type").as_deref() == Some("workspace") {
        string(node, "name").unwrap_or_default()
    } else {
        workspace.to_string()
    };
    let app = string(node, "app_id")
        .or_else(|| {
            node.get("window_properties")
                .and_then(|p| string(p, "class"))
        })
        .unwrap_or_default();
    if !app.is_empty() {
        if let Some(title) = string(node, "name") {
            out.push(Window {
                id: format!(
                    "sway:{}",
                    node.get("id").and_then(Value::as_i64).unwrap_or(0)
                ),
                app_id: app,
                title,
                workspace: workspace.clone(),
                monitor: String::new(),
                focused: node
                    .get("focused")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                floating: false,
            });
        }
    }
    for key in ["nodes", "floating_nodes"] {
        if let Some(children) = node.get(key).and_then(Value::as_array) {
            for child in children {
                collect(child, &workspace, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn windows_inherit_the_workspace_they_are_nested_under() {
        let tree = json!({
            "type": "root",
            "nodes": [{
                "type": "workspace",
                "name": "3",
                "nodes": [
                    { "id": 11, "name": "Firefox", "app_id": "firefox", "focused": true },
                    { "id": 12, "name": "Term", "app_id": "foot", "focused": false }
                ],
                "floating_nodes": [
                    { "id": 13, "name": "Calc", "app_id": "gnome-calculator" }
                ]
            }]
        });
        let mut out = Vec::new();
        collect(&tree, "", &mut out);
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|window| window.workspace == "3"));
        assert_eq!(out[0].id, "sway:11");
        assert!(out[0].focused);
        // A floating node is still a window on that workspace.
        assert_eq!(out[2].app_id, "gnome-calculator");
    }

    #[test]
    fn containers_without_an_app_are_not_windows() {
        let tree = json!({
            "type": "workspace",
            "name": "1",
            "nodes": [{ "id": 5, "name": "split", "nodes": [] }]
        });
        let mut out = Vec::new();
        collect(&tree, "", &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn an_xwayland_window_falls_back_to_its_class() {
        let tree = json!({
            "id": 9,
            "name": "Old App",
            "window_properties": { "class": "OldApp" }
        });
        let mut out = Vec::new();
        collect(&tree, "2", &mut out);
        assert_eq!(out[0].app_id, "OldApp");
    }
}
