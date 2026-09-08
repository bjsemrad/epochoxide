use super::{copy_text, run_shell, Provider};
use crate::{config::{expand, Config}, fuzzy, types::{action_map, ActionCapability, Item, ProviderCapability}};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::{
    collections::HashMap,
    fs,
    path::Path,
    sync::{atomic::{AtomicBool, Ordering}, Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

/// How long a `command`-backed menu reuses the entries its generator produced, unless the menu
/// overrides it with `cache_ms`. Menus are generated live (the keybinds menu asks the compositor
/// for its binds), so opening one always re-runs the generator; this only stops it running again
/// per keystroke while the user searches within what it returned.
const ENTRY_TTL: Duration = Duration::from_millis(10_000);

/// Regenerations closer together than this are skipped, so the shell's own repeat empty queries
/// do not start a generator several times over.
const MIN_REFRESH: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, Deserialize, Default)]
struct Menu {
    name: String,
    name_pretty: Option<String>,
    description: Option<String>,
    icon: Option<String>,
    action: Option<String>,
    command: Option<String>,
    cache_ms: Option<u64>,
    actions: Option<HashMap<String, String>>,
    entries: Option<Vec<MenuEntry>>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct MenuEntry {
    text: String,
    value: Option<String>,
    copy: Option<String>,
    subtext: Option<String>,
    icon: Option<String>,
    keywords: Option<Vec<String>>,
    actions: Option<HashMap<String, String>>,
    submenu: Option<String>,
    #[serde(rename = "async")]
    async_command: Option<String>,
}

impl MenuEntry {
    /// Enter copies rather than runs when the entry carries `copy`.
    fn is_copy(&self) -> bool { self.copy.is_some() }

    /// Actions in a stable order, `default`/`copy` first, so the shell's "first action is Enter"
    /// rule lands on the same one every time (a HashMap's own order is not stable between runs).
    fn action_names(&self, menu: &Menu) -> Vec<String> {
        if self.is_copy() { return vec!["copy".into()]; }
        let mut names: Vec<String> = self.actions.as_ref().or(menu.actions.as_ref())
            .map(|a| a.keys().cloned().collect()).unwrap_or_default();
        names.sort();
        if names.is_empty() { names.push("default".into()); }
        if let Some(pos) = names.iter().position(|n| n == "default") { names.swap(0, pos); }
        if self.submenu.is_some() { names.push("open".into()); }
        names
    }

    fn haystack(&self) -> String {
        let mut out = self.text.clone();
        if let Some(subtext) = &self.subtext { out.push(' '); out.push_str(subtext); }
        if let Some(keywords) = &self.keywords { out.push(' '); out.push_str(&keywords.join(" ")); }
        out
    }
}

/// One configured menu, exposed as a provider of its own.
///
/// Menus used to be a single `menus` provider whose results were the menus themselves, so reaching
/// an entry meant finding the menu and then drilling into it. Each menu is its own provider now:
/// it takes a query prefix like any other (`[query_prefixes]` in the config), searches its entries
/// directly, and shows up in the launcher's provider picker under its own name and icon.
pub struct MenuProvider {
    name: String,
    pretty: String,
    description: String,
    icon: String,
    menu: Menu,
    cache: Arc<Mutex<Option<(Instant, Vec<MenuEntry>)>>>,
    refreshing: Arc<AtomicBool>,
}

impl MenuProvider {
    fn new(menu: Menu) -> Self {
        let provider = Self {
            name: menu.name.clone(),
            pretty: menu.name_pretty.clone().unwrap_or_else(|| menu.name.clone()),
            description: menu.description.clone().unwrap_or_else(|| "Custom menu".into()),
            icon: menu.icon.clone().unwrap_or_else(|| "applications-other".into()),
            menu,
            cache: Arc::new(Mutex::new(None)),
            refreshing: Arc::new(AtomicBool::new(false)),
        };
        // Generators are slow enough to be worth having ready before anyone asks: the keybinds
        // menu shells out to the compositor and takes seconds, which is a launcher that shows
        // nothing for seconds if the first query is what starts it.
        if provider.menu.command.is_some() { provider.spawn_refresh(); }
        provider
    }

    fn generate(menu: &Menu) -> Vec<MenuEntry> {
        menu.command.as_deref().and_then(command_entries)
            .or_else(|| menu.entries.clone())
            .unwrap_or_default()
    }

    fn spawn_refresh(&self) {
        if self.refreshing.swap(true, Ordering::SeqCst) { return; }
        let menu = self.menu.clone();
        let cache = Arc::clone(&self.cache);
        let refreshing = Arc::clone(&self.refreshing);
        thread::spawn(move || {
            let entries = Self::generate(&menu);
            *cache.lock().unwrap() = Some((Instant::now(), entries));
            refreshing.store(false, Ordering::SeqCst);
        });
    }

