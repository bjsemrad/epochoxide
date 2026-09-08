//! niri, over `niri msg --json`.

use super::ipc::{niri_output, niri_shell, run};
use super::{Compositor, Monitor, Window, Workspace};
use anyhow::Result;
use serde_json::Value;
use std::collections::HashMap;

pub struct Niri;

impl Compositor for Niri {
    fn name(&self) -> &'static str {
        "niri"
    }

    fn responds(&self) -> bool {
        json(&["msg", "--json", "outputs"]).is_some()
    }

    fn windows(&self) -> Option<Vec<Window>> {
        let windows = json(&["msg", "--json", "windows"])?;
        // niri identifies a window's workspace by id; the shell wants its name.
        let names = workspace_names();
        Some(
            windows
                .as_array()?
                .iter()
                .map(|window| {
                    let id = window.get("id").and_then(Value::as_i64).unwrap_or(0);
                    Window {
                        id: format!("niri:{id}"),
                        app_id: string(window, "app_id").unwrap_or_default(),
                        title: string(window, "title").unwrap_or_default(),
                        workspace: window
                            .get("workspace_id")
                            .and_then(Value::as_i64)
                            .and_then(|id| names.get(&id).cloned())
                            .unwrap_or_default(),
                        monitor: string(window, "output").unwrap_or_default(),
                        focused: window
                            .get("is_focused")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        floating: window
                            .get("is_floating")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    }
                })
                .collect(),
        )
    }

    fn workspaces(&self) -> Option<Vec<Workspace>> {
        let workspaces = json(&["msg", "--json", "workspaces"])?;
        Some(
            workspaces
                .as_array()?
                .iter()
                .map(|workspace| {
                    let id = workspace.get("id").and_then(Value::as_i64).unwrap_or(0);
                    Workspace {
                        id: format!("niri:{id}"),
                        name: workspace_name(workspace, id),
                        monitor: string(workspace, "output").unwrap_or_default(),
                        active: workspace
                            .get("is_active")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        urgent: workspace
                            .get("is_urgent")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        windows: 0,
                    }
                })
                .collect(),
        )
    }

    fn monitors(&self) -> Option<Vec<Monitor>> {
        let outputs = json(&["msg", "--json", "outputs"])?;
        // niri keys outputs by connector name rather than returning a list.
        let entries: Vec<(String, &Value)> = match &outputs {
            Value::Object(map) => map
                .iter()
                .map(|(key, value)| (key.clone(), value))
                .collect(),
            Value::Array(items) => items
                .iter()
                .map(|item| (string(item, "name").unwrap_or_default(), item))
                .collect(),
            _ => return None,
        };
        Some(
            entries
                .into_iter()
                .map(|(key, output)| {
                    let logical = output.get("logical");
                    let field = |name: &str| logical.and_then(|l| l.get(name));
                    Monitor {
                        id: format!("niri:{key}"),
                        name: string(output, "name").unwrap_or(key),
                        width: field("width").and_then(Value::as_i64).unwrap_or(0),
                        height: field("height").and_then(Value::as_i64).unwrap_or(0),
                        scale: field("scale").and_then(Value::as_f64).unwrap_or(1.0),
                        focused: output
                            .get("is_focused")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    }
                })
                .collect(),
        )
    }

    fn focus_window(&self, handle: &str) -> Result<()> {
        run(&format!(
            "{} msg action focus-window --id {handle}",
            niri_shell()
        ))
    }

    fn close_window(&self, handle: &str) -> Result<()> {
        run(&format!(
            "{} msg action close-window --id {handle}",
            niri_shell()
        ))
    }

    fn focus_workspace(&self, handle: &str) -> Result<()> {
        run(&format!(
            "{} msg action focus-workspace {handle}",
            niri_shell()
        ))
    }
}

fn json(args: &[&str]) -> Option<Value> {
    serde_json::from_str(&niri_output(args)?).ok()
}

fn string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

/// niri workspaces are often unnamed; fall back to the display index, then the raw id, so a
/// workspace always has something a person can read.
fn workspace_name(workspace: &Value, id: i64) -> String {
    string(workspace, "name")
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| {
            workspace
                .get("idx")
                .and_then(Value::as_i64)
                .map(|idx| idx.to_string())
                .unwrap_or_else(|| id.to_string())
        })
}

fn workspace_names() -> HashMap<i64, String> {
    let Some(workspaces) = json(&["msg", "--json", "workspaces"]) else {
        return HashMap::new();
    };
    workspaces
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|workspace| {
                    let id = workspace.get("id").and_then(Value::as_i64)?;
                    Some((id, workspace_name(workspace, id)))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_named_workspace_keeps_its_name() {
        let workspace = json!({ "id": 7, "idx": 2, "name": "web" });
        assert_eq!(workspace_name(&workspace, 7), "web");
    }

    #[test]
    fn an_unnamed_workspace_falls_back_to_its_index() {
        // niri reports name: null for the workspaces most people actually use.
        let workspace = json!({ "id": 7, "idx": 2, "name": Value::Null });
        assert_eq!(workspace_name(&workspace, 7), "2");
    }

    #[test]
    fn an_empty_name_is_treated_as_no_name() {
        let workspace = json!({ "id": 7, "idx": 2, "name": "" });
        assert_eq!(workspace_name(&workspace, 7), "2");
    }

    #[test]
    fn with_neither_name_nor_index_the_id_is_used() {
        assert_eq!(workspace_name(&json!({ "id": 7 }), 7), "7");
    }
}
