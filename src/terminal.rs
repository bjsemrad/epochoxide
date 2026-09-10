//! Running a command where the user can watch it.
//!
//! Some actions cannot be done quietly in the background: a rebuild takes minutes and prints the
//! only record of what it did, a firmware update asks for a password and then asks the user to
//! reboot. Those belong in a terminal the user can see, read, and interrupt.
//!
//! Two details make the difference between this working and looking broken:
//!
//! 1. **The user's own shell, interactively.** A command worth putting on a button is usually one
//!    the user already runs by hand -- `nixswitch`, `rebuild-thor` -- and those are shell aliases,
//!    which live in an rc file that `sh -c` never reads.
//! 2. **The terminal stays open.** A window that closes on the last line takes the output with it,
//!    which is the one thing the user opened a terminal for.

use anyhow::{anyhow, bail, Context, Result};
use std::path::Path;
use std::process::{Command, Stdio};

/// Terminals to try when none is configured, in the order they are looked for. Every one of these
/// takes the command to run after `-e`.
const TERMINALS: &[&str] = &["ghostty", "kitty", "alacritty", "foot", "wezterm", "xterm"];

/// Run `command` in a terminal, optionally from `directory`.
///
/// Returns the command as it was given, so a caller can report what it started.
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
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(format!("{terminal} {shell} -i -c {}", quote(&script)))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("running {terminal}"))?;
    // Nothing waits on the terminal, but something has to reap it.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(command.to_string())
}

fn default_terminal() -> Option<String> {
    TERMINALS
        .iter()
        .find(|candidate| which::which(candidate).is_ok())
        .map(|candidate| format!("{candidate} -e"))
}

/// The user's login shell, which is where their aliases are defined.
fn user_shell() -> String {
    if let Ok(shell) = std::env::var("SHELL") {
        if !shell.trim().is_empty() {
            return shell;
        }
    }
    // A systemd user service does not always inherit SHELL, so passwd is the fallback.
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
