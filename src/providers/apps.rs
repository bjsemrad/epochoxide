use super::{run_shell, Provider};
use crate::{config::Config, fuzzy, types::Item};
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
        let mut dirs = Vec::new();
        if let Some(data_home) = dirs::data_dir() { dirs.push(data_home.join("applications")); }
        if let Ok(data_dirs) = std::env::var("XDG_DATA_DIRS") {
            dirs.extend(data_dirs.split(':').map(|d| PathBuf::from(d).join("applications")));
        } else {
            dirs.push(PathBuf::from("/usr/share/applications"));
            dirs.push(PathBuf::from("/usr/local/share/applications"));
        }

        for dir in dirs {
            if !dir.exists() { continue; }
            for entry in walkdir::WalkDir::new(&dir).follow_links(true).into_iter().filter_map(|e| e.ok()) {
                if entry.path().extension().and_then(|e| e.to_str()) == Some("desktop") {
                    if let Ok(app) = parse_desktop(entry.path()) {
                        if !app.name.is_empty() && self.visible(&app) {
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
}

impl Provider for AppsProvider {
    fn name(&self) -> &'static str { "apps" }
    fn pretty_name(&self) -> &'static str { "Desktop Applications" }

    fn query(&mut self, query: &str, limit: usize, exact: bool) -> Vec<Item> {
        let mut items = Vec::new();
        for app in &self.apps {
            if let Some((score, info)) = fuzzy::score(query, &app.search, exact, "text") {
                let mut item = Item::new(self.name(), &app.id, &app.name);
                item.subtext = if app.generic_name.is_empty() { app.comment.clone() } else { app.generic_name.clone() };
                item.icon = app.icon.clone();
                item.actions = vec!["open".into()];
                item.score = score + 20_000;
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
        items.sort_by(|a, b| b.score.cmp(&a.score));
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
        match key.split('[').next().unwrap_or(key) {
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
    use super::clean_exec;

    #[test]
    fn removes_desktop_exec_field_codes() {
        assert_eq!(clean_exec("firefox %u"), "firefox");
    }
}
