use super::{command_output, run_shell, Provider};
use crate::{fuzzy, types::{action_map, ActionCapability, Item, ProviderCapability}};
use anyhow::Result;
use serde::Deserialize;
use std::{collections::HashMap, io::{Read, Write}, os::unix::net::UnixStream, path::PathBuf, time::Duration};

pub struct WindowsProvider { wm_class_icons: HashMap<String, String> }

impl WindowsProvider {
    pub fn new(wm_class_icons: HashMap<String, String>) -> Self { Self { wm_class_icons } }

    fn icon_for(&self, app: &str) -> String {
        icon_candidates(app).into_iter()
            .find_map(|key| self.wm_class_icons.get(&key).cloned())
            .unwrap_or_else(|| "preferences-system-windows".into())
    }
}

const BROWSER_PREFIXES: [&str; 6] = ["google-chrome-", "microsoft-edge-", "chromium-", "chrome-", "brave-", "vivaldi-"];

/// Desktop-entry keys to try for a window class, best first.
///
/// An exact match covers well-behaved apps. Chromium-family web apps do not report the
/// `StartupWMClass` their .desktop file declares — they report `brave-gmail.com__-Default` or
/// `brave-mail.proton.me__u_0_inbox-Default` — so the host is peeled out of the class and tried
/// as a name, then by domain, and finally the browser itself so a web app at least gets the
/// browser's icon instead of a generic window.
fn icon_candidates(app: &str) -> Vec<String> {
    let key = app.to_lowercase();
    let mut out = vec![key.clone()];

    let base = key.split("__").next().unwrap_or(&key).trim_end_matches("-default").to_string();
    out.push(base.clone());

    let browser = BROWSER_PREFIXES.iter().find(|prefix| base.starts_with(**prefix));
    let host = browser.and_then(|prefix| base.strip_prefix(*prefix)).unwrap_or(&base).to_string();
    out.push(host.clone());

    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() > 1 {
        out.push(labels[0].to_string());
        for start in 1..labels.len() - 1 { out.push(labels[start..].join(".")); }
        out.push(labels[labels.len() - 1].to_string());
    }

    if let Some(prefix) = browser {
        let name = prefix.trim_end_matches('-');
        out.push(format!("{name}-browser"));
        out.push(name.to_string());
    }

    let mut seen = std::collections::HashSet::new();
    out.into_iter().filter(|key| !key.is_empty() && seen.insert(key.clone())).collect()
}

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
                item.icon = self.icon_for(&window.app);
                item.actions = vec!["focus".into(), "close".into()];
                item.score = score + 5_000;
                item.fuzzyinfo = Some(info);
                out.push(item);
            }
        }
        out.truncate(limit);
        out
    }

    fn activate(&mut self, identifier: &str, action: &str, _query: &str, _arguments: &str) -> Result<()> {
        let Some((backend, id)) = identifier.split_once(':') else { return Ok(()); };
        match (action, backend) {
            ("focus", "hypr") => hypr_activate(&format!("dispatch focuswindow address:{id}")),
            ("focus", "sway") => run_shell(&format!("swaymsg '[con_id={id}] focus'")),
            ("focus", "niri") => run_shell(&format!("{} msg action focus-window --id {id}", niri_shell())),
            ("focus", "wmctrl") => run_shell(&format!("wmctrl -ia {id}")),
            ("close", "hypr") => hypr_activate(&format!("dispatch closewindow address:{id}")),
            ("close", "sway") => run_shell(&format!("swaymsg '[con_id={id}] kill'")),
            ("close", "niri") => run_shell(&format!("{} msg action close-window --id {id}", niri_shell())),
            ("close", "wmctrl") => run_shell(&format!("wmctrl -ic {id}")),
            _ => anyhow::bail!("unsupported windows action: {action}"),
        }
    }

    fn capability(&self) -> ProviderCapability {
        ProviderCapability {
            name: self.name().into(),
            name_pretty: self.pretty_name().into(),
            description: "Search, focus, and close open windows".into(),
            prefixes: Vec::new(),
            actions: action_map(&[
                ("focus", ActionCapability::new("Focus")),
                ("close", ActionCapability::new("Close").destructive()),
            ]),
            supports_query: true,
            supports_activate: true,
            supports_streaming: true,
            supports_subscriptions: false,
            emits_events: false,
        }
    }
}

