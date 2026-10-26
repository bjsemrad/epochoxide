use anyhow::{Context, Result};
use std::{
    io::{BufRead, BufReader, Read, Write},
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

pub fn hyprctl() -> std::process::Command {
    let mut command = std::process::Command::new("hyprctl");
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        if let Some(sig) = hypr_signature() {
            command.env("HYPRLAND_INSTANCE_SIGNATURE", sig);
        }
    }
    command
}

pub fn hypr_output(args: &[&str]) -> Option<String> {
    let mut command = hyprctl();
    command.args(args);
    output(command)
}

/// The socket file alone proves nothing: a dead session's socket stays in the runtime directory.
pub fn hypr_running() -> bool {
    hypr_ipc("version").is_some()
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

pub fn hypr_send(args: &[&str]) -> bool {
    let reply = hypr_ipc(&args.join(" ")).or_else(|| hypr_output(args));
    reply.is_some_and(|reply| reply.trim() == "ok")
}

/// Under a Lua config, the classic dispatch arrives as a Lua syntax error that still returns a
/// non-`ok` reply; classic first (older Hyprlands), then a Lua lookup by address.
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

fn lua_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

pub fn hypr_watch(on_event: &mut dyn FnMut(&str) -> anyhow::Result<()>) -> Result<()> {
    let socket = hypr_event_socket_path()
        .ok_or_else(|| anyhow::anyhow!("hyprland event socket not found"))?;
    let stream = UnixStream::connect(socket).context("connecting to the hyprland event socket")?;
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { break };
        on_event(&line)?;
    }
    Ok(())
}

fn hypr_event_socket_path() -> Option<PathBuf> {
    let sig = hypr_signature()?;
    let socket = std::path::Path::new(&runtime_dir())
        .join("hypr")
        .join(sig)
        .join(".socket2.sock");
    socket.exists().then_some(socket)
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

/// One request over niri's socket. `niri msg` can only name a workspace by index or name (and a
/// numeric name parses as an index), so anything that needs a workspace's id goes through here.
pub fn niri_request(request: &serde_json::Value) -> Result<()> {
    let socket = niri_socket().context("no niri socket")?;
    let mut stream = UnixStream::connect(&socket).context("connecting to niri")?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    writeln!(stream, "{request}")?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    let reply: serde_json::Value = serde_json::from_str(&reply).context("niri's reply")?;
    if reply.get("Ok").is_some() {
        Ok(())
    } else {
        anyhow::bail!("niri refused the request: {reply}")
    }
}

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

pub fn niri_watch(on_event: &mut dyn FnMut() -> anyhow::Result<()>) -> Result<()> {
    let socket = niri_socket().ok_or_else(|| anyhow::anyhow!("niri socket not found"))?;
    let mut stream = UnixStream::connect(socket).context("connecting to the niri socket")?;
    writeln!(stream, "\"EventStream\"")?;
    stream.flush()?;
    for line in BufReader::new(stream).lines() {
        let Ok(_line) = line else { break };
        on_event()?;
    }
    Ok(())
}

pub fn mango_request(command: &str) -> Option<String> {
    let mut stream = UnixStream::connect(mango_socket()?).ok()?;
    let timeout = Some(Duration::from_millis(300));
    let _ = stream.set_read_timeout(timeout);
    let _ = stream.set_write_timeout(timeout);
    writeln!(stream, "{command}").ok()?;
    let mut out = String::new();
    stream.read_to_string(&mut out).ok()?;
    let out = out.trim();
    (!out.is_empty()).then(|| out.to_string())
}

pub fn mango_watch(on_event: &mut dyn FnMut() -> anyhow::Result<()>) -> Result<()> {
    let socket = mango_socket().ok_or_else(|| anyhow::anyhow!("mango socket not found"))?;
    let mut stream = UnixStream::connect(socket).context("connecting to the mango socket")?;
    writeln!(stream, "watch all-clients")?;
    stream.flush()?;
    for line in BufReader::new(stream).lines() {
        let Ok(_line) = line else { break };
        on_event()?;
    }
    Ok(())
}

/// `MANGO_INSTANCE_SIGNATURE` is the socket path; without it, the newest `mango-<pid>.sock` is
/// used.
fn mango_socket() -> Option<PathBuf> {
    if let Ok(socket) = std::env::var("MANGO_INSTANCE_SIGNATURE") {
        if !socket.is_empty() {
            return Some(PathBuf::from(socket));
        }
    }
    let mut latest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(runtime_dir()).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !(name.starts_with("mango-") && name.ends_with(".sock")) {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if latest.as_ref().is_none_or(|(time, _)| modified > *time) {
            latest = Some((modified, entry.path()));
        }
    }
    latest.map(|(_, path)| path)
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
        assert_eq!(lua_string("a\"b"), "\"a\\\"b\"");
        assert_eq!(lua_string("a\\b"), "\"a\\\\b\"");
    }
}
