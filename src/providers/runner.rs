use super::{run_shell, Provider};
use crate::{
    config::{Config, RunnerCommand},
    fuzzy,
    types::{action_map, ActionCapability, Item, ProviderCapability},
};
use anyhow::{anyhow, Context, Result};
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
        let mut this = Self {
            config,
            commands: Vec::new(),
        };
        this.reindex();
        this
    }

    fn reindex(&mut self) {
        self.commands.clear();
        self.add_custom_commands();
        if self.config.runner_scan_path {
            self.add_path_commands();
        }
    }

    fn add_custom_commands(&mut self) {
        for command in &self.config.runner_commands {
            if command.name.is_empty() || command.command.is_empty() {
                continue;
            }
            self.commands.push(entry_from_custom(command));
        }
    }

    fn add_path_commands(&mut self) {
        let mut seen = HashSet::new();
        let path = std::env::var_os("PATH").unwrap_or_default();
        for dir in std::env::split_paths(&path) {
            let Ok(entries) = fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if !is_executable_file(&path) {
                    continue;
                }
                let Some(name) = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(str::to_string)
                else {
                    continue;
                };
                if !seen.insert(name.clone()) {
                    continue;
                }
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
    fn name(&self) -> &str {
        "runner"
    }
    fn pretty_name(&self) -> &str {
        "Runner"
    }

    fn query(&mut self, query: &str, limit: usize, exact: bool) -> Vec<Item> {
        let query_lower = query.to_lowercase();
        let mut out = Vec::new();
        for command in &self.commands {
            if let Some((score, info)) =
                fuzzy::score_lower(&query_lower, &command.search, exact, "text")
            {
                let mut item = Item::new(self.name(), &command.id, &command.name);
                item.subtext = command.command.clone();
                item.icon = command.icon.clone();
                item.actions = vec!["run".into()];
                if command.terminal {
                    item.state.push("terminal".into());
                }
                if command.custom {
                    item.state.push("custom".into());
                }
                item.score = score + if command.custom { 15_000 } else { 1_000 };
                item.fuzzyinfo = Some(info);
                out.push(item);
            }
        }
        if let Some(item) = shell_fallback_item(query) {
            out.push(item);
        }
        out.sort_by_key(|item| std::cmp::Reverse(item.score));
        out.truncate(limit);
        out
    }

    fn activate(
        &mut self,
        identifier: &str,
        action: &str,
        _query: &str,
        arguments: &str,
    ) -> Result<()> {
        match action {
            "run" => {
                let (run, terminal) = if let Some(command) = identifier.strip_prefix("shell:") {
                    (command.to_string(), true)
                } else {
                    let command = self
                        .commands
                        .iter()
                        .find(|c| c.id == identifier)
                        .context("runner command not found")?;
                    (
                        command.command.replace("%ARGS%", arguments),
                        command.terminal,
                    )
                };
                let run = if terminal {
                    terminal_command(&self.config, &run)?
                } else {
                    run
                };
                run_shell(&run)
            }
            "reindex" => {
                self.reindex();
                Ok(())
            }
            _ => anyhow::bail!("unsupported runner action: {action}"),
        }
    }

    fn capability(&self) -> ProviderCapability {
        ProviderCapability {
            name: self.name().into(),
            name_pretty: self.pretty_name().into(),
            description: "Run executables from PATH and configured commands".into(),
            icon: String::new(),
            prefixes: Vec::new(),
            actions: action_map(&[
                ("run", ActionCapability::new("Run")),
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

fn shell_fallback_item(query: &str) -> Option<Item> {
    let command = query.trim();
    if command.is_empty() {
        return None;
    }
    let mut item = Item::new(
        "runner",
        format!("shell:{command}"),
        format!("Run: {command}"),
    );
    item.subtext = "Run as shell command".into();
    item.icon = "utilities-terminal".into();
    item.actions = vec!["run".into()];
    item.state.push("shell".into());
    item.state.push("terminal".into());
    item.score = if looks_like_shell_command(command) {
        50_000
    } else {
        500
    };
    Some(item)
}

fn looks_like_shell_command(command: &str) -> bool {
    command.chars().any(|c| {
        c.is_whitespace()
            || matches!(
                c,
                '|' | '&' | ';' | '<' | '>' | '$' | '`' | '(' | ')' | '*' | '?' | '='
            )
    })
}

fn terminal_command(config: &Config, command: &str) -> Result<String> {
    let template = if config.terminal_cmd.is_empty() {
        default_terminal_cmd().ok_or_else(|| anyhow!("no terminal found; set terminal_cmd"))?
    } else {
        config.terminal_cmd.clone()
    };
    Ok(template.replace("%COMMAND%", &shell_quote(command)))
}

fn default_terminal_cmd() -> Option<String> {
    if let Ok(terminal) = std::env::var("TERMINAL") {
        if which::which(&terminal).is_ok() {
            return Some(format!("{} -e sh -lc %COMMAND%", shell_quote(&terminal)));
        }
    }
    for (program, template) in [
        (
            "x-terminal-emulator",
            "x-terminal-emulator -e sh -lc %COMMAND%",
        ),
        ("ghostty", "ghostty -e sh -lc %COMMAND%"),
        ("kitty", "kitty sh -lc %COMMAND%"),
        ("alacritty", "alacritty -e sh -lc %COMMAND%"),
        ("foot", "foot sh -lc %COMMAND%"),
        ("wezterm", "wezterm start -- sh -lc %COMMAND%"),
        ("gnome-terminal", "gnome-terminal -- sh -lc %COMMAND%"),
        ("konsole", "konsole -e sh -lc %COMMAND%"),
        ("xterm", "xterm -e sh -lc %COMMAND%"),
    ] {
        if which::which(program).is_ok() {
            return Some(template.into());
        }
    }
    None
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn entry_from_custom(command: &RunnerCommand) -> CommandEntry {
    let search = format!(
        "{} {} {}",
        command.name,
        command.command,
        command.keywords.join(" ")
    )
    .to_lowercase();
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
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    meta.is_file() && meta.permissions().mode() & 0o111 != 0
}

#[cfg(test)]
mod tests {
    use super::{entry_from_custom, shell_fallback_item, shell_quote, terminal_command};
    use crate::config::{Config, RunnerCommand};

    #[test]
    fn custom_command_search_includes_keywords() {
        let entry = entry_from_custom(&RunnerCommand {
            name: "Docs".into(),
            command: "xdg-open https://example.com".into(),
            keywords: vec!["help".into()],
            icon: None,
            terminal: false,
        });
        assert!(entry.search.contains("help"));
    }

    #[test]
    fn shell_fallback_prefers_shell_like_queries() {
        let item = shell_fallback_item("echo hi").unwrap();
        assert_eq!(item.identifier, "shell:echo hi");
        assert_eq!(item.actions, vec!["run"]);
        assert!(item.score > 15_000);
    }

    #[test]
    fn shell_fallback_does_not_beat_simple_command_names() {
        let item = shell_fallback_item("firefox").unwrap();
        assert!(item.score < 1_000);
    }

    #[test]
    fn terminal_command_quotes_shell_command() {
        let config = Config {
            terminal_cmd: "term -e sh -lc %COMMAND%".into(),
            ..Config::default()
        };
        let command = terminal_command(&config, "printf 'hi'").unwrap();
        assert_eq!(command, "term -e sh -lc 'printf '\\''hi'\\'''");
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("printf 'hi'"), "'printf '\\''hi'\\'''");
    }
}