    /// `opening` means the menu is being shown rather than searched within, which is when a
    /// dynamic menu should go and look at the thing it reports on again.
    ///
    /// A stale answer is served immediately and the regeneration happens behind it, so a slow
    /// generator costs the user nothing after the first time. Only a menu nobody has generated
    /// yet is waited on.
    fn entries(&mut self, opening: bool) -> Vec<MenuEntry> {
        let ttl = self.menu.cache_ms.map(Duration::from_millis).unwrap_or(ENTRY_TTL);
        let cached = self.cache.lock().unwrap().clone();
        if let Some((stamp, entries)) = cached {
            let fresh_enough = if opening { stamp.elapsed() < MIN_REFRESH } else { stamp.elapsed() < ttl };
            if !fresh_enough { self.spawn_refresh(); }
            return entries;
        }
        let entries = Self::generate(&self.menu);
        *self.cache.lock().unwrap() = Some((Instant::now(), entries.clone()));
        entries
    }

    fn item(&self, idx: usize, entry: &MenuEntry, total: usize) -> Item {
        let mut item = Item::new(&self.name, idx.to_string(), entry.text.clone());
        item.subtext = entry.subtext.clone().unwrap_or_else(|| {
            entry.copy.clone().or_else(|| entry.value.clone())
                .or_else(|| entry.keywords.clone().map(|k| k.join(" ")))
                .unwrap_or_default()
        });
        item.icon = entry.icon.clone().unwrap_or_else(|| self.icon.clone());
        item.actions = entry.action_names(&self.menu);
        if let Some(cmd) = &entry.async_command {
            item.preview = command_preview(cmd);
            item.preview_type = "text".into();
        }
        // Keeps the menu's own order when nothing is typed, where every fuzzy score is equal.
        item.score = (total - idx) as i32;
        item
    }
}

impl Provider for MenuProvider {
    fn name(&self) -> &str { &self.name }
    fn pretty_name(&self) -> &str { &self.pretty }

    fn query(&mut self, query: &str, limit: usize, exact: bool) -> Vec<Item> {
        let entries = self.entries(query.is_empty());
        let total = entries.len();
        let mut out = Vec::new();
        for (idx, entry) in entries.iter().enumerate() {
            if let Some((score, info)) = fuzzy::score(query, &entry.haystack(), exact, "text") {
                let mut item = self.item(idx, entry, total);
                item.score += score;
                item.fuzzyinfo = Some(info);
                out.push(item);
            }
        }
        out.sort_by(|a, b| b.score.cmp(&a.score));
        out.truncate(limit);
        out
    }

    fn menu(&mut self, _menu: &str) -> Vec<Item> {
        let entries = self.entries(true);
        let total = entries.len();
        entries.iter().enumerate().map(|(idx, entry)| self.item(idx, entry, total)).collect()
    }

    fn activate(&mut self, identifier: &str, action: &str, _query: &str, arguments: &str) -> Result<()> {
        let idx: usize = identifier.parse().context("invalid menu entry")?;
        let entries = self.entries(false);
        let entry = entries.get(idx).context("menu entry not found")?;
        if action == "open" && entry.submenu.is_some() { return Ok(()); }
        if entry.is_copy() || action == "copy" {
            let text = entry.copy.as_deref().or(entry.value.as_deref()).unwrap_or(&entry.text);
            return copy_text(text);
        }
        let mut command = entry.actions.as_ref().and_then(|a| a.get(action)).cloned()
            .or_else(|| self.menu.actions.as_ref().and_then(|a| a.get(action)).cloned())
            .or_else(|| self.menu.action.clone());
        if action == "default" && command.is_none() { command = self.menu.action.clone(); }
        let Some(mut command) = command else { return Ok(()); };
        let value = entry.value.as_deref().unwrap_or(&entry.text);
        command = command.replace("%VALUE%", value).replace("%ARGS%", arguments);
        run_shell(&command)
    }

    fn capability(&self) -> ProviderCapability {
        ProviderCapability {
            name: self.name.clone(),
            name_pretty: self.pretty.clone(),
            description: self.description.clone(),
            icon: self.icon.clone(),
            prefixes: Vec::new(),
            actions: action_map(&[
                ("default", ActionCapability::new("Run")),
                ("copy", ActionCapability::new("Copy")),
                ("open", ActionCapability::new("Open")),
            ]),
            supports_query: true,
            supports_activate: true,
            supports_streaming: true,
            supports_subscriptions: false,
            emits_events: false,
        }
    }
}

/// Loads every `*.toml` in the menus directory as its own provider, in name order.
pub fn providers(config: &Config) -> Result<Vec<Box<dyn Provider>>> {
    let dir = expand(&config.menus_dir);
    if !Path::new(&dir).exists() { return Ok(Vec::new()); }
    let mut menus = Vec::new();
    for entry in fs::read_dir(&dir).with_context(|| format!("reading menu dir {dir}"))? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") { continue; }
        let raw = fs::read_to_string(&path)?;
        let menu: Menu = toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        if !menu.name.is_empty() { menus.push(menu); }
    }
    menus.sort_by(|a, b| a.name.cmp(&b.name));
    menus.dedup_by(|a, b| a.name == b.name);
    Ok(menus.into_iter().map(|menu| Box::new(MenuProvider::new(menu)) as Box<dyn Provider>).collect())
}

