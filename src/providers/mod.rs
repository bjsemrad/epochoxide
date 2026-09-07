mod apps;
mod calc;
mod clipboard;
mod files;
mod menus;
mod windows;

use crate::{config::Config, types::Item};
use anyhow::{anyhow, Result};

pub trait Provider: Send {
    fn name(&self) -> &'static str;
    fn pretty_name(&self) -> &'static str;
    fn query(&mut self, query: &str, limit: usize, exact: bool) -> Vec<Item>;
    fn activate(&mut self, identifier: &str, action: &str, query: &str, arguments: &str) -> Result<()>;
    fn menu(&mut self, _menu: &str) -> Vec<Item> { Vec::new() }
}

pub struct Registry {
    providers: Vec<Box<dyn Provider>>,
}

impl Registry {
    pub fn new(config: Config) -> Result<Self> {
        Ok(Self { providers: vec![
            Box::new(apps::AppsProvider::new(config.clone())?),
            Box::new(files::FilesProvider::new(config.clone())),
            Box::new(clipboard::ClipboardProvider::new(config.clone())?),
            Box::new(windows::WindowsProvider::new()),
            Box::new(calc::CalcProvider::new(config.clone())),
            Box::new(menus::MenusProvider::new(config)?),
        ] })
    }

    pub fn providers(&self) -> Vec<serde_json::Value> {
        self.providers.iter().map(|p| serde_json::json!({"name": p.name(), "name_pretty": p.pretty_name()})).collect()
    }

    pub fn query(&mut self, providers: &[String], query: &str, limit: usize, exact: bool) -> Vec<Item> {
        let include_all = providers.is_empty();
        let mut out = Vec::new();
        for provider in &mut self.providers {
            if include_all || providers.iter().any(|p| p == provider.name()) {
                out.extend(provider.query(query, limit, exact));
            }
        }
        out.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.text.cmp(&b.text)));
        out.truncate(limit);
        out
    }

    pub fn activate(&mut self, provider: &str, identifier: &str, action: &str, query: &str, arguments: &str) -> Result<()> {
        self.providers.iter_mut()
            .find(|p| p.name() == provider)
            .ok_or_else(|| anyhow!("unknown provider: {provider}"))?
            .activate(identifier, action, query, arguments)
    }

    pub fn menu(&mut self, name: &str) -> Vec<Item> {
        self.providers.iter_mut().find(|p| p.name() == "menus").map(|p| p.menu(name)).unwrap_or_default()
    }
}

pub fn run_shell(command: &str) -> Result<()> {
    std::process::Command::new("sh").arg("-c").arg(command).spawn()?;
    Ok(())
}

pub fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(program).args(args).output().ok()?;
    if out.status.success() { Some(String::from_utf8_lossy(&out.stdout).trim().to_string()) } else { None }
}
