//! The receiving half of LocalSend: a small HTTPS server plus a multicast responder.
//!
//! Only the endpoints a sender actually uses are implemented. A transfer arrives in two steps --
//! `prepare-upload` asks permission and `upload` delivers the bytes -- and permission is the part
//! that matters: `prepare-upload` blocks until someone accepts through the shell, or until the
//! request expires. Nothing is written to disk before that.

use super::cert;
use anyhow::{anyhow, Context, Result};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long a sender is left waiting for someone to accept before the request is dropped.
const CONSENT_TIMEOUT: Duration = Duration::from_secs(120);
/// Refuse absurd uploads outright rather than filling a disk.
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IncomingFile {
    pub id: String,
    pub name: String,
    pub size: u64,
    /// Set once the file has been written.
    pub saved_to: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IncomingTransfer {
    pub session: String,
    /// The sending device's alias, as it described itself.
    pub device: String,
    pub fingerprint: String,
    pub files: Vec<IncomingFile>,
    /// Seconds since the epoch, so a UI can age the request out.
    pub requested_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    Pending,
    Accepted,
    Declined,
}

struct Session {
    transfer: IncomingTransfer,
    decision: Decision,
    /// fileId -> upload token handed back on acceptance.
    tokens: HashMap<String, String>,
}

struct Inner {
    sessions: HashMap<String, Session>,
    download_dir: PathBuf,
    port: u16,
    alias: String,
    fingerprint: String,
    /// Files written since the daemon started, newest first.
    received: Vec<IncomingFile>,
}

/// Shared receiver state. The condvar is how an accepted or declined request wakes the HTTP
/// handler that is holding the sender's connection open.
pub struct Receiver {
    inner: Mutex<Inner>,
    decided: Condvar,
}

impl Receiver {
    pub fn port(&self) -> u16 {
        self.inner.lock().unwrap().port
    }

    pub fn fingerprint(&self) -> String {
        self.inner.lock().unwrap().fingerprint.clone()
    }

    pub fn alias(&self) -> String {
        self.inner.lock().unwrap().alias.clone()
    }

    pub fn download_dir(&self) -> PathBuf {
        self.inner.lock().unwrap().download_dir.clone()
    }

    /// Transfers waiting on a decision.
    pub fn pending(&self) -> Vec<IncomingTransfer> {
        let inner = self.inner.lock().unwrap();
        inner
            .sessions
            .values()
            .filter(|session| session.decision == Decision::Pending)
            .map(|session| session.transfer.clone())
            .collect()
    }

    pub fn received(&self) -> Vec<IncomingFile> {
        self.inner.lock().unwrap().received.clone()
    }

    fn decide(&self, session: &str, decision: Decision) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let entry = inner
            .sessions
            .get_mut(session)
            .ok_or_else(|| anyhow!("no transfer waiting with session \"{session}\""))?;
        if entry.decision != Decision::Pending {
            return Err(anyhow!("that transfer has already been answered"));
        }
        entry.decision = decision;
        drop(inner);
        self.decided.notify_all();
        Ok(())
    }

    pub fn accept(&self, session: &str) -> Result<()> {
        self.decide(session, Decision::Accepted)
    }

    pub fn decline(&self, session: &str) -> Result<()> {
        self.decide(session, Decision::Declined)
    }

    /// This device, in the shape LocalSend announcements and `/info` use.
    fn info(&self) -> Value {
        let inner = self.inner.lock().unwrap();
        json!({
            "alias": inner.alias,
            "version": "2.0",
            "deviceModel": "Linux",
            "deviceType": "desktop",
            "fingerprint": inner.fingerprint,
            "port": inner.port,
            "protocol": "https",
            "download": false,
        })
    }
}

/// Start the receiver, returning the shared state the API reads.
///
/// Binds `preferred` when it can and the next free port otherwise: the announcement carries
/// whichever port was taken, and senders honour it, so a LocalSend app already holding the
/// standard port is a fallback rather than a failure.
pub fn start(alias: String, download_dir: PathBuf, preferred: u16) -> Result<Arc<Receiver>> {
    let identity = cert::load_or_create()?;
    let (listener, port) = bind(preferred)?;

    let receiver = Arc::new(Receiver {
        inner: Mutex::new(Inner {
            sessions: HashMap::new(),
            download_dir,
            port,
            alias,
            fingerprint: identity.fingerprint.clone(),
            received: Vec::new(),
        }),
        decided: Condvar::new(),
    });

    let config = Arc::new(tls_config(&identity)?);
    let serving = Arc::clone(&receiver);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let config = Arc::clone(&config);
            let receiver = Arc::clone(&serving);
            // One thread per transfer: a handler blocks for as long as consent takes, so it must
            // not hold up anything else.
            std::thread::spawn(move || {
                if let Err(err) = handle(stream, config, receiver) {
                    eprintln!("localsend: {err:#}");
                }
            });
        }
    });

    let responding = Arc::clone(&receiver);
    std::thread::spawn(move || {
        if let Err(err) = respond_to_announcements(responding) {
            eprintln!("localsend: discovery responder stopped: {err:#}");
        }
    });

    Ok(receiver)
}

