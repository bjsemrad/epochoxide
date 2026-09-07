use super::{run_shell, Provider};
use crate::{config::{expand, Config}, fuzzy, types::Item};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::{collections::HashMap, fs, path::Path};

#[derive(Debug, Clone, Deserialize, Default)]
struct Menu {
    name: String,
    name_pretty: Option<String>,
    icon: Option<String>,
    action: Option<String>,
    actions: Option<HashMap<String, String>>,
    entries: Option<Vec<MenuEntry>>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct MenuEntry {
    text: String,
    value: Option<String>,
    subtext: Option<String>,
    icon: Option<String>,
    keywords: Option<Vec<String>>,
    actions: Option<HashMap<String, String>>,
    submenu: Option<String>,
    #[serde(rename = "async")]
    async_command: Option<String>,
}

pub struct MenusProvider { menus: HashMap<String, Menu> }

impl MenusProvider {
    pub fn new(config: Config) -> Result<Self> {
        let mut menus = HashMap::new();
        let dir = expand(&config.menus_dir);
        if Path::new(&dir).exists() {
            for entry in fs::read_dir(&dir).with_context(|| format!("reading menu dir {dir}"))? {
                let path = entry?.path();
                if path.extension().and_then(|e| e.to_str()) != Some("toml") { continue; }
                let raw = fs::read_to_string(&path)?;
                let menu: Menu = toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
                if !menu.name.is_empty() { menus.insert(menu.name.clone(), menu); }
            }
        }
        Ok(Self { menus })
    }
}

impl Provider for MenusProvider {
    fn name(&self) -> &'static str { "menus" }
    fn pretty_name(&self) -> &'static str { "Menus" }

    fn query(&mut self, query: &str, limit: usize, exact: bool) -> Vec<Item> {
        let mut out = Vec::new();
        for menu in self.menus.values() {
            let text = menu.name_pretty.as_deref().unwrap_or(&menu.name);
            if let Some((score, info)) = fuzzy::score(query, text, exact, "text") {
                let mut item = Item::new(self.name(), format!("menus:{}", menu.name), text);
                item.subtext = "Custom menu".into();
                item.icon = menu.icon.clone().unwrap_or_else(|| "applications-other".into());
                item.actions = vec!["open".into()];
                item.score = score + 2_000;
                item.fuzzyinfo = Some(info);
                out.push(item);
            }
        }
        out.truncate(limit);
        out
    }

    fn menu(&mut self, menu_name: &str) -> Vec<Item> {
        let Some(menu) = self.menus.get(menu_name) else { return Vec::new(); };
        menu.entries.clone().unwrap_or_default().into_iter().enumerate().map(|(idx, entry)| {
            let mut item = Item::new(self.name(), format!("{menu_name}:{idx}"), entry.text.clone());
            item.subtext = entry.subtext.unwrap_or_else(|| {
                entry.value.clone().or_else(|| entry.keywords.clone().map(|k| k.join(" "))).unwrap_or_default()
            });
            item.icon = entry.icon.or_else(|| menu.icon.clone()).unwrap_or_default();
            item.actions = entry.actions.as_ref().map(|a| a.keys().cloned().collect()).unwrap_or_else(|| vec!["default".into()]);
            if entry.submenu.is_some() { item.actions.push("open".into()); }
            if let Some(cmd) = entry.async_command { item.preview = command_preview(&cmd); item.preview_type = "text".into(); }
            item.score = 1_000_000 - idx as i32;
            item
        }).collect()
    }

    fn activate(&mut self, identifier: &str, action: &str, _query: &str, arguments: &str) -> Result<()> {
        if let Some(menu_name) = identifier.strip_prefix("menus:") {
            let _ = menu_name;
            return Ok(());
        }
        let (menu_name, idx) = identifier.split_once(':').context("invalid menu identifier")?;
        let idx: usize = idx.parse()?;
        let menu = self.menus.get(menu_name).context("menu not found")?;
        let entry = menu.entries.as_ref().and_then(|e| e.get(idx)).context("menu entry not found")?;
        if action == "open" && entry.submenu.is_some() { return Ok(()); }
        let mut command = entry.actions.as_ref().and_then(|a| a.get(action)).cloned()
            .or_else(|| menu.actions.as_ref().and_then(|a| a.get(action)).cloned())
            .or_else(|| menu.action.clone());
        if action == "default" && command.is_none() { command = menu.action.clone(); }
        let Some(mut command) = command else { return Ok(()); };
        let value = entry.value.as_deref().unwrap_or(&entry.text);
        command = command.replace("%VALUE%", value).replace("%ARGS%", arguments);
        run_shell(&command)
    }
}

fn command_preview(command: &str) -> String {
    std::process::Command::new("sh").arg("-c").arg(command).output().ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::Menu;

    #[test]
    fn parses_toml_menu() {
        let raw = "name = 'bookmarks'\naction = 'xdg-open %VALUE%'\n[[entries]]\ntext = 'Rust'\nvalue = 'https://rust-lang.org'\n";
        let menu: Menu = toml::from_str(raw).unwrap();
        assert_eq!(menu.name, "bookmarks");
        assert_eq!(menu.entries.unwrap()[0].text, "Rust");
    }
}
