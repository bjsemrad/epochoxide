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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long a sender is left waiting for someone to accept before the request is dropped.
const CONSENT_TIMEOUT: Duration = Duration::from_secs(120);
/// How often a stopped receiver notices it should shut down.
const ACCEPT_POLL: Duration = Duration::from_millis(150);
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
    /// Where this transfer was accepted into, when the caller chose somewhere other than the
    /// configured download directory.
    directory: Option<PathBuf>,
}

struct Inner {
    sessions: HashMap<String, Session>,
    download_dir: PathBuf,
    port: u16,
    alias: String,
    fingerprint: String,
    /// Files written since the daemon started, newest first.
    received: Vec<IncomingFile>,
    /// Devices this machine has heard from, by fingerprint.
    ///
    /// The receiver is the only thing listening continuously, so it is the only place that
    /// reliably hears every peer. A one-shot scan can miss answers: with the receiver running
    /// there are two sockets bound to the discovery port, and the kernel hands a *unicast* reply
    /// to just one of them. Recording here means it does not matter which one got it.
    seen: HashMap<String, super::Device>,
}

/// Shared receiver state. The condvar is how an accepted or declined request wakes the HTTP
/// handler that is holding the sender's connection open.
pub struct Receiver {
    inner: Mutex<Inner>,
    decided: Condvar,
    /// Set to stop accepting. The worker threads check it and return, which drops the listener
    /// and releases the port -- the whole point of being able to stop.
    stopping: AtomicBool,
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

    /// Remember a device we heard from, whether it announced over multicast or registered
    /// over HTTP.
    pub fn remember(&self, device: super::Device) {
        if device.fingerprint.is_empty() || device.fingerprint == self.fingerprint() {
            return;
        }
        self.inner
            .lock()
            .unwrap()
            .seen
            .insert(device.fingerprint.clone(), device);
    }

    /// Every device heard from since the daemon started.
    pub fn seen(&self) -> Vec<super::Device> {
        self.inner.lock().unwrap().seen.values().cloned().collect()
    }

    fn decide(&self, session: &str, decision: Decision, directory: Option<PathBuf>) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let entry = inner
            .sessions
            .get_mut(session)
            .ok_or_else(|| anyhow!("no transfer waiting with session \"{session}\""))?;
        if entry.decision != Decision::Pending {
            return Err(anyhow!("that transfer has already been answered"));
        }
        entry.decision = decision;
        entry.directory = directory;
        drop(inner);
        self.decided.notify_all();
        Ok(())
    }

    /// Accept a transfer. `directory` overrides the configured download directory for this
    /// transfer only, so a caller can ask where the files should land.
    pub fn accept(&self, session: &str, directory: Option<PathBuf>) -> Result<()> {
        self.decide(session, Decision::Accepted, directory)
    }

    pub fn decline(&self, session: &str) -> Result<()> {
        self.decide(session, Decision::Declined, None)
    }

    /// This device, in the shape LocalSend announcements and `/info` use.
    /// Ask the workers to stop. They notice within a poll interval, drop the listener and the
    /// multicast socket, and the port is free for something else -- the LocalSend app, usually.
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        // Wake anything waiting on a consent decision so it does not sit until its timeout.
        self.decided.notify_all();
    }

    fn stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

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
    let identity = cert::shared()?;
    let (listener, port) = bind(preferred)?;

    let receiver = Arc::new(Receiver {
        inner: Mutex::new(Inner {
            sessions: HashMap::new(),
            download_dir,
            port,
            alias,
            fingerprint: identity.fingerprint.clone(),
            received: Vec::new(),
            seen: HashMap::new(),
        }),
        decided: Condvar::new(),
        stopping: AtomicBool::new(false),
    });

    let config = Arc::new(tls_config(&identity)?);
    let serving = Arc::clone(&receiver);
    listener
        .set_nonblocking(true)
        .context("configuring the listener")?;
    std::thread::spawn(move || {
        loop {
            if serving.stopping() {
                // Returning drops `listener`, which is what actually frees the port.
                return;
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let config = Arc::clone(&config);
                    let receiver = Arc::clone(&serving);
                    // One thread per transfer: a handler blocks for as long as consent takes, so
                    // it must not hold up anything else.
                    std::thread::spawn(move || {
                        if let Err(err) = handle(stream, config, receiver) {
                            eprintln!("localsend: {err:#}");
                        }
                    });
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(ACCEPT_POLL);
                }
                Err(_) => return,
            }
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
    ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(identity.chain()?, identity.key()?)
        .context("building the TLS configuration")
}

struct Request {
    method: String,
    path: String,
    query: HashMap<String, String>,
    body: Vec<u8>,
}

fn handle(stream: TcpStream, config: Arc<ServerConfig>, receiver: Arc<Receiver>) -> Result<()> {
    let peer = stream.peer_addr().ok().map(|address| address.ip());
    stream.set_read_timeout(Some(Duration::from_secs(300)))?;
    stream.set_write_timeout(Some(Duration::from_secs(300)))?;
    let connection = ServerConnection::new(config)?;
    let mut tls = StreamOwned::new(connection, stream);
    let request = read_request(&mut tls)?;
    let (status, body) = route(&request, &receiver, peer);
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    tls.write_all(response.as_bytes())?;
    tls.write_all(&body)?;
    tls.flush()?;
    Ok(())
}

/// Read one request, honouring whichever framing the sender chose.
///
/// Both framings turn up in practice. A sender that knows the size up front sends a
/// Content-Length; LocalSend's own client streams file uploads instead and frames them with
/// `Transfer-Encoding: chunked`, the same way its server answers us (see `http::parse_response`).
/// Reading only Content-Length made a chunked upload look like a zero-length body, which is how
/// an accepted transfer landed on disk as an empty file.
fn read_request<S: Read + Write>(stream: &mut S) -> Result<Request> {
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

    let headers: Vec<(&str, &str)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim(), value.trim()))
        .collect();
    let header = |wanted: &str| {
        headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(wanted))
            .map(|(_, value)| *value)
    };

    // Whatever arrived alongside the head is already the start of the body.
    let mut body = raw[head_end + 4..].to_vec();

    // A sender that asks permission first will not send a byte until it is told to go ahead --
    // curl does this for any sizeable upload -- so silence here reads to it as a stall.
    if body.is_empty()
        && header("expect").is_some_and(|value| value.eq_ignore_ascii_case("100-continue"))
    {
        stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
        stream.flush()?;
    }

    if header("transfer-encoding").is_some_and(|value| value.to_lowercase().contains("chunked")) {
        body = read_chunked(stream, body)?;
    } else {
        let length: usize = header("content-length")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        while body.len() < length {
            let read = stream.read(&mut chunk)?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read]);
        }
        body.truncate(length);
    }

    Ok(Request {
        method,
        path,
        query,
        body,
    })
}