fn bind(preferred: u16) -> Result<(TcpListener, u16)> {
    match TcpListener::bind(("0.0.0.0", preferred)) {
        Ok(listener) => Ok((listener, preferred)),
        Err(_) => {
            // Port 0 lets the OS pick; whatever it gives is what gets announced.
            let listener = TcpListener::bind(("0.0.0.0", 0))
                .context("binding a port for the LocalSend receiver")?;
            let port = listener.local_addr()?.port();
            Ok((listener, port))
        }
    }
}

fn tls_config(identity: &cert::Identity) -> Result<ServerConfig> {
    let certs = rustls_pemfile_certs(&identity.certificate_pem)?;
    let key = rustls_pemfile_key(&identity.key_pem)?;
    ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("building the TLS configuration")
}

fn rustls_pemfile_certs(pem: &str) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    let der = cert::der_from_pem(pem).ok_or_else(|| anyhow!("could not read the certificate"))?;
    Ok(vec![rustls::pki_types::CertificateDer::from(der)])
}

fn rustls_pemfile_key(pem: &str) -> Result<rustls::pki_types::PrivateKeyDer<'static>> {
    let der = cert::der_from_pem(pem).ok_or_else(|| anyhow!("could not read the private key"))?;
    rustls::pki_types::PrivateKeyDer::try_from(der)
        .map_err(|err| anyhow!("unusable private key: {err}"))
}

struct Request {
    method: String,
    path: String,
    query: HashMap<String, String>,
    body: Vec<u8>,
}

fn handle(stream: TcpStream, config: Arc<ServerConfig>, receiver: Arc<Receiver>) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(300)))?;
    stream.set_write_timeout(Some(Duration::from_secs(300)))?;
    let connection = ServerConnection::new(config)?;
    let mut tls = StreamOwned::new(connection, stream);
    let request = read_request(&mut tls)?;
    let (status, body) = route(&request, &receiver);
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    tls.write_all(response.as_bytes())?;
    tls.write_all(&body)?;
    tls.flush()?;
    Ok(())
}

/// Read one request, using Content-Length to know where the body ends.
fn read_request(stream: &mut dyn Read) -> Result<Request> {
    let mut raw = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Err(anyhow!("the sender closed the connection"));
        }
        raw.extend_from_slice(&chunk[..read]);
        if let Some(position) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            break position;
        }
        if raw.len() > 64 * 1024 {
            return Err(anyhow!("request headers were unreasonably large"));
        }
    };

    let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
    let mut lines = head.lines();
    let start = lines.next().unwrap_or_default();
    let mut parts = start.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let (path, query_text) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (target, String::new()),
    };
    let query = query_text
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();

    let length: usize = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
        .unwrap_or(0);

    let mut body = raw[head_end + 4..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(length);

    Ok(Request {
        method,
        path,
        query,
        body,
    })
}

fn json_body(value: Value) -> Vec<u8> {
    serde_json::to_vec(&value).unwrap_or_default()
}