fn discover_windows() -> Vec<Window> {
    let backends = candidate_backends();
    for name in backends {
        let windows = match name {
            "hypr" => hypr_windows(),
            "sway" => sway_windows(),
            "niri" => niri_windows(),
            _ => wmctrl_windows(),
        };
        if let Some(windows) = windows { return windows; }
    }
    Vec::new()
}

/// Returns compositor backends to try in order. Env hints pick the preferred
/// backend; without them (typical under systemd), each backend probes its own
/// IPC/CLI path and we fall back to wmctrl last.
fn candidate_backends() -> Vec<&'static str> {
    match backend_from_env() {
        Backend::Hypr => vec!["hypr", "niri", "sway", "wmctrl"],
        Backend::Sway => vec!["sway", "hypr", "niri", "wmctrl"],
        Backend::Niri => vec!["niri", "hypr", "sway", "wmctrl"],
        // No env hint (typical under systemd): probe everything.
        Backend::Wmctrl => vec!["hypr", "niri", "sway", "wmctrl"],
    }
}

fn backend_from_env() -> Backend {
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() { return Backend::Hypr; }
    if std::env::var_os("SWAYSOCK").is_some() { return Backend::Sway; }
    if std::env::var_os("NIRI_SOCKET").is_some() { return Backend::Niri; }
    if let Ok(desktop) = std::env::var("XDG_CURRENT_DESKTOP") {
        let d = desktop.to_lowercase();
        if d.contains("hypr") { return Backend::Hypr; }
        if d.contains("sway") { return Backend::Sway; }
        if d.contains("niri") { return Backend::Niri; }
    }
    Backend::Wmctrl
}

/// Active backend based on environment hints only.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Backend { Hypr, Sway, Niri, Wmctrl }

fn hypr_windows() -> Option<Vec<Window>> {
    let raw = hypr_ipc("j/clients").or_else(|| hypr_output(&["clients", "-j"]))?;
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
    let raw = niri_output(&["msg", "--json", "windows"])?;
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

fn hypr_output(args: &[&str]) -> Option<String> {
    let mut command = std::process::Command::new("hyprctl");
    command.args(args);
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        if let Some(sig) = hypr_signature() { command.env("HYPRLAND_INSTANCE_SIGNATURE", sig); }
    }
    output(command)
}

fn niri_output(args: &[&str]) -> Option<String> {
    let mut command = std::process::Command::new("niri");
    command.args(args);
    if std::env::var_os("NIRI_SOCKET").is_none() {
        if let Some(socket) = niri_socket() { command.env("NIRI_SOCKET", socket); }
    }
    output(command)
}

fn output(mut command: std::process::Command) -> Option<String> {
    let out = command.output().ok()?;
    if out.status.success() { Some(String::from_utf8_lossy(&out.stdout).trim().to_string()) } else { None }
}

fn hypr_activate(command: &str) -> Result<()> {
    if hypr_ipc(command).is_some() { return Ok(()); }
    run_shell(&format!("{} {command}", hypr_shell()))
}

fn hypr_ipc(command: &str) -> Option<String> {
    let socket = hypr_socket_path()?;
    let mut stream = UnixStream::connect(socket).ok()?;
    let timeout = Some(Duration::from_millis(300));
    let _ = stream.set_read_timeout(timeout);
    let _ = stream.set_write_timeout(timeout);
    stream.write_all(command.as_bytes()).ok()?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut out = String::new();
    stream.read_to_string(&mut out).ok()?;
    if out.trim().is_empty() { None } else { Some(out.trim().to_string()) }
}

fn hypr_socket_path() -> Option<PathBuf> {
    let sig = hypr_signature()?;
    let socket = std::path::Path::new(&runtime_dir()).join("hypr").join(sig).join(".socket.sock");
    socket.exists().then_some(socket)
}

fn hypr_shell() -> String {
    hypr_signature().map(|sig| format!("HYPRLAND_INSTANCE_SIGNATURE={} hyprctl", shell_quote(&sig))).unwrap_or_else(|| "hyprctl".into())
}

fn niri_shell() -> String {
    niri_socket().map(|socket| format!("NIRI_SOCKET={} niri", shell_quote(&socket))).unwrap_or_else(|| "niri".into())
}