/// Read a chunked body: repeating `<hex size>CRLF<bytes>CRLF`, ending at a zero-size chunk.
///
/// `buffered` is however much of the body already came in with the head. Chunked framing declares
/// no total up front, so `MAX_FILE_BYTES` is the only thing bounding what a sender can push here.
fn read_chunked<S: Read>(stream: &mut S, buffered: Vec<u8>) -> Result<Vec<u8>> {
    let mut pending = buffered;
    let mut body: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];

    // Pull until `pending` holds at least `wanted` bytes, reporting whether the sender ran out.
    let mut fill = |pending: &mut Vec<u8>, wanted: usize, stream: &mut S| -> Result<bool> {
        while pending.len() < wanted {
            let read = stream.read(&mut chunk)?;
            if read == 0 {
                return Ok(false);
            }
            pending.extend_from_slice(&chunk[..read]);
        }
        Ok(true)
    };

    loop {
        // Every chunk opens with its size on a line of its own.
        let line_end = loop {
            if let Some(position) = pending.windows(2).position(|window| window == b"\r\n") {
                break position;
            }
            if pending.len() > 1024 {
                return Err(anyhow!("the sender's chunk header was unreasonably long"));
            }
            let more = pending.len() + 1;
            if !fill(&mut pending, more, stream)? {
                return Err(anyhow!("the sender closed the connection mid-transfer"));
            }
        };
        let header = String::from_utf8_lossy(&pending[..line_end]).to_string();
        // A chunk size may carry extensions after a semicolon.
        let size_text = header.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| anyhow!("the sender used an unreadable chunk size \"{size_text}\""))?;
        pending.drain(..line_end + 2);
        if size == 0 {
            break;
        }
        if body.len() as u64 + size as u64 > MAX_FILE_BYTES {
            return Err(anyhow!("the upload is larger than this receiver accepts"));
        }
        // The chunk's bytes plus the CRLF closing it.
        let complete = fill(&mut pending, size + 2, stream)?;
        let taken = size.min(pending.len());
        body.extend_from_slice(&pending[..taken]);
        pending.drain(..(taken + 2).min(pending.len()));
        if !complete {
            // A sender that hangs up without its final zero chunk still delivered what arrived.
            break;
        }
    }

    Ok(body)
}

