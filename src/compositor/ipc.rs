//! Low-level ways to reach a compositor: sockets, CLIs, and the environment hints that locate
//! them. Nothing here knows what a window is; the backends build on it.

use anyhow::Result;
use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    time::Duration,
};

pub fn output(mut command: std::process::Command) -> Option<String> {
    let out = command.output().ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        None
    }
}

pub fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let mut command = std::process::Command::new(program);
    command.args(args);
    output(command)
}

/// Run a shell command, failing if it exits non-zero.
pub fn run(command: &str) -> Result<()> {
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("command failed: {command}")
    }
}

// --- Hyprland ---------------------------------------------------------------

pub fn hypr_output(args: &[&str]) -> Option<String> {
    let mut command = std::process::Command::new("hyprctl");
    command.args(args);
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        if let Some(sig) = hypr_signature() {
            command.env("HYPRLAND_INSTANCE_SIGNATURE", sig);
        }
    }
    output(command)
}

pub fn hypr_ipc(command: &str) -> Option<String> {
    let socket = hypr_socket_path()?;
    let mut stream = UnixStream::connect(socket).ok()?;
    let timeout = Some(Duration::from_millis(300));
    let _ = stream.set_read_timeout(timeout);
    let _ = stream.set_write_timeout(timeout);
    stream.write_all(command.as_bytes()).ok()?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut out = String::new();
    stream.read_to_string(&mut out).ok()?;
    if out.trim().is_empty() {
        None
    } else {
        Some(out.trim().to_string())
    }
}

/// True only when Hyprland answered `ok`; every failure path (Lua syntax error, unknown window,
/// unreachable compositor) answers with something else.
pub fn hypr_send(args: &[&str]) -> bool {
    let reply = hypr_ipc(&args.join(" ")).or_else(|| hypr_output(args));
    reply.is_some_and(|reply| reply.trim() == "ok")
}

/// Focus/close a window by address, whichever config flavour Hyprland was started with.
///
/// A Hyprland started from a Lua config runs `dispatch` through Lua, so the classic
/// `dispatch focuswindow address:0x…` arrives as `hl.dispatch(focuswindow address:0x…)` and dies
/// on a syntax error — one that still comes back as a normal (non-empty) reply, so it has to be
/// told apart from the plain `ok` a real dispatch answers with. The classic form goes first
/// because it is the only one older Hyprlands understand; the Lua fallback looks the window
/// object up by address and dispatches against it.
pub fn hypr_window_dispatch(dispatcher: &str, lua_dispatch: &str, address: &str) -> Result<()> {
    if hypr_send(&["dispatch", dispatcher, &format!("address:{address}")]) {
        return Ok(());
    }
    let lua = format!(
        "for _, w in ipairs(hl.get_windows()) do if w.address == \"{address}\" then hl.dispatch({lua_dispatch}) return \"ok\" end end return \"no window\""
    );
    if hypr_send(&["repl", &lua]) {
        return Ok(());
    }
    anyhow::bail!("hyprland rejected {dispatcher} for {address}")
}

/// Switch to a workspace by id, or by `name:` for a named one. A workspace that does not exist
/// yet is created, matching what the classic dispatcher does.
///
/// Same Lua-config problem as [`hypr_window_dispatch`], with one extra trap: the `hl.dsp.*`
/// helpers *build* a dispatcher object rather than running one, so the result has to be handed to
/// `hl.dispatch()`. Passing the dispatcher string straight to `hl.dispatch("workspace 3")` is
/// accepted and answers `ok` while silently doing nothing, so it cannot be used as a shortcut.
pub fn hypr_workspace_dispatch(handle: &str) -> Result<()> {
    // Hyprland reads a bare number as a workspace id and anything else as a name.
    let numeric = handle.parse::<i64>().ok();
    let classic = match numeric {
        Some(id) => format!("workspace {id}"),
        None => format!("workspace name:{handle}"),
    };
    if hypr_send(&["dispatch", &classic]) {
        return Ok(());
    }
    let lua = match numeric {
        Some(id) => format!("hl.dispatch(hl.dsp.focus({{ workspace = {id} }})) return \"ok\""),
        // A named workspace has to be looked up as an object; there is no name form of the
        // dispatcher argument.
        None => format!(
            "for _, w in ipairs(hl.get_workspaces()) do if tostring(w.name) == {name} then hl.dispatch(hl.dsp.focus({{ workspace = w }})) return \"ok\" end end return \"no workspace\"",
            name = lua_string(handle)
        ),
    };
    if hypr_send(&["repl", &lua]) {
        return Ok(());
    }
    anyhow::bail!("hyprland rejected workspace \"{handle}\"")
}

/// Quote a value as a Lua string literal.
fn lua_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn hypr_socket_path() -> Option<PathBuf> {
    let sig = hypr_signature()?;
    let socket = std::path::Path::new(&runtime_dir())
        .join("hypr")
        .join(sig)
        .join(".socket.sock");
    socket.exists().then_some(socket)
}

fn hypr_signature() -> Option<String> {
    if let Ok(sig) = std::env::var("HYPRLAND_INSTANCE_SIGNATURE") {
        if !sig.is_empty() {
            return Some(sig);
        }
    }
    let rt = runtime_dir();
    let dir = std::path::Path::new(&rt).join("hypr");
    let mut latest: Option<(std::time::SystemTime, String)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if !path.join(".socket.sock").exists() {
            continue;
        }
        let sig = entry.file_name().to_string_lossy().to_string();
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if latest.as_ref().is_none_or(|(time, _)| modified > *time) {
            latest = Some((modified, sig));
        }
    }
    latest.map(|(_, sig)| sig)
}

// --- niri -------------------------------------------------------------------

pub fn niri_output(args: &[&str]) -> Option<String> {
    let mut command = std::process::Command::new("niri");
    command.args(args);
    if std::env::var_os("NIRI_SOCKET").is_none() {
        if let Some(socket) = niri_socket() {
            command.env("NIRI_SOCKET", socket);
        }
    }
    output(command)
}

/// A `niri` invocation carrying the socket, for use inside a shell command.
pub fn niri_shell() -> String {
    niri_socket()
        .map(|socket| format!("NIRI_SOCKET={} niri", shell_quote(&socket)))
        .unwrap_or_else(|| "niri".into())
}

fn niri_socket() -> Option<String> {
    if let Ok(socket) = std::env::var("NIRI_SOCKET") {
        if !socket.is_empty() {
            return Some(socket);
        }
    }
    let rt = runtime_dir();
    [
        format!("{rt}/niri.sock"),
        format!("{rt}/niri-ipc/niri.sock"),
    ]
    .into_iter()
    .find(|p| std::path::Path::new(p).exists())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn runtime_dir() -> String {
    std::env::var("XDG_RUNTIME_DIR")
        .or_else(|_| std::env::var("UID").map(|uid| format!("/run/user/{uid}")))
        .unwrap_or_else(|_| "/run/user/1000".into())
}

#[cfg(test)]
mod tests {
    use super::lua_string;

    #[test]
    fn a_workspace_name_is_quoted_as_a_lua_string() {
        assert_eq!(lua_string("web"), "\"web\"");
    }

    #[test]
    fn quotes_and_backslashes_cannot_break_out_of_the_literal() {
        // The name reaches Lua inside a generated program, so an unescaped quote would be a
        // syntax error at best and an injection at worst.
        assert_eq!(lua_string("a\"b"), "\"a\\\"b\"");
        assert_eq!(lua_string("a\\b"), "\"a\\\\b\"");
    }
}