fn command_entries(command: &str) -> Option<Vec<MenuEntry>> {
    let out = std::process::Command::new("sh").arg("-c").arg(command).output().ok()?;
    if !out.status.success() { return None; }
    serde_json::from_slice(&out.stdout).ok()
}

fn command_preview(command: &str) -> String {
    std::process::Command::new("sh").arg("-c").arg(command).output().ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{Menu, MenuEntry, MenuProvider};
    use crate::providers::Provider;

    fn menu(raw: &str) -> Menu { toml::from_str(raw).unwrap() }

    #[test]
    fn parses_toml_menu() {
        let menu = menu("name = 'bookmarks'\naction = 'xdg-open %VALUE%'\n[[entries]]\ntext = 'Rust'\nvalue = 'https://rust-lang.org'\n");
        assert_eq!(menu.name, "bookmarks");
        assert_eq!(menu.entries.unwrap()[0].text, "Rust");
    }

    #[test]
    fn each_menu_is_its_own_provider() {
        let provider = MenuProvider::new(menu("name = 'keybinds'\nname_pretty = 'Keybinds'\nicon = 'input-keyboard'\n"));
        let cap = provider.capability();
        assert_eq!(cap.name, "keybinds");
        assert_eq!(cap.name_pretty, "Keybinds");
        assert_eq!(cap.icon, "input-keyboard");
    }

    #[test]
    fn searches_entries_rather_than_menu_names() {
        let mut provider = MenuProvider::new(menu(
            "name = 'screenshots'\naction = '%VALUE%'\n[[entries]]\ntext = 'Region'\nvalue = 'grim -g'\n[[entries]]\ntext = 'Fullscreen'\nvalue = 'grim'\n"
        ));
        let items = provider.query("full", 10, false);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "Fullscreen");
        assert_eq!(items[0].provider, "screenshots");
    }

    #[test]
    fn empty_query_keeps_the_configured_order() {
        let mut provider = MenuProvider::new(menu(
            "name = 'm'\n[[entries]]\ntext = 'first'\n[[entries]]\ntext = 'second'\n[[entries]]\ntext = 'third'\n"
        ));
        let items = provider.query("", 10, false);
        let texts: Vec<&str> = items.iter().map(|i| i.text.as_str()).collect();
        assert_eq!(texts, ["first", "second", "third"]);
    }

    #[test]
    fn a_generated_menu_regenerates_when_opened() {
        let dir = std::env::temp_dir().join(format!("epochoxide-menu-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("entries.json");
        std::fs::write(&path, r#"[{"text":"first"}]"#).unwrap();
        let mut provider = MenuProvider::new(menu(&format!(
            "name = 'gen'\ncache_ms = 600000\ncommand = 'cat {}'\n", path.display()
        )));

        assert_eq!(provider.query("", 10, false)[0].text, "first");
        std::fs::write(&path, r#"[{"text":"second"}]"#).unwrap();
        // Searching within the menu reuses what the generator already returned ...
        assert_eq!(provider.query("fir", 10, false)[0].text, "first");
        // ... while opening it again regenerates behind the answer it hands back, so the new
        // state is there next time rather than the user waiting on the generator.
        std::thread::sleep(super::MIN_REFRESH);
        assert_eq!(provider.query("", 10, false)[0].text, "first");
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            if provider.query("", 10, false)[0].text == "second" { break; }
        }
        assert_eq!(provider.query("", 10, false)[0].text, "second");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn entries_are_commands_or_text() {
        let command: MenuEntry = toml::from_str("text = 'Region'\nvalue = 'grim -g'").unwrap();
        let text: MenuEntry = toml::from_str("text = 'Shrug'\ncopy = 'x'").unwrap();
        let menu = menu("name = 'm'\naction = '%VALUE%'\n");
        assert_eq!(command.action_names(&menu), ["default"]);
        assert_eq!(text.action_names(&menu), ["copy"]);
    }

    #[test]
    fn entry_falls_back_to_the_menu_icon() {
        let provider = MenuProvider::new(menu("name = 'm'\nicon = 'input-keyboard'\n[[entries]]\ntext = 'a'\n"));
        let entry: MenuEntry = toml::from_str("text = 'a'").unwrap();
        assert_eq!(provider.item(0, &entry, 1).icon, "input-keyboard");
    }

    #[test]
    fn default_action_sorts_ahead_of_the_rest() {
        let entry: MenuEntry = toml::from_str("text = 'a'\n[actions]\nzeta = 'z'\ndefault = 'd'\nalpha = 'a'").unwrap();
        assert_eq!(entry.action_names(&Menu::default())[0], "default");
    }
}
