use super::{command_output, run_shell, Provider};
use crate::{fuzzy, types::{action_map, ActionCapability, Item, ProviderCapability}};
use anyhow::Result;
use serde::Deserialize;

pub struct WindowsProvider;

impl WindowsProvider { pub fn new() -> Self { Self } }

#[derive(Debug, Deserialize)]
struct HyprClient { address: String, title: String, class: String, workspace: HyprWorkspace }
#[derive(Debug, Deserialize)]
struct HyprWorkspace { name: String }

#[derive(Debug, Deserialize)]
struct SwayNode { id: i64, name: Option<String>, app_id: Option<String>, window_properties: Option<SwayProps>, nodes: Option<Vec<SwayNode>>, floating_nodes: Option<Vec<SwayNode>> }
#[derive(Debug, Deserialize)]
struct SwayProps { class: Option<String> }

#[derive(Debug, Clone)]
struct Window { id: String, title: String, app: String, workspace: String, backend: &'static str }

impl Provider for WindowsProvider {
    fn name(&self) -> &'static str { "windows" }
    fn pretty_name(&self) -> &'static str { "Windows" }

    fn query(&mut self, query: &str, limit: usize, exact: bool) -> Vec<Item> {
        let mut out = Vec::new();
        for window in discover_windows() {
            let haystack = format!("{} {} {}", window.title, window.app, window.workspace);
            if let Some((score, info)) = fuzzy::score(query, &haystack, exact, "text") {
                let mut item = Item::new(self.name(), format!("{}:{}", window.backend, window.id), window.title);
                item.subtext = format!("{} {}", window.app, window.workspace).trim().to_string();
                item.icon = "preferences-system-windows".into();
                item.actions = vec!["focus".into()];
                item.score = score + 5_000;
                item.fuzzyinfo = Some(info);
                out.push(item);
            }
        }
        out.truncate(limit);
        out
    }

    fn activate(&mut self, identifier: &str, action: &str, _query: &str, _arguments: &str) -> Result<()> {
        if action != "focus" { anyhow::bail!("unsupported windows action: {action}"); }
        let Some((backend, id)) = identifier.split_once(':') else { return Ok(()); };
        match backend {
            "hypr" => run_shell(&format!("hyprctl dispatch focuswindow address:{id}")),
            "sway" => run_shell(&format!("swaymsg '[con_id={id}] focus'")),
            "niri" => run_shell(&format!("niri msg action focus-window --id {id}")),
            "wmctrl" => run_shell(&format!("wmctrl -ia {id}")),
            _ => Ok(()),
        }
    }

    fn capability(&self) -> ProviderCapability {
        ProviderCapability {
            name: self.name().into(),
            name_pretty: self.pretty_name().into(),
            description: "Search and focus open windows".into(),
            prefixes: Vec::new(),
            actions: action_map(&[("focus", ActionCapability::new("Focus"))]),
            supports_query: true,
            supports_activate: true,
            supports_streaming: true,
            supports_subscriptions: false,
            emits_events: false,
        }
    }
}

fn discover_windows() -> Vec<Window> {
    hypr_windows().or_else(sway_windows).or_else(niri_windows).or_else(wmctrl_windows).unwrap_or_default()
}

fn hypr_windows() -> Option<Vec<Window>> {
    let raw = command_output("hyprctl", &["clients", "-j"])?;
    let clients: Vec<HyprClient> = serde_json::from_str(&raw).ok()?;
    Some(clients.into_iter().map(|c| Window { id: c.address, title: c.title, app: c.class, workspace: c.workspace.name, backend: "hypr" }).collect())
}

fn sway_windows() -> Option<Vec<Window>> {
    let raw = command_output("swaymsg", &["-t", "get_tree"])?;
    let root: SwayNode = serde_json::from_str(&raw).ok()?;
    let mut out = Vec::new();
    collect_sway(&root, &mut out);
    Some(out)
}

fn collect_sway(node: &SwayNode, out: &mut Vec<Window>) {
    if let Some(name) = &node.name {
        let app = node.app_id.clone().or_else(|| node.window_properties.as_ref().and_then(|p| p.class.clone())).unwrap_or_default();
        if !app.is_empty() { out.push(Window { id: node.id.to_string(), title: name.clone(), app, workspace: String::new(), backend: "sway" }); }
    }
    if let Some(nodes) = &node.nodes { for n in nodes { collect_sway(n, out); } }
    if let Some(nodes) = &node.floating_nodes { for n in nodes { collect_sway(n, out); } }
}

fn niri_windows() -> Option<Vec<Window>> {
    let raw = command_output("niri", &["msg", "--json", "windows"])?;
    let values: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let arr = values.as_array()?;
    Some(arr.iter().filter_map(|v| Some(Window {
        id: v.get("id")?.to_string(),
        title: v.get("title").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
        app: v.get("app_id").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
        workspace: v.get("workspace_id").map(|v| v.to_string()).unwrap_or_default(),
        backend: "niri",
    })).collect())
}

fn wmctrl_windows() -> Option<Vec<Window>> {
    let raw = command_output("wmctrl", &["-lx"])?;
    Some(raw.lines().filter_map(|line| {
        let mut parts = line.split_whitespace();
        let id = parts.next()?.to_string();
        let _desktop = parts.next();
        let app = parts.next().unwrap_or_default().to_string();
        let _host = parts.next();
        let title = parts.collect::<Vec<_>>().join(" ");
        Some(Window { id, title, app, workspace: String::new(), backend: "wmctrl" })
    }).collect())
}
