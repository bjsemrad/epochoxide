mod cert;
mod http;
pub mod server;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 167);
const PORT: u16 = 53317;
const PROTOCOL_VERSION: &str = "2.0";

const DISCOVERY_WINDOW: Duration = Duration::from_millis(1200);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Device {
    pub alias: String,
    pub fingerprint: String,
    pub device_model: String,
    pub device_type: String,
    pub ip: String,
    pub port: u16,
    pub protocol: String,
    pub download: bool,
}

impl Device {
    fn endpoint(&self) -> http::Endpoint {
        http::Endpoint {
            host: self.ip.clone(),
            port: self.port,
            https: self.protocol.eq_ignore_ascii_case("https"),
            fingerprint: self.fingerprint.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Sent {
    pub device: String,
    pub files: Vec<String>,
}

fn own_identity() -> Value {
    let receiving = receiver();
    json!({
        "alias": receiving.as_ref().map(|r| r.alias()).unwrap_or_else(alias),
        "version": PROTOCOL_VERSION,
        "deviceModel": "Linux",
        "deviceType": "desktop",
        "fingerprint": own_fingerprint(),
        "port": receiving.as_ref().map(|r| r.port()).unwrap_or(PORT),
        "protocol": if receiving.is_some() { "https" } else { "http" },
        "download": false,
    })
}

fn alias() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "Epoch".to_string())
}

fn own_fingerprint() -> String {
    // The announced fingerprint must hash the certificate we actually present: peers pin it both
    // ways.
    if let Ok(identity) = cert::shared() {
        return identity.fingerprint.clone();
    }
    let seed = format!("epochoxide:{}", alias());
    let digest = ring::digest::digest(&ring::digest::SHA256, seed.as_bytes());
    digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// SO_REUSEADDR/SO_REUSEPORT: multicast is received on the group's own port, which the LocalSend
/// app also holds.
pub(crate) fn multicast_socket() -> Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .context("creating the discovery socket")?;
    socket.set_reuse_address(true)?;
    #[cfg(target_os = "linux")]
    socket.set_reuse_port(true)?;
    socket
        .bind(&SocketAddr::from(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, PORT)).into())
        .with_context(|| format!("binding UDP {PORT} for LocalSend discovery"))?;
    socket
        .join_multicast_v4(&GROUP, &Ipv4Addr::UNSPECIFIED)
        .context("joining the LocalSend multicast group")?;
    socket.set_read_timeout(Some(Duration::from_millis(200)))?;
    Ok(socket.into())
}

static RECEIVER: Mutex<Option<Arc<server::Receiver>>> = Mutex::new(None);

static SETTINGS: OnceLock<(String, PathBuf)> = OnceLock::new();

pub fn receiver() -> Option<Arc<server::Receiver>> {
    RECEIVER.lock().ok()?.clone()
}

pub fn configure(config: &crate::config::Config) {
    let name = if config.localsend_alias.is_empty() {
        alias()
    } else {
        config.localsend_alias.clone()
    };
    let directory = PathBuf::from(shellexpand::tilde(&config.localsend_download_dir).to_string());
    let _ = SETTINGS.set((name, directory));
}

pub fn start_receiver() -> Result<()> {
    let mut slot = RECEIVER
        .lock()
        .map_err(|_| anyhow!("the receiver lock is poisoned"))?;
    if slot.is_some() {
        return Ok(());
    }
    let (name, directory) = SETTINGS.get().cloned().unwrap_or_else(|| {
        (
            alias(),
            PathBuf::from(shellexpand::tilde("~/Downloads").to_string()),
        )
    });
    *slot = Some(server::start(name, directory, PORT)?);
    Ok(())
}

pub fn stop_receiver() -> Result<bool> {
    let mut slot = RECEIVER
        .lock()
        .map_err(|_| anyhow!("the receiver lock is poisoned"))?;
    match slot.take() {
        Some(receiver) => {
            receiver.stop();
            Ok(true)
        }
        None => Ok(false),
    }
}

pub fn available() -> Result<()> {
    multicast_socket().map(|_| ())
}

