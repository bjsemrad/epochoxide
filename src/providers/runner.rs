use super::{run_shell, Provider};
use crate::{config::{Config, RunnerCommand}, fuzzy, types::{action_map, ActionCapability, Item, ProviderCapability}};
use anyhow::{Context, Result};
use std::{collections::HashSet, fs, os::unix::fs::PermissionsExt, path::PathBuf};

#[derive(Debug, Clone)]
struct CommandEntry {
    id: String,
    name: String,
    command: String,
    search: String,
    icon: String,
    terminal: bool,
    custom: bool,
}

pub struct RunnerProvider {
    config: Config,
    commands: Vec<CommandEntry>,
}

impl RunnerProvider {
    pub fn new(config: Config) -> Self {
        let mut this = Self { config, commands: Vec::new() };
        this.reindex();
        this
    }

    fn reindex(&mut self) {
        self.commands.clear();
        self.add_custom_commands();
        if self.config.runner_scan_path { self.add_path_commands(); }
    }

    fn add_custom_commands(&mut self) {
        for command in &self.config.runner_commands {
            if command.name.is_empty() || command.command.is_empty() { continue; }
            self.commands.push(entry_from_custom(command));
        }
    }

    fn add_path_commands(&mut self) {
        let mut seen = HashSet::new();
        let path = std::env::var_os("PATH").unwrap_or_default();
        for dir in std::env::split_paths(&path) {
            let Ok(entries) = fs::read_dir(dir) else { continue; };
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if !is_executable_file(&path) { continue; }
                let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_string) else { continue; };
                if !seen.insert(name.clone()) { continue; }
                let command = path.display().to_string();
                self.commands.push(CommandEntry {
                    id: format!("path:{name}"),
                    search: name.to_lowercase(),
                    name,
                    command,
                    icon: "application-x-executable".into(),
                    terminal: false,
                    custom: false,
                });
            }
        }
    }
}

impl Provider for RunnerProvider {
    fn name(&self) -> &'static str { "runner" }
    fn pretty_name(&self) -> &'static str { "Runner" }

    fn query(&mut self, query: &str, limit: usize, exact: bool) -> Vec<Item> {
        let query_lower = query.to_lowercase();
        let mut out = Vec::new();
        for command in &self.commands {
            if let Some((score, info)) = fuzzy::score_lower(&query_lower, &command.search, exact, "text") {
                let mut item = Item::new(self.name(), &command.id, &command.name);
                item.subtext = command.command.clone();
                item.icon = command.icon.clone();
                item.actions = vec!["run".into()];
                if command.terminal { item.state.push("terminal".into()); }
                if command.custom { item.state.push("custom".into()); }
                item.score = score + if command.custom { 15_000 } else { 1_000 };
                item.fuzzyinfo = Some(info);
                out.push(item);
            }
        }
        out.sort_by_key(|item| std::cmp::Reverse(item.score));
        out.truncate(limit);
        out
    }

    fn activate(&mut self, identifier: &str, action: &str, _query: &str, arguments: &str) -> Result<()> {
        match action {
            "run" => {
                let command = self.commands.iter().find(|c| c.id == identifier).context("runner command not found")?;
                let mut run = command.command.replace("%ARGS%", arguments);
                if command.terminal && !self.config.terminal_cmd.is_empty() {
                    run = self.config.terminal_cmd.replace("%COMMAND%", &run);
                }
                run_shell(&run)
            }
            "reindex" => { self.reindex(); Ok(()) }
            _ => anyhow::bail!("unsupported runner action: {action}"),
        }
    }

    fn capability(&self) -> ProviderCapability {
        ProviderCapability {
            name: self.name().into(),
            name_pretty: self.pretty_name().into(),
            description: "Run executables from PATH and configured commands".into(),
            prefixes: Vec::new(),
            actions: action_map(&[
                ("run", ActionCapability::new("Run").needs_args()),
                ("reindex", ActionCapability::new("Reindex").async_action()),
            ]),
            supports_query: true,
            supports_activate: true,
            supports_streaming: true,
            supports_subscriptions: false,
            emits_events: false,
        }
    }
}

fn entry_from_custom(command: &RunnerCommand) -> CommandEntry {
    let search = format!("{} {} {}", command.name, command.command, command.keywords.join(" ")).to_lowercase();
    CommandEntry {
        id: format!("custom:{}", command.name),
        name: command.name.clone(),
        command: command.command.clone(),
        search,
        icon: command.icon.clone().unwrap_or_else(|| "system-run".into()),
        terminal: command.terminal,
        custom: true,
    }
}

fn is_executable_file(path: &PathBuf) -> bool {
    let Ok(meta) = fs::metadata(path) else { return false; };
    meta.is_file() && meta.permissions().mode() & 0o111 != 0
}

#[cfg(test)]
mod tests {
    use super::entry_from_custom;
    use crate::config::RunnerCommand;

    #[test]
    fn custom_command_search_includes_keywords() {
        let entry = entry_from_custom(&RunnerCommand { name: "Docs".into(), command: "xdg-open https://example.com".into(), keywords: vec!["help".into()], icon: None, terminal: false });
        assert!(entry.search.contains("help"));
    }
}
