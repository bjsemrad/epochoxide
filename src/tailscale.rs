//! Normalized Tailscale state.
//!
//! `tailscale status --json` is a large, version-dependent payload; only the fields the shell and
//! CLI actually render are lifted out of it, so a Tailscale upgrade that adds or moves keys does
//! not ripple into the UI.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Machine {
    pub id: String,
    /// Short host name, e.g. `thor`.
    pub name: String,
    /// Fully-qualified MagicDNS name, trailing dot stripped.
    pub dns_name: String,
    pub os: String,
    pub online: bool,
    pub ips: Vec<String>,
    /// True when this machine is the exit node currently in use.
    pub exit_node: bool,
    /// True when this machine offers itself as an exit node.
    pub exit_node_option: bool,
    /// True for this device itself rather than a peer.
    pub is_self: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Status {
    /// True only when the backend is fully up.
    pub running: bool,
    /// Raw backend state, e.g. `Running`, `Stopped`, `NeedsLogin`.
    pub state: String,
    pub version: String,
    pub tailnet: String,
    pub magic_dns_suffix: String,
    pub self_machine: Option<Machine>,
    /// Name of the exit node in use, if any.
    pub exit_node: Option<String>,
    /// Health warnings Tailscale is reporting, empty when healthy.
    pub health: Vec<String>,
}

/// Whether the Tailscale CLI is installed at all. Used to decide the API group's availability
/// rather than failing every call.
pub fn available() -> bool {
    which("tailscale").is_some()
}

fn which(binary: &str) -> Option<std::path::PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(binary))
        .find(|candidate| candidate.is_file())
}

/// `tailscale status --json`, parsed.
fn status_json() -> Result<Value> {
    if !available() {
        bail!("tailscale is not installed");
    }
    let output = Command::new("tailscale")
        .args(["status", "--json"])
        .output()
        .context("running tailscale status")?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!(if err.is_empty() {
            "tailscale status failed".to_string()
        } else {
            err
        });
    }
    serde_json::from_slice(&output.stdout).context("parsing tailscale status")
}

fn string(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn machine(node: &Value, is_self: bool, backend_running: bool) -> Machine {
    // Peers report their addresses under TailscaleIPs on current releases and Addrs on older
    // ones; take whichever is present so both keep working.
    let ips = ["TailscaleIPs", "Addrs"]
        .iter()
        .find_map(|key| node.get(*key).and_then(Value::as_array))
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Machine {
        id: string(node, "ID"),
        name: string(node, "HostName"),
        dns_name: string(node, "DNSName").trim_end_matches('.').to_string(),
        os: string(node, "OS"),
        // A peer's Online is control-plane visibility, which is what a UI wants. For this
        // device it is not: Tailscale reports Self.Online false whenever it cannot reach the
        // coordination server, even with the tailnet up and BackendState Running, so showing the
        // local machine as offline would contradict the status right next to it.
        online: if is_self {
            backend_running
        } else {
            node.get("Online").and_then(Value::as_bool).unwrap_or(false)
        },
        ips,
        exit_node: node
            .get("ExitNode")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        exit_node_option: node
            .get("ExitNodeOption")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        is_self,
    }
}

pub fn status() -> Result<Status> {
    let raw = status_json()?;
    let state = string(&raw, "BackendState");
    let running = state == "Running";
    let self_machine = raw.get("Self").map(|node| machine(node, true, running));
    let exit_node = raw
        .get("Peer")
        .and_then(Value::as_object)
        .and_then(|peers| {
            peers
                .values()
                .find(|peer| peer.get("ExitNode").and_then(Value::as_bool) == Some(true))
                .map(|peer| string(peer, "HostName"))
        });
    let health = raw
        .get("Health")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Ok(Status {
        running,
        state,
        version: string(&raw, "Version"),
        tailnet: raw
            .get("CurrentTailnet")
            .map(|tailnet| string(tailnet, "Name"))
            .unwrap_or_default(),
        magic_dns_suffix: string(&raw, "MagicDNSSuffix"),
        self_machine,
        exit_node,
        health,
    })
}

/// Every machine in the tailnet, this device first, then peers by name.
pub fn machines() -> Result<Vec<Machine>> {
    let raw = status_json()?;
    let running = string(&raw, "BackendState") == "Running";
    let mut out = Vec::new();
    if let Some(node) = raw.get("Self") {
        out.push(machine(node, true, running));
    }
    if let Some(peers) = raw.get("Peer").and_then(Value::as_object) {
        let mut peers: Vec<Machine> = peers
            .values()
            .map(|peer| machine(peer, false, running))
            .collect();
        peers.sort_by(|a, b| a.name.cmp(&b.name));
        out.extend(peers);
    }
    Ok(out)
}

/// Send files to a peer with Taildrop.
///
/// The peer is named the way `machines` reports it -- short name or MagicDNS name -- and is
/// resolved here so callers never have to build a `tailscale file cp` target themselves.
pub fn send(peer: &str, files: &[String]) -> Result<()> {
    if files.is_empty() {
        bail!("no files given");
    }
    let known = machines()?;
    let target = known
        .iter()
        .find(|machine| {
            !machine.is_self
                && (machine.name.eq_ignore_ascii_case(peer)
                    || machine.dns_name.eq_ignore_ascii_case(peer))
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no tailnet peer named \"{peer}\" (known: {})",
                known
                    .iter()
                    .filter(|machine| !machine.is_self)
                    .map(|machine| machine.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
    for file in files {
        if !std::path::Path::new(file).exists() {
            bail!("no such file: {file}");
        }
    }
    let mut command = Command::new("tailscale");
    command.arg("file").arg("cp");
    command.args(files);
    command.arg(format!("{}:", target.dns_name));
    let output = command.output().context("running tailscale file cp")?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!(if err.is_empty() {
            format!("sending to {} failed", target.name)
        } else {
            err
        });
    }
    Ok(())
}
