use super::{run_shell, Provider};
use crate::{config::Config, fuzzy, types::{action_map, ActionCapability, Item, ProviderCapability}};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, path::{Path, PathBuf}};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DesktopEntry {
    id: String,
    name: String,
    generic_name: String,
    comment: String,
    exec: String,
    icon: String,
    keywords: Vec<String>,
    categories: Vec<String>,
    no_display: bool,
    hidden: bool,
    terminal: bool,
    only_show_in: Vec<String>,
    not_show_in: Vec<String>,
    startup_wm_class: String,
    actions: HashMap<String, String>,
    search: String,
}

pub struct AppsProvider {
    config: Config,
    apps: Vec<DesktopEntry>,
    desktops: Vec<String>,
}

impl AppsProvider {
    pub fn new(config: Config) -> Result<Self> {
        let desktops = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().split(':').map(str::to_string).collect();
        let mut this = Self { config, apps: Vec::new(), desktops };
        this.reload()?;
        Ok(this)
    }

    fn reload(&mut self) -> Result<()> {
        self.apps.clear();
        let mut seen = std::collections::HashSet::new();

        for dir in application_dirs() {
            if !dir.exists() { continue; }
            for entry in walkdir::WalkDir::new(&dir).follow_links(true).into_iter().filter_map(|e| e.ok()) {
                if entry.path().extension().and_then(|e| e.to_str()) == Some("desktop") {
                    if let Ok(app) = parse_desktop(entry.path()) {
                        if !app.name.is_empty() && self.visible(&app) && seen.insert(app.id.clone()) {
                            self.apps.push(app);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn visible(&self, app: &DesktopEntry) -> bool {
        if app.hidden || app.no_display { return false; }
        if !app.only_show_in.is_empty() && !app.only_show_in.iter().any(|d| self.desktops.contains(d)) { return false; }
        if app.not_show_in.iter().any(|d| self.desktops.contains(d)) { return false; }
        true
    }

    pub fn wm_class_icons(&self) -> HashMap<String, String> {
        self.apps.iter().filter(|app| !app.icon.is_empty()).map(|app| {
            let key = if !app.startup_wm_class.is_empty() { app.startup_wm_class.clone() } else { app.id.trim_end_matches(".desktop").to_string() };
            (key.to_lowercase(), app.icon.clone())
        }).collect()
    }
}

impl Provider for AppsProvider {
    fn name(&self) -> &'static str { "apps" }
    fn pretty_name(&self) -> &'static str { "Desktop Applications" }

    fn query(&mut self, query: &str, limit: usize, exact: bool) -> Vec<Item> {
        let query_lower = query.to_lowercase();
        let mut items = Vec::new();
        for app in &self.apps {
            if let Some((score, info)) = fuzzy::score_lower(&query_lower, &app.search, exact, "text") {
                let mut item = Item::new(self.name(), &app.id, &app.name);
                item.subtext = if app.generic_name.is_empty() { app.comment.clone() } else { app.generic_name.clone() };
                item.icon = app.icon.clone();
                item.actions = vec!["open".into()];
                item.score = score + 20_000 + app_name_bonus(&query_lower, &app.name);
                item.fuzzyinfo = Some(info);
                items.push(item);
            }
            for action in app.actions.keys() {
                if query.is_empty() { continue; }
                if let Some((score, info)) = fuzzy::score(query, action, exact, "text") {
                    let mut item = Item::new(self.name(), format!("{}:{action}", app.id), format!("{}: {action}", app.name));
                    item.icon = app.icon.clone();
                    item.actions = vec!["open".into()];
                    item.score = score + 10_000;
                    item.fuzzyinfo = Some(info);
                    items.push(item);
                }
            }
        }
        items.sort_by_key(|item| std::cmp::Reverse(item.score));
        items.truncate(limit);
        items
    }

    fn activate(&mut self, identifier: &str, action: &str, _query: &str, _arguments: &str) -> Result<()> {
        if action != "open" { anyhow::bail!("unsupported apps action: {action}"); }
        let (id, desktop_action) = identifier.split_once(':').map_or((identifier, None), |(id, a)| (id, Some(a)));
        let app = self.apps.iter().find(|a| a.id == id).context("app not found")?;
        let exec = desktop_action.and_then(|a| app.actions.get(a)).unwrap_or(&app.exec);
        let command = if self.config.launch_prefix.is_empty() { exec.clone() } else { format!("{} {}", self.config.launch_prefix, exec) };
        run_shell(&command)
    }

    fn capability(&self) -> ProviderCapability {
        ProviderCapability {
            name: self.name().into(),
            name_pretty: self.pretty_name().into(),
            description: "Search and launch desktop applications".into(),
            prefixes: Vec::new(),
            actions: action_map(&[("open", ActionCapability::new("Open"))]),
            supports_query: true,
            supports_activate: true,
            supports_streaming: true,
            supports_subscriptions: false,
            emits_events: false,
        }
    }
}

/// Desktop entry directories, in precedence order. XDG_DATA_DIRS is honoured when present, but
/// a service started by systemd frequently inherits a near-empty environment (no XDG_DATA_DIRS
/// at all), which would silently reduce the app list — and every icon derived from it, including
/// window icons — to whatever lives under XDG_DATA_HOME. The profile locations below are probed
/// unconditionally for that case; missing ones are skipped.
fn application_dirs() -> Vec<PathBuf> {
    let mut bases = Vec::new();
    if let Some(data_home) = dirs::data_dir() { bases.push(data_home); }
    if let Ok(data_dirs) = std::env::var("XDG_DATA_DIRS") {
        bases.extend(data_dirs.split(':').filter(|d| !d.is_empty()).map(PathBuf::from));
    }
    if let Some(home) = dirs::home_dir() {
        bases.push(home.join(".nix-profile/share"));
        if let Some(user) = home.file_name() {
            bases.push(PathBuf::from("/etc/profiles/per-user").join(user).join("share"));
        }
    }
    bases.extend([
        "/run/current-system/sw/share",
        "/nix/var/nix/profiles/default/share",
        "/usr/local/share",
        "/usr/share",
    ].map(PathBuf::from));

    let mut seen = std::collections::HashSet::new();
    bases.into_iter().map(|base| base.join("applications")).filter(|dir| seen.insert(dir.clone()) && dir.exists()).collect()
}

fn app_name_bonus(query_lower: &str, name: &str) -> i32 {
    if query_lower.is_empty() { return 0; }
    let name_lower = name.to_lowercase();
    let Some(start) = name_lower.find(query_lower) else { return 0; };
    15_000 + if start == 0 { 5_000 } else { 0 }
}

fn parse_desktop(path: &Path) -> Result<DesktopEntry> {
    let raw = fs::read_to_string(path)?;
    let mut app = DesktopEntry { id: path.file_name().unwrap_or_default().to_string_lossy().to_string(), ..Default::default() };
    let mut in_entry = false;
    let mut current_action: Option<String> = None;
    for line in raw.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') { continue; }
        if line == "[Desktop Entry]" { in_entry = true; current_action = None; continue; }
        if let Some(name) = line.strip_prefix("[Desktop Action ").and_then(|s| s.strip_suffix(']')) { current_action = Some(name.to_string()); in_entry = false; continue; }
        let Some((key, val)) = line.split_once('=') else { continue; };
        if let Some(action) = &current_action {
            if key == "Exec" { app.actions.insert(action.clone(), clean_exec(val)); }
            continue;
        }
        if !in_entry { continue; }
        match key {
            "Name" if app.name.is_empty() => app.name = val.to_string(),
            "GenericName" if app.generic_name.is_empty() => app.generic_name = val.to_string(),
            "Comment" if app.comment.is_empty() => app.comment = val.to_string(),
            "Exec" => app.exec = clean_exec(val),
            "Icon" => app.icon = val.to_string(),
            "Keywords" => app.keywords = split_list(val),
            "Categories" => app.categories = split_list(val),
            "NoDisplay" => app.no_display = val.eq_ignore_ascii_case("true"),
            "Hidden" => app.hidden = val.eq_ignore_ascii_case("true"),
            "Terminal" => app.terminal = val.eq_ignore_ascii_case("true"),
            "OnlyShowIn" => app.only_show_in = split_list(val),
            "NotShowIn" => app.not_show_in = split_list(val),
            "StartupWMClass" => app.startup_wm_class = val.to_string(),
            _ => {}
        }
    }
    app.search = format!("{} {} {} {}", app.name, app.generic_name, app.comment, app.keywords.join(" ")).to_lowercase();
    Ok(app)
}

fn clean_exec(input: &str) -> String {
    const CODES: [&str; 15] = ["%f", "%F", "%u", "%U", "%d", "%D", "%n", "%N", "%i", "%c", "%k", "%v", "%m", "%%", "%" ];
    let mut out = input.to_string();
    for code in CODES { out = out.replace(code, ""); }
    out.trim().to_string()
}

fn split_list(value: &str) -> Vec<String> {
    value.split(';').filter(|s| !s.is_empty()).map(str::to_string).collect()
}

#[cfg(test)]
mod tests {
    use super::{app_name_bonus, clean_exec, parse_desktop, AppsProvider, DesktopEntry};
    use crate::config::Config;

    #[test]
    fn removes_desktop_exec_field_codes() {
        assert_eq!(clean_exec("firefox %u"), "firefox");
    }

    #[test]
    fn ignores_localized_name_variants_regardless_of_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blueman-manager.desktop");
        std::fs::write(&path, "[Desktop Entry]\nName[de]=Bluetooth-Verwaltung\nName=Bluetooth Manager\nComment[de]=Verwalten Sie Bluetooth-Geräte\nComment=Manage Bluetooth devices\n").unwrap();
        let app = parse_desktop(&path).unwrap();
        assert_eq!(app.name, "Bluetooth Manager");
        assert_eq!(app.comment, "Manage Bluetooth devices");
    }

    #[test]
    fn parses_startup_wm_class() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("firefox.desktop");
        std::fs::write(&path, "[Desktop Entry]\nName=Firefox\nIcon=firefox\nStartupWMClass=firefox\n").unwrap();
        let app = parse_desktop(&path).unwrap();
        assert_eq!(app.startup_wm_class, "firefox");
    }

    #[test]
    fn wm_class_icons_prefers_startup_wm_class_over_id() {
        let provider = AppsProvider {
            config: Config::default(),
            apps: vec![
                DesktopEntry { id: "org.foo.Bar.desktop".into(), startup_wm_class: "foobar".into(), icon: "foo-icon".into(), ..Default::default() },
                DesktopEntry { id: "baz.desktop".into(), icon: "baz-icon".into(), ..Default::default() },
            ],
            desktops: Vec::new(),
        };
        let icons = provider.wm_class_icons();
        assert_eq!(icons.get("foobar"), Some(&"foo-icon".to_string()));
        assert_eq!(icons.get("baz"), Some(&"baz-icon".to_string()));
    }

    #[test]
    fn visible_name_exact_match_gets_history_sized_bonus() {
        assert!(app_name_bonus("system info", "System Info") > 10_000);
        assert_eq!(app_name_bonus("system info", "Thunar File Manager"), 0);
    }

    #[test]
    fn reload_deduplicates_desktop_ids() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("ghostty.desktop");
        let second = dir.path().join("nested").join("ghostty.desktop");
        std::fs::create_dir_all(second.parent().unwrap()).unwrap();
        std::fs::write(&first, "[Desktop Entry]\nName=Ghostty\nExec=ghostty\n").unwrap();
        std::fs::write(&second, "[Desktop Entry]\nName=Ghostty\nExec=ghostty\n").unwrap();

        let mut seen = std::collections::HashSet::new();
        let apps = [first, second].into_iter()
            .filter_map(|path| parse_desktop(&path).ok())
            .filter(|app| seen.insert(app.id.clone()))
            .collect::<Vec<_>>();

        assert_eq!(apps.len(), 1);
    }
}