pub fn devices() -> Result<Vec<Device>> {
    let socket = multicast_socket()?;
    let mut announcement = own_identity();
    announcement["announce"] = json!(true);
    let payload = serde_json::to_vec(&announcement)?;
    socket
        .send_to(&payload, SocketAddrV4::new(GROUP, PORT))
        .context("announcing to the LocalSend multicast group")?;

    let own = own_fingerprint();
    let mut found: Vec<Device> = Vec::new();
    let deadline = Instant::now() + DISCOVERY_WINDOW;
    let mut buffer = [0u8; 8192];
    while Instant::now() < deadline {
        let (read, from) = match socket.recv_from(&mut buffer) {
            Ok(result) => result,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(err) if err.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(err) => return Err(err).context("reading LocalSend announcements"),
        };
        let Some(device) = parse_announcement(&buffer[..read], from) else {
            continue;
        };
        if device.fingerprint == own {
            continue;
        }
        if found
            .iter()
            .any(|seen| seen.fingerprint == device.fingerprint)
        {
            continue;
        }
        found.push(device);
    }
    // Merge what the receiver heard: with two sockets on the port the kernel delivers a unicast
    // reply to only one, and some devices announce over HTTP.
    if let Some(receiver) = receiver() {
        for device in receiver.seen() {
            if !found
                .iter()
                .any(|seen| seen.fingerprint == device.fingerprint)
            {
                found.push(device);
            }
        }
    }
    found.sort_by_key(|device| device.alias.to_lowercase());
    Ok(found)
}

fn parse_announcement(payload: &[u8], from: SocketAddr) -> Option<Device> {
    let value: Value = serde_json::from_slice(payload).ok()?;
    device_from_announcement(&value, from.ip(), PORT)
}

