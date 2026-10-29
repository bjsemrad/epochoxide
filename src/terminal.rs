//! Commands run in the user's interactive shell (their aliases live in rc files `sh -c` never
//! reads), the terminal stays open after, and it runs in its own systemd scope: as a child of this
//! service a rebuild restarting the service killed it.

use anyhow::{anyhow, bail, Context, Result};
use std::path::Path;
use std::process::{Command, Stdio};

const TERMINALS: &[&str] = &["ghostty", "kitty", "alacritty", "foot", "wezterm", "xterm"];

pub fn run(command: &str, directory: Option<&Path>, terminal: &str) -> Result<String> {
    if command.trim().is_empty() {
        bail!("nothing to run");
    }
    let terminal = if terminal.trim().is_empty() {
        default_terminal()
            .ok_or_else(|| anyhow!("no terminal found; set terminal_cmd to the one to use"))?
    } else {
        terminal.to_string()
    };

    let mut script = String::new();
    if let Some(directory) = directory {
        script.push_str(&format!(
            "cd {} && ",
            quote(&directory.display().to_string())
        ));
    }
    script.push_str(command);
    script
        .push_str("\nstatus=$?\nprintf '\\n[exited %s] press enter to close ' \"$status\"\nread _");

    let shell = user_shell();
    let line = format!("{terminal} {shell} -i -c {}", quote(&script));
    let mut child = match scoped(&line).spawn() {
        Ok(child) => child,
        Err(_) => detached(Command::new("sh").arg("-c").arg(&line))
            .spawn()
            .with_context(|| format!("running {terminal}"))?,
    };
    // Nothing waits on the terminal, but something has to reap it.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(command.to_string())
}

fn scoped(line: &str) -> Command {
    let mut command = Command::new("systemd-run");
    command.args([
        "--user",
        "--scope",
        "--collect",
        "--quiet",
        "--slice=app.slice",
        "--",
        "sh",
        "-c",
        line,
    ]);
    detached(&mut command);
    command
}

fn detached(command: &mut Command) -> &mut Command {
    command
        .envs(crate::providers::session_env())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
}

fn default_terminal() -> Option<String> {
    TERMINALS
        .iter()
        .find(|candidate| which::which(candidate).is_ok())
        .map(|candidate| format!("{candidate} -e"))
}

fn user_shell() -> String {
    if let Ok(shell) = std::env::var("SHELL") {
        if !shell.trim().is_empty() {
            return shell;
        }
    }
    // A systemd user service does not always inherit SHELL.
    let user = std::env::var("USER").unwrap_or_default();
    if !user.is_empty() {
        if let Some(line) = Command::new("getent")
            .args(["passwd", &user])
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        {
            if let Some(shell) = line.rsplit(':').next() {
                if !shell.is_empty() {
                    return shell.to_string();
                }
            }
        }
    }
    "sh".to_string()
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_survives_a_quote() {
        assert_eq!(quote("plain"), "'plain'");
        assert_eq!(quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn an_empty_command_is_refused() {
        assert!(run("   ", None, "").is_err());
    }
}