fn json_body(value: Value) -> Vec<u8> {
    serde_json::to_vec(&value).unwrap_or_default()
}

fn route(
    request: &Request,
    receiver: &Arc<Receiver>,
    peer: Option<std::net::IpAddr>,
) -> (&'static str, Vec<u8>) {
    match (request.method.as_str(), request.path.as_str()) {
        // Both the HTTP discovery fallback and the plain info lookup answer with this device.
        (_, "/api/localsend/v2/register") | ("GET", "/api/localsend/v2/info") => {
            // A register is the other device announcing itself, so record it: answering without
            // remembering is how this machine ended up visible to peers it could not itself see.
            if let (Some(address), Ok(payload)) =
                (peer, serde_json::from_slice::<Value>(&request.body))
            {
                if let Some(device) =
                    super::device_from_announcement(&payload, address, receiver.port())
                {
                    receiver.remember(device);
                }
            }
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
                directory: None,
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
        if receiver.stopping() {
            return Decision::Declined;
        }
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

    let (directory, name, offered) = {
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
        // Whatever the transfer was accepted into, falling back to the configured directory.
        let directory = entry
            .directory
            .clone()
            .unwrap_or_else(|| inner.download_dir.clone());
        (directory, file.name.clone(), file.size)
    };

    // The body has to be the size that was offered and accepted. Anything else means the request
    // was framed in a way this server misread, and writing it anyway is how a truncated -- or
    // empty -- file ends up on disk looking like a completed transfer. A sender that offered no
    // size gets the benefit of the doubt, since there is nothing to check against.
    if offered > 0 && request.body.len() as u64 != offered {
        return (
            "400 Bad Request",
            json_body(json!({
                "message": format!(
                    "expected {offered} bytes of \"{name}\" but the body held {}",
                    request.body.len()
                )
            })),
        );
    }

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
        if receiver.stopping() {
            return Ok(());
        }
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
        let their_fingerprint = payload.get("fingerprint").and_then(Value::as_str);
        if their_fingerprint == Some(receiver.fingerprint().as_str()) {
            continue;
        }
        // Record every device heard from, announcement or reply alike -- this is the socket that
        // is always listening, so it is what makes discovery reliable.
        if let Some(device) = super::parse_announcement(&buffer[..read], from) {
            receiver.remember(device);
        }
        // Only answer devices that asked for answers.
        if payload.get("announce").and_then(Value::as_bool) != Some(true) {
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

    /// One canned request, handed over in `per_read`-sized pieces, with whatever the server
    /// writes back kept for inspection. Real requests arrive split across reads, and a body
    /// reader that only works when everything lands in one go is the bug this guards.
    struct Wire {
        incoming: Vec<u8>,
        outgoing: Vec<u8>,
        per_read: usize,
    }

    impl Wire {
        fn new(request: &[u8], per_read: usize) -> Self {
            Self {
                incoming: request.to_vec(),
                outgoing: Vec::new(),
                per_read,
            }
        }
    }

    impl Read for Wire {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let take = self.per_read.min(buf.len()).min(self.incoming.len());
            buf[..take].copy_from_slice(&self.incoming[..take]);
            self.incoming.drain(..take);
            Ok(take)
        }
    }

    impl Write for Wire {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.outgoing.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_request_is_split_into_method_path_query_and_body() {
        let raw = b"POST /api/localsend/v2/upload?sessionId=a&fileId=0&token=t HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello";
        let request = read_request(&mut Wire::new(raw, 8192)).unwrap();
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
        let request = read_request(&mut Wire::new(raw, 8192)).unwrap();
        assert_eq!(request.body, b"he");
    }

    #[test]
    fn a_chunked_upload_is_reassembled() {
        // LocalSend streams file uploads, so they carry no Content-Length at all. Reading only
        // Content-Length made every one of these an empty body -- and an empty file on disk.
        let raw = b"POST /api/localsend/v2/upload?sessionId=a&fileId=0&token=t HTTP/1.1\r\n                    Transfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let request = read_request(&mut Wire::new(raw, 8192)).unwrap();
        assert_eq!(request.body, b"hello world");
    }

    #[test]
    fn a_chunked_upload_split_across_reads_is_reassembled() {
        // A byte at a time is the worst case: every size line and every chunk straddles a read.
        let raw = b"POST /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
        for per_read in [1, 2, 7, 8192] {
            let request = read_request(&mut Wire::new(raw, per_read)).unwrap();
            assert_eq!(request.body, b"abcde", "per_read {per_read}");
        }
    }

    #[test]
    fn a_chunk_size_may_carry_extensions() {
        let raw =
            b"POST /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n5;name=v\r\nhello\r\n0\r\n\r\n";
        assert_eq!(read_request(&mut Wire::new(raw, 3)).unwrap().body, b"hello");
    }

    #[test]
    fn a_sender_that_asks_before_uploading_is_told_to_go_ahead() {
        // curl sends this for any sizeable body and will not start until it is answered.
        let raw = b"POST /x HTTP/1.1\r\nExpect: 100-continue\r\nContent-Length: 5\r\n\r\nhello";
        let mut wire = Wire::new(raw, 8192);
        // The head has to arrive on its own, the way it does when the sender is waiting.
        let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        wire.per_read = split;
        let request = read_request(&mut wire).unwrap();
        assert_eq!(wire.outgoing, b"HTTP/1.1 100 Continue\r\n\r\n");
        assert_eq!(request.body, b"hello");
    }

    #[test]
    fn a_request_with_no_body_framing_has_no_body() {
        // `/info` and the discovery fallback send neither header, and must not hang waiting.
        let raw = b"GET /api/localsend/v2/info HTTP/1.1\r\nHost: x\r\n\r\n";
        assert!(read_request(&mut Wire::new(raw, 8192))
            .unwrap()
            .body
            .is_empty());
    }

    #[test]
    fn an_unreadable_chunk_size_is_refused() {
        let raw = b"POST /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\nhello\r\n";
        assert!(read_request(&mut Wire::new(raw, 8192)).is_err());
    }

    /// A receiver holding one accepted transfer, ready for `upload` to be called against it.
    fn accepted(directory: &Path, size: u64) -> Arc<Receiver> {
        let file = IncomingFile {
            id: "0".to_string(),
            name: "notes.txt".to_string(),
            size,
            saved_to: None,
        };
        let mut sessions = HashMap::new();
        sessions.insert(
            "s".to_string(),
            Session {
                transfer: IncomingTransfer {
                    session: "s".to_string(),
                    device: "Phone".to_string(),
                    fingerprint: "ab12".to_string(),
                    files: vec![file],
                    requested_at: 0,
                },
                decision: Decision::Accepted,
                tokens: HashMap::from([("0".to_string(), "t".to_string())]),
                directory: Some(directory.to_path_buf()),
            },
        );
        Arc::new(Receiver {
            inner: Mutex::new(Inner {
                sessions,
                download_dir: directory.to_path_buf(),
                port: 53317,
                alias: "Epoch".to_string(),
                fingerprint: "cd34".to_string(),
                received: Vec::new(),
                seen: HashMap::new(),
            }),
            decided: Condvar::new(),
            stopping: AtomicBool::new(false),
        })
    }

    fn upload_request(body: &[u8]) -> Request {
        Request {
            method: "POST".to_string(),
            path: "/api/localsend/v2/upload".to_string(),
            query: HashMap::from([
                ("sessionId".to_string(), "s".to_string()),
                ("fileId".to_string(), "0".to_string()),
                ("token".to_string(), "t".to_string()),
            ]),
            body: body.to_vec(),
        }
    }

    #[test]
    fn an_accepted_file_is_written_whole() {
        let dir = tempdir().unwrap();
        let receiver = accepted(dir.path(), 5);
        let (status, _) = upload(&upload_request(b"hello"), &receiver);
        assert_eq!(status, "200 OK");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("notes.txt")).unwrap(),
            "hello"
        );
        assert_eq!(receiver.received().len(), 1);
    }

    #[test]
    fn a_body_that_does_not_match_what_was_offered_is_refused() {
        // The symptom this backstops: a body the server misread arriving as nothing, and being
        // written out as a zero-byte file that looks like a completed transfer.
        let dir = tempdir().unwrap();
        let receiver = accepted(dir.path(), 5);
        let (status, _) = upload(&upload_request(b""), &receiver);
        assert_eq!(status, "400 Bad Request");
        assert!(!dir.path().join("notes.txt").exists());
        assert!(receiver.received().is_empty());
    }

    #[test]
    fn a_sender_that_offered_no_size_is_taken_at_its_word() {
        // Nothing to check against, so the bytes are written rather than refused.
        let dir = tempdir().unwrap();
        let receiver = accepted(dir.path(), 0);
        let (status, _) = upload(&upload_request(b"hello"), &receiver);
        assert_eq!(status, "200 OK");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("notes.txt")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn tokens_are_unpredictable_and_distinct() {
        let a = random_token();
        let b = random_token();
        assert_ne!(a, b);
        assert_eq!(a.len(), 32);
    }
}