/// The address comes from the connection, not the payload, so a device cannot claim to be
/// elsewhere.
pub(crate) fn device_from_announcement(
    value: &Value,
    address: std::net::IpAddr,
    default_port: u16,
) -> Option<Device> {
    let fingerprint = value.get("fingerprint")?.as_str()?.to_string();
    if fingerprint.is_empty() {
        return None;
    }
    let string = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    Some(Device {
        alias: {
            let alias = string("alias");
            if alias.is_empty() {
                address.to_string()
            } else {
                alias
            }
        },
        fingerprint,
        device_model: string("deviceModel"),
        device_type: string("deviceType"),
        ip: address.to_string(),
        port: value
            .get("port")
            .and_then(Value::as_u64)
            .and_then(|port| u16::try_from(port).ok())
            .unwrap_or(default_port),
        protocol: {
            let protocol = string("protocol");
            if protocol.is_empty() {
                "https".to_string()
            } else {
                protocol
            }
        },
        download: value
            .get("download")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn find(devices: Vec<Device>, wanted: &str) -> Result<Device> {
    let needle = wanted.trim().to_lowercase();
    let known: Vec<String> = devices.iter().map(|d| d.alias.clone()).collect();
    devices
        .into_iter()
        .find(|device| {
            device.alias.to_lowercase() == needle || device.fingerprint.to_lowercase() == needle
        })
        .ok_or_else(|| {
            if known.is_empty() {
                anyhow!("no LocalSend devices answered; is the app open on the other device?")
            } else {
                anyhow!(
                    "no LocalSend device named \"{wanted}\" (found: {})",
                    known.join(", ")
                )
            }
        })
}

pub fn send(device: &str, files: &[String]) -> Result<Sent> {
    if files.is_empty() {
        bail!("no files given");
    }
    let mut prepared = Vec::new();
    for path in files {
        let file = Path::new(path);
        let metadata =
            std::fs::metadata(file).with_context(|| format!("reading {}", file.display()))?;
        if !metadata.is_file() {
            bail!("{} is not a file", file.display());
        }
        let name = file
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .ok_or_else(|| anyhow!("{} has no file name", file.display()))?;
        prepared.push((path.clone(), name, metadata.len()));
    }

    let target = find(devices()?, device)?;
    let endpoint = target.endpoint();

    let mut manifest = serde_json::Map::new();
    for (index, (_, name, size)) in prepared.iter().enumerate() {
        let id = index.to_string();
        manifest.insert(
            id.clone(),
            json!({
                "id": id,
                "fileName": name,
                "size": size,
                "fileType": "application/octet-stream",
            }),
        );
    }

    let response = endpoint
        .post(
            "/api/localsend/v2/prepare-upload",
            "application/json",
            &serde_json::to_vec(&json!({ "info": own_identity(), "files": manifest }))?,
        )
        .with_context(|| format!("asking {} to accept the transfer", target.alias))?;
    let prepared_response: Value = serde_json::from_slice(&response)
        .context("the device's answer to prepare-upload was not JSON")?;
    let session = prepared_response
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("{} did not open a transfer session", target.alias))?;
    let tokens = prepared_response
        .get("files")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("{} returned no upload tokens", target.alias))?;

    let mut sent = Vec::new();
    for (index, (path, name, _)) in prepared.iter().enumerate() {
        let id = index.to_string();
        let Some(token) = tokens.get(&id).and_then(Value::as_str) else {
            continue;
        };
        let body = std::fs::read(path).with_context(|| format!("reading {path}"))?;
        endpoint
            .post(
                &format!("/api/localsend/v2/upload?sessionId={session}&fileId={id}&token={token}"),
                "application/octet-stream",
                &body,
            )
            .with_context(|| format!("sending {name} to {}", target.alias))?;
        sent.push(name.clone());
    }

    if sent.is_empty() {
        bail!("{} declined every file", target.alias);
    }
    Ok(Sent {
        device: target.alias,
        files: sent,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn from() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42)), 53317)
    }

    #[test]
    fn an_announcement_becomes_a_device() {
        let payload = br#"{"alias":"Phone","fingerprint":"ab12","deviceModel":"Pixel",
            "deviceType":"mobile","port":53318,"protocol":"https","download":true}"#;
        let device = parse_announcement(payload, from()).expect("parsed");
        assert_eq!(device.alias, "Phone");
        assert_eq!(device.port, 53318);
        assert_eq!(device.protocol, "https");
        assert!(device.download);
        // The IP comes from the datagram's source, never from the payload.
        assert_eq!(device.ip, "192.168.1.42");
    }

    #[test]
    fn a_device_without_a_fingerprint_is_ignored() {
        assert!(parse_announcement(br#"{"alias":"Phone"}"#, from()).is_none());
        assert!(parse_announcement(br#"{"alias":"P","fingerprint":""}"#, from()).is_none());
    }

    #[test]
    fn a_non_announcement_datagram_is_ignored() {
        assert!(parse_announcement(b"not json at all", from()).is_none());
    }

    #[test]
    fn defaults_fill_in_for_a_terse_announcement() {
        let device = parse_announcement(br#"{"fingerprint":"ab12"}"#, from()).expect("parsed");
        assert_eq!(device.port, PORT);
        assert_eq!(device.protocol, "https");
        assert_eq!(device.alias, "192.168.1.42");
    }

    #[test]
    fn a_device_can_be_found_by_alias_or_fingerprint_in_any_case() {
        let device =
            parse_announcement(br#"{"alias":"My Phone","fingerprint":"AB12"}"#, from()).unwrap();
        assert!(find(vec![device.clone()], "my phone").is_ok());
        assert!(find(vec![device.clone()], "ab12").is_ok());
        assert!(find(vec![device], "laptop").is_err());
    }

    #[test]
    fn an_unknown_device_names_the_ones_that_answered() {
        let device =
            parse_announcement(br#"{"alias":"Phone","fingerprint":"ab12"}"#, from()).unwrap();
        let err = find(vec![device], "laptop").unwrap_err().to_string();
        assert!(err.contains("Phone"), "{err}");
    }

    #[test]
    fn our_own_fingerprint_is_stable_between_calls() {
        assert_eq!(own_fingerprint(), own_fingerprint());
        assert_eq!(own_fingerprint().len(), 64);
    }
}