fn hypr_signature() -> Option<String> {
    if let Ok(sig) = std::env::var("HYPRLAND_INSTANCE_SIGNATURE") { if !sig.is_empty() { return Some(sig); } }
    let rt = runtime_dir();
    let dir = std::path::Path::new(&rt).join("hypr");
    let mut latest: Option<(std::time::SystemTime, String)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if !path.join(".socket.sock").exists() { continue; }
        let sig = entry.file_name().to_string_lossy().to_string();
        let modified = entry.metadata().and_then(|m| m.modified()).unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if latest.as_ref().is_none_or(|(time, _)| modified > *time) { latest = Some((modified, sig)); }
    }
    latest.map(|(_, sig)| sig)
}

fn niri_socket() -> Option<String> {
    if let Ok(socket) = std::env::var("NIRI_SOCKET") { if !socket.is_empty() { return Some(socket); } }
    let rt = runtime_dir();
    [format!("{rt}/niri.sock"), format!("{rt}/niri-ipc/niri.sock")].into_iter().find(|p| std::path::Path::new(p).exists())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn runtime_dir() -> String {
    std::env::var("XDG_RUNTIME_DIR")
        .or_else(|_| std::env::var("UID").map(|uid| format!("/run/user/{uid}")))
        .unwrap_or_else(|_| "/run/user/1000".into())
}

#[cfg(test)]
mod tests {
    use super::{backend_from_env, icon_candidates, Backend, WindowsProvider};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::OnceLock;

    /// Tests mutate process env vars, which are global. Serialize them.
    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    fn set(vars: &[(&str, &str)]) {
        for (k, v) in vars { std::env::set_var(k, v); }
        for k in ["SWAYSOCK", "NIRI_SOCKET", "HYPRLAND_INSTANCE_SIGNATURE", "XDG_CURRENT_DESKTOP"] {
            if !vars.iter().any(|(kk, _)| *kk == k) { std::env::remove_var(k); }
        }
    }

    #[test]
    fn detects_hyprland_from_instance_signature() {
        let _g = lock_env();
        set(&[("HYPRLAND_INSTANCE_SIGNATURE", "s")]);
        assert_eq!(backend_from_env(), Backend::Hypr);
    }

    #[test]
    fn detects_niri_from_xdg_desktop() {
        let _g = lock_env();
        set(&[("XDG_CURRENT_DESKTOP", "niri")]);
        assert_eq!(backend_from_env(), Backend::Niri);
    }

    #[test]
    fn defaults_to_wmctrl_without_signals() {
        let _g = lock_env();
        set(&[]);
        assert_eq!(backend_from_env(), Backend::Wmctrl);
    }

    fn provider(entries: &[(&str, &str)]) -> WindowsProvider {
        WindowsProvider::new(entries.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<HashMap<_, _>>())
    }

    #[test]
    fn matches_window_class_exactly() {
        let provider = provider(&[("com.mitchellh.ghostty", "ghostty-icon")]);
        assert_eq!(provider.icon_for("com.mitchellh.ghostty"), "ghostty-icon");
    }

    #[test]
    fn matches_chromium_webapp_class_by_host() {
        // Brave reports these classes; the .desktop files declare StartupWMClass=gmail / proton.me.
        let provider = provider(&[("gmail", "gmail-icon"), ("proton.me", "proton-icon")]);
        assert_eq!(provider.icon_for("brave-gmail.com__-Default"), "gmail-icon");
        assert_eq!(provider.icon_for("brave-mail.proton.me__u_0_inbox-Default"), "proton-icon");
    }

    #[test]
    fn falls_back_to_the_browser_then_a_generic_window() {
        let provider = provider(&[("brave-browser", "brave-icon")]);
        assert_eq!(provider.icon_for("brave-unknown.example__-Default"), "brave-icon");
        assert_eq!(provider.icon_for("some-unknown-app"), "preferences-system-windows");
    }

    #[test]
    fn prefers_earlier_candidates() {
        let candidates = icon_candidates("brave-gmail.com__-Default");
        let gmail = candidates.iter().position(|c| c == "gmail").unwrap();
        let brave = candidates.iter().position(|c| c == "brave-browser").unwrap();
        assert!(gmail < brave);
    }
}
