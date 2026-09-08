mod apps;
mod calc;
mod clipboard;
mod files;
mod menus;
mod runner;
mod windows;

use crate::{config::Config, history::UsageHistory, icons, types::{action_map, ActionCapability, Item, ProviderCapability}};
use anyhow::{anyhow, Result};
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::thread;

pub trait Provider: Send {
    fn name(&self) -> &str;
    fn pretty_name(&self) -> &str;
    fn query(&mut self, query: &str, limit: usize, exact: bool) -> Vec<Item>;
    fn activate(&mut self, identifier: &str, action: &str, query: &str, arguments: &str) -> Result<()>;
    fn menu(&mut self, _menu: &str) -> Vec<Item> { Vec::new() }
    fn events(&mut self) -> Vec<serde_json::Value> { Vec::new() }
    fn capability(&self) -> ProviderCapability {
        ProviderCapability {
            name: self.name().to_string(),
            name_pretty: self.pretty_name().to_string(),
            description: self.pretty_name().to_string(),
            icon: String::new(),
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

pub struct Registry {
    providers: Vec<Arc<Mutex<Box<dyn Provider>>>>,
    capabilities: Vec<ProviderCapability>,
    history: Arc<RwLock<UsageHistory>>,
    config: Mutex<Arc<Config>>,
    icons: Mutex<Arc<icons::IconResolver>>,
}

impl Registry {
    pub fn new(config: Config) -> Result<Self> {
        let mut providers: Vec<Box<dyn Provider>> = Vec::new();
        let mut wm_class_icons = std::collections::HashMap::new();
        if enabled(&config, "apps") {
            let apps = apps::AppsProvider::new(config.clone())?;
            wm_class_icons = apps.wm_class_icons();
            providers.push(Box::new(apps));
        }
        if enabled(&config, "files") { providers.push(Box::new(files::LazyFilesProvider::new(config.clone()))); }
        if enabled(&config, "runner") { providers.push(Box::new(runner::RunnerProvider::new(config.clone()))); }
        if enabled(&config, "clipboard") { providers.push(Box::new(clipboard::ClipboardProvider::new(config.clone())?)); }
        if enabled(&config, "windows") { providers.push(Box::new(windows::WindowsProvider::new(wm_class_icons))); }
        if enabled(&config, "calc") { providers.push(Box::new(calc::CalcProvider::new(config.clone()))); }
        // Menus are not one provider: each configured menu registers as its own, so it can be
        // given a query prefix and jumped straight into. A menu whose name collides with a
        // provider already registered is skipped rather than shadowing it.
        if enabled(&config, "menus") {
            for menu in menus::providers(&config)? {
                if !enabled(&config, menu.name()) { continue; }
                if providers.iter().any(|p| p.name() == menu.name()) { continue; }
                providers.push(menu);
            }
        }
        let capabilities = providers.iter().map(|p| p.capability()).collect();
        let icons = icons::IconResolver::new(&config);
        let providers = providers.into_iter().map(|p| Arc::new(Mutex::new(p))).collect();
        Ok(Self {
            providers,
            capabilities,
            history: Arc::new(RwLock::new(UsageHistory::load())),
            config: Mutex::new(Arc::new(config)),
            icons: Mutex::new(Arc::new(icons)),
        })
    }

    /// Swaps in a freshly-loaded config. Only settings Registry itself reads per-query take
    /// effect this way (provider_weights, query_prefixes, icon_theme/icon_cache_dir,
    /// thumbnail_cache_enabled) — each provider captured its own config snapshot at construction
    /// time (file_roots, runner_commands, provider_enabled, ...) and won't see the change without
    /// a full restart.
    pub fn reload_config(&self, config: Config) {
        let icons = icons::IconResolver::new(&config);
        *self.icons.lock().unwrap() = Arc::new(icons);
        *self.config.lock().unwrap() = Arc::new(config);
    }

    fn config(&self) -> Arc<Config> {
        self.config.lock().unwrap().clone()
    }

    fn icons(&self) -> Arc<icons::IconResolver> {
        self.icons.lock().unwrap().clone()
    }

    pub fn providers(&self) -> Vec<ProviderCapability> {
        let config = self.config();
        self.capabilities.iter().map(|cap| {
            let mut cap = cap.clone();
            cap.prefixes = config.query_prefixes.iter().filter(|(_, provider)| provider == &&cap.name).map(|(prefix, _)| prefix.clone()).collect();
            cap
        }).collect()
    }

    fn targets(&self, providers: &[String]) -> Vec<usize> {
        let include_all = providers.is_empty();
        (0..self.providers.len()).filter(|&i| include_all || providers.iter().any(|p| p == &self.capabilities[i].name)).collect()
    }

    pub fn query(&self, providers: &[String], query: &str, limit: usize, exact: bool) -> Vec<Item> {
        let (providers, query) = self.route_query(providers, query);
        let targets = self.targets(&providers);
        let config = self.config();
        let icons = self.icons();

        let per_provider: Vec<Vec<Item>> = thread::scope(|scope| {
            let handles: Vec<_> = targets.iter().map(|&i| {
                let provider = &self.providers[i];
                let history = &self.history;
                let weight = config.provider_weights.get(&self.capabilities[i].name).copied().unwrap_or_default();
                scope.spawn(move || {
                    let mut items = provider.lock().unwrap().query(query, limit, exact);
                    let history = history.read().unwrap();
                    for item in &mut items {
                        item.score += weight;
                        history.apply(item, query);
                    }
                    items
                })
            }).collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        let mut out: Vec<Item> = per_provider.into_iter().flatten().collect();
        out.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.text.cmp(&b.text)));
        out.truncate(limit);
        for item in &mut out {
            let icon_path = icons.resolve(&item.icon);
            item.icon_path = icon_path.clone();
            if let Some(ref p) = icon_path { item.thumbnail = icons::thumbnail(p, &config); }
        }
        out
    }

    pub fn activate(&self, provider: &str, identifier: &str, action: &str, query: &str, arguments: &str) -> Result<()> {
        let idx = self.capabilities.iter().position(|c| c.name == provider).ok_or_else(|| anyhow!("unknown provider: {provider}"))?;
        self.providers[idx].lock().unwrap().activate(identifier, action, query, arguments)?;
        self.history.write().unwrap().record(provider, identifier)?;
        Ok(())
    }

    pub fn menu(&self, name: &str) -> Vec<Item> {
        let Some(idx) = self.capabilities.iter().position(|c| c.name == name) else { return Vec::new(); };
        self.providers[idx].lock().unwrap().menu(name)
    }

    /// Spawns one thread per selected provider and streams `(provider, items)` back over the
    /// returned channel as each provider finishes, in completion order rather than registration
    /// order — a slow provider no longer holds up faster ones.
    pub fn query_batches(&self, providers: &[String], query: &str, limit: usize, exact: bool) -> mpsc::Receiver<(String, Vec<Item>)> {
        let (providers, query) = self.route_query(providers, query);
        let targets = self.targets(&providers);
        let query = query.to_string();
        let (tx, rx) = mpsc::channel();
        let config = self.config();
        let icons = self.icons();

        for i in targets {
            let provider = Arc::clone(&self.providers[i]);
            let history = Arc::clone(&self.history);
            let config = Arc::clone(&config);
            let icons = Arc::clone(&icons);
            let name = self.capabilities[i].name.clone();
            let weight = config.provider_weights.get(&name).copied().unwrap_or_default();
            let tx = tx.clone();
            let query = query.clone();
            thread::spawn(move || {
                let mut items = provider.lock().unwrap().query(&query, limit, exact);
                {
                    let history = history.read().unwrap();
                    for item in &mut items {
                        item.score += weight;
                        history.apply(item, &query);
                    }
                }
                items.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.text.cmp(&b.text)));
                items.truncate(limit);
                for item in &mut items {
                    let icon_path = icons.resolve(&item.icon);
                    item.icon_path = icon_path.clone();
                    if let Some(ref p) = icon_path { item.thumbnail = icons::thumbnail(p, &config); }
                }
                let _ = tx.send((name, items));
            });
        }
        rx
    }

    pub fn events(&self) -> Vec<serde_json::Value> {
        self.providers.iter().flat_map(|p| p.lock().unwrap().events()).collect()
    }

    fn route_query<'a>(&self, providers: &'a [String], query: &'a str) -> (Vec<String>, &'a str) {
        if !providers.is_empty() { return (providers.to_vec(), query); }
        let config = self.config();
        let mut prefixes = config.query_prefixes.iter().collect::<Vec<_>>();
        prefixes.sort_by_key(|(prefix, _)| std::cmp::Reverse(prefix.len()));
        for (prefix, provider) in prefixes {
            if let Some(stripped) = query.strip_prefix(prefix) {
                return (vec![provider.clone()], stripped.trim_start());
            }
        }
        (Vec::new(), query)
    }
}