fn route(request: &Request, receiver: &Arc<Receiver>) -> (&'static str, Vec<u8>) {
    match (request.method.as_str(), request.path.as_str()) {
        // Both the HTTP discovery fallback and the plain info lookup answer with this device.
        (_, "/api/localsend/v2/register") | ("GET", "/api/localsend/v2/info") => {
            ("200 OK", json_body(receiver.info()))
        }
        ("POST", "/api/localsend/v2/prepare-upload") => prepare_upload(request, receiver),
        ("POST", "/api/localsend/v2/upload") => upload(request, receiver),
        ("POST", "/api/localsend/v2/cancel") => {
            if let Some(session) = request.query.get("sessionId") {
                let _ = receiver.decline(session);
            }
            ("200 OK", json_body(json!({})))
        }
        _ => (
            "404 Not Found",
            json_body(json!({ "message": "unknown endpoint" })),
        ),
    }
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn random_token() -> String {
    // Tokens only need to be unguessable within one short-lived session; the system RNG is what
    // rustls already brings.
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; 16];
    let _ = ring::rand::SystemRandom::new().fill(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn prepare_upload(request: &Request, receiver: &Arc<Receiver>) -> (&'static str, Vec<u8>) {
    let Ok(payload) = serde_json::from_slice::<Value>(&request.body) else {
        return (
            "400 Bad Request",
            json_body(json!({ "message": "unreadable request" })),
        );
    };
    let info = payload.get("info").cloned().unwrap_or(Value::Null);
    let device = info
        .get("alias")
        .and_then(Value::as_str)
        .unwrap_or("Unknown device")
        .to_string();
    let fingerprint = info
        .get("fingerprint")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let Some(offered) = payload.get("files").and_then(Value::as_object) else {
        return (
            "400 Bad Request",
            json_body(json!({ "message": "no files offered" })),
        );
    };

    let mut files = Vec::new();
    for (id, entry) in offered {
        let size = entry.get("size").and_then(Value::as_u64).unwrap_or(0);
        if size > MAX_FILE_BYTES {
            return (
                "400 Bad Request",
                json_body(json!({ "message": "file too large" })),
            );
        }
        files.push(IncomingFile {
            id: id.clone(),
            name: entry
                .get("fileName")
                .and_then(Value::as_str)
                .unwrap_or("unnamed")
                .to_string(),
            size,
            saved_to: None,
        });
    }
    if files.is_empty() {
        return (
            "400 Bad Request",
            json_body(json!({ "message": "no files offered" })),
        );
    }

    let session = random_token();
    let transfer = IncomingTransfer {
        session: session.clone(),
        device,
        fingerprint,
        files: files.clone(),
        requested_at: now_seconds(),
    };

    {
        let mut inner = receiver.inner.lock().unwrap();
        // One transfer at a time, which is what the LocalSend clients expect; a second sender is
        // told to try again rather than silently queued behind a prompt.
        if inner
            .sessions
            .values()
            .any(|existing| existing.decision == Decision::Pending)
        {
            return (
                "409 Conflict",
                json_body(json!({ "message": "Blocked by another session" })),
            );
        }
        inner.sessions.insert(
            session.clone(),
            Session {
                transfer,
                decision: Decision::Pending,
                tokens: HashMap::new(),
            },
        );
    }

    // Hold the sender's connection while someone decides. Nothing has touched the disk yet.
    let decision = wait_for_decision(receiver, &session);

    let mut inner = receiver.inner.lock().unwrap();
    match decision {
        Decision::Accepted => {
            let tokens: HashMap<String, String> = files
                .iter()
                .map(|file| (file.id.clone(), random_token()))
                .collect();
            if let Some(entry) = inner.sessions.get_mut(&session) {
                entry.tokens = tokens.clone();
            }
            (
                "200 OK",
                json_body(json!({ "sessionId": session, "files": tokens })),
            )
        }
        _ => {
            inner.sessions.remove(&session);
            ("403 Forbidden", json_body(json!({ "message": "Declined" })))
        }
    }
}

fn wait_for_decision(receiver: &Arc<Receiver>, session: &str) -> Decision {
    let deadline = Instant::now() + CONSENT_TIMEOUT;
    let mut inner = receiver.inner.lock().unwrap();
    loop {
        let current = inner
            .sessions
            .get(session)
            .map(|entry| entry.decision)
            .unwrap_or(Decision::Declined);
        if current != Decision::Pending {
            return current;
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            // Nobody answered; treat silence as a refusal rather than leaving the sender hanging.
            return Decision::Declined;
        };
        let (guard, _) = receiver
            .decided
            .wait_timeout(inner, remaining)
            .unwrap_or_else(|err| err.into_inner());
        inner = guard;
    }
}

fn upload(request: &Request, receiver: &Arc<Receiver>) -> (&'static str, Vec<u8>) {
    let (Some(session), Some(file_id), Some(token)) = (
        request.query.get("sessionId"),
        request.query.get("fileId"),
        request.query.get("token"),
    ) else {
        return (
            "400 Bad Request",
            json_body(json!({ "message": "missing session, file or token" })),
        );
    };

    let (directory, name) = {
        let inner = receiver.inner.lock().unwrap();
        let Some(entry) = inner.sessions.get(session) else {
            return (
                "403 Forbidden",
                json_body(json!({ "message": "unknown session" })),
            );
        };
        // The token is the only thing authorising this write; a mismatch means the sender never
        // had permission for this file.
        if entry.decision != Decision::Accepted || entry.tokens.get(file_id) != Some(token) {
            return (
                "403 Forbidden",
                json_body(json!({ "message": "invalid token" })),
            );
        }
        let Some(file) = entry.transfer.files.iter().find(|file| &file.id == file_id) else {
            return (
                "403 Forbidden",
                json_body(json!({ "message": "unknown file" })),
            );
        };
        (inner.download_dir.clone(), file.name.clone())
    };

    let destination = match unique_path(&directory, &name) {
        Ok(path) => path,
        Err(err) => {
            return (
                "500 Internal Server Error",
                json_body(json!({ "message": err.to_string() })),
            )
        }
    };
    if let Err(err) = std::fs::write(&destination, &request.body) {
        return (
            "500 Internal Server Error",
            json_body(json!({ "message": err.to_string() })),
        );
    }

    let mut inner = receiver.inner.lock().unwrap();
    if let Some(entry) = inner.sessions.get_mut(session) {
        if let Some(file) = entry
            .transfer
            .files
            .iter_mut()
            .find(|file| &file.id == file_id)
        {
            file.saved_to = Some(destination.display().to_string());
            let saved = file.clone();
            inner.received.insert(0, saved);
        }
    }
    ("200 OK", json_body(json!({})))
}

/// A safe destination inside the download directory.
///
/// The name comes from the sender, so it is reduced to its final component: a name like
/// `../../.ssh/authorized_keys` must not escape the download directory. Collisions get a counter
/// rather than overwriting whatever is already there.
fn unique_path(directory: &Path, name: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(directory)
        .with_context(|| format!("creating {}", directory.display()))?;
    let cleaned = Path::new(name)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .filter(|name| !name.is_empty() && name != "." && name != "..")
        .unwrap_or_else(|| "unnamed".to_string());
    let candidate = directory.join(&cleaned);
    if !candidate.exists() {
        return Ok(candidate);
    }
    let path = Path::new(&cleaned);
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_else(|| cleaned.clone());
    let extension = path
        .extension()
        .map(|extension| format!(".{}", extension.to_string_lossy()))
        .unwrap_or_default();
    for index in 1..1000 {
        let candidate = directory.join(format!("{stem} ({index}){extension}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(anyhow!("too many files named {cleaned}"))
}

/// Answer other devices' announcements so this machine is discoverable.
fn respond_to_announcements(receiver: Arc<Receiver>) -> Result<()> {
    let socket = super::multicast_socket()?;
    let mut buffer = [0u8; 8192];
    loop {
        let (read, from) = match socket.recv_from(&mut buffer) {
            Ok(result) => result,
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(err) => return Err(err).context("reading announcements"),
        };
        let Ok(payload) = serde_json::from_slice::<Value>(&buffer[..read]) else {
            continue;
        };
        // Only answer devices that asked for answers, and never answer ourselves.
        if payload.get("announce").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        let their_fingerprint = payload.get("fingerprint").and_then(Value::as_str);
        if their_fingerprint == Some(receiver.fingerprint().as_str()) {
            continue;
        }
        let mut reply = receiver.info();
        reply["announce"] = json!(false);
        if let Ok(bytes) = serde_json::to_vec(&reply) {
            let _ = socket.send_to(&bytes, from);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn a_sender_cannot_escape_the_download_directory() {
        // The file name is attacker-controlled; a traversal must land inside the directory.
        let dir = tempdir().unwrap();
        let path = unique_path(dir.path(), "../../.ssh/authorized_keys").unwrap();
        assert_eq!(path.parent().unwrap(), dir.path());
        assert_eq!(path.file_name().unwrap(), "authorized_keys");
    }

    #[test]
    fn an_absolute_name_is_reduced_to_its_final_component() {
        let dir = tempdir().unwrap();
        let path = unique_path(dir.path(), "/etc/passwd").unwrap();
        assert_eq!(path.parent().unwrap(), dir.path());
        assert_eq!(path.file_name().unwrap(), "passwd");
    }

    #[test]
    fn an_existing_file_is_never_overwritten() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"original").unwrap();
        let path = unique_path(dir.path(), "notes.txt").unwrap();
        assert_eq!(path.file_name().unwrap(), "notes (1).txt");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("notes.txt")).unwrap(),
            "original"
        );
    }

    #[test]
    fn a_nameless_file_still_gets_a_destination() {
        let dir = tempdir().unwrap();
        for name in ["", ".", ".."] {
            let path = unique_path(dir.path(), name).unwrap();
            assert_eq!(path.parent().unwrap(), dir.path());
        }
    }

    #[test]
    fn a_request_is_split_into_method_path_query_and_body() {
        let raw = b"POST /api/localsend/v2/upload?sessionId=a&fileId=0&token=t HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello";
        let request = read_request(&mut &raw[..]).unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/api/localsend/v2/upload");
        assert_eq!(request.query.get("sessionId").unwrap(), "a");
        assert_eq!(request.query.get("token").unwrap(), "t");
        assert_eq!(request.body, b"hello");
    }

    #[test]
    fn a_body_longer_than_content_length_is_truncated() {
        // Never write more than the sender declared.
        let raw = b"POST /x HTTP/1.1\r\nContent-Length: 2\r\n\r\nhello";
        let request = read_request(&mut &raw[..]).unwrap();
        assert_eq!(request.body, b"he");
    }

    #[test]
    fn tokens_are_unpredictable_and_distinct() {
        let a = random_token();
        let b = random_token();
        assert_ne!(a, b);
        assert_eq!(a.len(), 32);
    }
}
