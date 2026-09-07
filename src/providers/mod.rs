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
    fn name(&self) -> &'static str;
    fn pretty_name(&self) -> &'static str;
    fn query(&mut self, query: &str, limit: usize, exact: bool) -> Vec<Item>;
    fn activate(&mut self, identifier: &str, action: &str, query: &str, arguments: &str) -> Result<()>;
    fn menu(&mut self, _menu: &str) -> Vec<Item> { Vec::new() }
    fn events(&mut self) -> Vec<serde_json::Value> { Vec::new() }
    fn capability(&self) -> ProviderCapability {
        ProviderCapability {
            name: self.name().to_string(),
            name_pretty: self.pretty_name().to_string(),
            description: self.pretty_name().to_string(),
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
    config: Arc<Config>,
}

impl Registry {
    pub fn new(config: Config) -> Result<Self> {
        let mut providers: Vec<Box<dyn Provider>> = Vec::new();
        if enabled(&config, "apps") { providers.push(Box::new(apps::AppsProvider::new(config.clone())?)); }
        if enabled(&config, "files") { providers.push(Box::new(files::FilesProvider::new(config.clone()))); }
        if enabled(&config, "runner") { providers.push(Box::new(runner::RunnerProvider::new(config.clone()))); }
        if enabled(&config, "clipboard") { providers.push(Box::new(clipboard::ClipboardProvider::new(config.clone())?)); }
        if enabled(&config, "windows") { providers.push(Box::new(windows::WindowsProvider::new())); }
        if enabled(&config, "calc") { providers.push(Box::new(calc::CalcProvider::new(config.clone()))); }
        if enabled(&config, "menus") { providers.push(Box::new(menus::MenusProvider::new(config.clone())?)); }
        let capabilities = providers.iter().map(|p| p.capability()).collect();
        let providers = providers.into_iter().map(|p| Arc::new(Mutex::new(p))).collect();
        Ok(Self { providers, capabilities, history: Arc::new(RwLock::new(UsageHistory::load())), config: Arc::new(config) })
    }

    pub fn providers(&self) -> Vec<ProviderCapability> {
        self.capabilities.iter().map(|cap| {
            let mut cap = cap.clone();
            cap.prefixes = self.config.query_prefixes.iter().filter(|(_, provider)| provider == &&cap.name).map(|(prefix, _)| prefix.clone()).collect();
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

        let per_provider: Vec<Vec<Item>> = thread::scope(|scope| {
            let handles: Vec<_> = targets.iter().map(|&i| {
                let provider = &self.providers[i];
                let history = &self.history;
                let weight = self.config.provider_weights.get(&self.capabilities[i].name).copied().unwrap_or_default();
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
            let icon_path = icons::resolve(&item.icon, &self.config);
            item.icon_path = icon_path.as_deref().map(str::to_string);
            if let Some(ref p) = icon_path { item.thumbnail = icons::thumbnail(p, &self.config); }
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
        let Some(idx) = self.capabilities.iter().position(|c| c.name == "menus") else { return Vec::new(); };
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

        for i in targets {
            let provider = Arc::clone(&self.providers[i]);
            let history = Arc::clone(&self.history);
            let config = Arc::clone(&self.config);
            let name = self.capabilities[i].name.clone();
            let weight = self.config.provider_weights.get(&name).copied().unwrap_or_default();
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
                    let icon_path = icons::resolve(&item.icon, &config);
                    item.icon_path = icon_path.as_deref().map(str::to_string);
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
        let mut prefixes = self.config.query_prefixes.iter().collect::<Vec<_>>();
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

pub fn run_shell(command: &str) -> Result<()> {
    std::process::Command::new("sh").arg("-c").arg(command).spawn()?;
    Ok(())
}

pub fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(program).args(args).output().ok()?;
    if out.status.success() { Some(String::from_utf8_lossy(&out.stdout).trim().to_string()) } else { None }
}