fn enabled(config: &Config, provider: &str) -> bool {
    config.provider_enabled.get(provider).copied().unwrap_or(true)
}

pub fn copy_text(text: &str) -> Result<()> {
    use std::io::Write;
    let mut child = std::process::Command::new("wl-copy").stdin(std::process::Stdio::piped()).spawn()?;
    child.stdin.as_mut().ok_or_else(|| anyhow!("clipboard stdin unavailable"))?.write_all(text.as_bytes())?;
    reap(child);
    Ok(())
}

pub fn run_shell(command: &str) -> Result<()> {
    let mut child = std::process::Command::new("sh").arg("-c").arg(command).stderr(std::process::Stdio::piped()).spawn()?;
    let command = command.to_string();
    thread::spawn(move || {
        let mut stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() { let _ = std::io::Read::read_to_string(&mut pipe, &mut stderr); }
        let Ok(status) = child.wait() else { return };
        if status.success() { return; }
        let body = if stderr.trim().is_empty() { format!("Command failed: {command}") } else { stderr.trim().to_string() };
        let _ = std::process::Command::new("notify-send").arg("EpochOxide").arg(body).status();
    });
    Ok(())
}

pub fn reap(mut child: std::process::Child) {
    thread::spawn(move || { let _ = child.wait(); });
}

pub fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(program).args(args).output().ok()?;
    if out.status.success() { Some(String::from_utf8_lossy(&out.stdout).trim().to_string()) } else { None }
}
