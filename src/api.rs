//! The Epoch API contract.
//!
//! One versioned, discoverable surface in front of everything EpochOxide knows that is not a
//! launcher result. Methods are `group.method`, grouped by domain, and every group declares
//! whether it is actually usable on this machine.
//!
//! Three rules hold across the whole surface:
//!
//! 1. **Normalized, not raw.** A caller never sees a `hyprctl` payload or a `tailscale status`
//!    blob. Compositor and service details are mapped to stable shapes first, so a Hyprland
//!    upgrade or a move to niri does not reach the UI.
//! 2. **Discoverable.** `api.describe` reports the contract version, every group and method, and
//!    the availability of each group, so a shell can feature-detect instead of guessing from
//!    error strings.
//! 3. **Typed failures.** Errors carry a machine-readable `code`, so a caller can tell "this
//!    build has no such method" from "the compositor is not answering".
//!
//! Compatibility: the major version changes when an existing method's shape changes
//! incompatibly. Adding a group, a method, or a field is a minor bump.

use crate::{awake, capture, compositor, hardware, localsend, nix, power, tailscale};
use serde::Serialize;
use serde_json::{json, Value};

pub const VERSION: &str = "1.0";
pub const VERSION_MAJOR: u32 = 1;
pub const VERSION_MINOR: u32 = 0;

/// Why a call failed, in a form a caller can branch on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// No such group in this contract.
    UnknownGroup,
    /// The group exists but has no such method.
    UnknownMethod,
    /// The group is part of the contract but not usable here (not implemented, or its
    /// dependency is missing).
    Unavailable,
    /// Parameters were missing or the wrong type.
    InvalidParams,
    /// The underlying tool or compositor failed.
    BackendError,
    /// The caller asked for a contract major version this build does not speak.
    VersionMismatch,
}

#[derive(Debug, Clone)]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
    /// Extra context, e.g. the methods that do exist when one does not.
    pub detail: Option<Value>,
}

impl ApiError {
    fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            detail: None,
        }
    }

    fn with_detail(mut self, detail: Value) -> Self {
        self.detail = Some(detail);
        self
    }

    pub fn to_value(&self) -> Value {
        json!({
            "code": self.code,
            "message": self.message,
            "detail": self.detail,
        })
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

/// Wrap a backend failure, keeping its whole cause chain.
///
/// anyhow's `Display` shows only the outermost context, so a failure three layers down arrives as
/// "asking X to accept the transfer" with no hint of why. The alternate form keeps the chain.
fn backend_error(err: anyhow::Error) -> ApiError {
    ApiError::new(ErrorCode::BackendError, format!("{err:#}"))
}

/// Whether a group can be called on this machine, and why not when it cannot.
#[derive(Debug, Clone)]
pub enum Availability {
    Available,
    /// In the contract, but nothing implements it in this build yet.
    Planned,
    /// Implemented, but a dependency is missing at runtime.
    Unavailable(String),
}

impl Availability {
    fn status(&self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Planned => "planned",
            Self::Unavailable(_) => "unavailable",
        }
    }

    fn reason(&self) -> Option<&str> {
        match self {
            Self::Unavailable(reason) => Some(reason),
            Self::Planned => Some("not implemented in this build"),
            Self::Available => None,
        }
    }

    fn callable(&self) -> bool {
        matches!(self, Self::Available)
    }
}

struct Method {
    name: &'static str,
    summary: &'static str,
    params: &'static [(&'static str, &'static str)],
    /// A streaming method answers with many payloads over a held-open connection instead of one.
    streaming: bool,
}

struct Group {
    name: &'static str,
    summary: &'static str,
    methods: &'static [Method],
}

const fn method(
    name: &'static str,
    summary: &'static str,
    params: &'static [(&'static str, &'static str)],
) -> Method {
    Method {
        name,
        summary,
        params,
        streaming: false,
    }
}

const fn streaming(name: &'static str, summary: &'static str) -> Method {
    Method {
        name,
        summary,
        params: &[],
        streaming: true,
    }
}

const COMPOSITOR: &[Method] = &[
    method("windows", "Every open window, normalized", &[]),
    method("activeWindow", "The focused window, or null", &[]),
    method("workspaces", "Every workspace", &[]),
    method("monitors", "Every monitor", &[]),
    method(
        "focusWindow",
        "Focus a window by id",
        &[("id", "string, from compositor.windows")],
    ),
    method(
        "closeWindow",
        "Close a window by id",
        &[("id", "string, from compositor.windows")],
    ),
    method(
        "focusWorkspace",
        "Switch to a workspace by id or name",
        &[("id", "string, from compositor.workspaces, or a bare name")],
    ),
    streaming(
        "subscribe",
        "Stream normalized state whenever the compositor changes",
    ),
];

const TAILSCALE: &[Method] = &[
    method("status", "Backend state, tailnet, exit node, health", &[]),
    method("machines", "Every machine in the tailnet", &[]),
    method("up", "Bring Tailscale up", &[]),
    method("down", "Bring Tailscale down", &[]),
    method("pendingFiles", "Files waiting in the Taildrop inbox", &[]),
    method(
        "receive",
        "Receive waiting Taildrop files into a directory",
        &[("directory", "target directory")],
    ),
    method(
        "send",
        "Send files to a peer with Taildrop",
        &[
            ("peer", "string, a name from tailscale.machines"),
            ("files", "array of absolute paths"),
        ],
    ),
];

const CAPTURE: &[Method] = &[
    method(
        "screenshot",
        "Take a screenshot, save it, and put it on the clipboard",
        &[
            (
                "mode",
                "optional string: region (default), window, fullscreen, or all",
            ),
            (
                "output",
                "optional string, a monitor name from compositor.monitors; fullscreen only",
            ),
            (
                "select",
                "optional bool; in window mode, click the window instead of taking the focused one",
            ),
            ("cursor", "optional bool; include the pointer"),
            ("delay", "optional number of seconds to wait before capturing"),
            ("copy", "optional bool, defaulting to screenshot_copy"),
            ("save", "optional bool, defaulting to screenshot_save"),
            (
                "directory",
                "optional string; where to save this shot, defaulting to screenshot_dir",
            ),
            ("notify", "optional bool, defaulting to screenshot_notify"),
        ],
    ),
    method(
        "ocr",
        "Capture a region and copy the text read out of it",
        &[
            (
                "mode",
                "optional string: region (default), window, fullscreen, or all",
            ),
            (
                "language",
                "optional tesseract language, defaulting to ocr_language; join several with +",
            ),
            ("select", "optional bool; click the window instead of taking the focused one"),
            ("delay", "optional number of seconds to wait before capturing"),
            ("copy", "optional bool, defaulting to screenshot_copy"),
            (
                "save",
                "optional bool; keep the captured image too, off by default",
            ),
            ("notify", "optional bool, defaulting to screenshot_notify"),
        ],
    ),
    method(
        "record",
        "Start recording the screen",
        &[
            (
                "mode",
                "optional string: region (default), window, fullscreen, or all",
            ),
            (
                "output",
                "optional string, a monitor name from compositor.monitors; fullscreen only",
            ),
            (
                "select",
                "optional bool; click the window instead of taking the focused one",
            ),
            ("delay", "optional number of seconds to wait before starting"),
            (
                "directory",
                "optional string; where to write, defaulting to recording_dir",
            ),
        ],
    ),
    method(
        "stopRecording",
        "Stop the recording in progress and finish the file",
        &[("notify", "optional bool, defaulting to recording_notify")],
    ),
    method(
        "recording",
        "What is being recorded right now, if anything",
        &[],
    ),
    method(
        "status",
        "Where screenshots land, which capture tools are installed, and what this compositor supports",
        &[],
    ),
];

const LOCALSEND: &[Method] = &[
    method("devices", "Discover LocalSend devices on the network", &[]),
    method(
        "status",
        "Whether this machine is accepting transfers, and where they land",
        &[],
    ),
    method(
        "pending",
        "Transfers waiting for someone to accept them",
        &[],
    ),
    method(
        "accept",
        "Accept a waiting transfer",
        &[
            ("session", "string, from localsend.pending"),
            (
                "directory",
                "optional string; where to save this transfer, defaulting to localsend_download_dir",
            ),
        ],
    ),
    method(
        "decline",
        "Decline a waiting transfer",
        &[("session", "string, from localsend.pending")],
    ),
    method("received", "Files accepted since the daemon started", &[]),
    method(
        "startReceiving",
        "Start accepting transfers, binding the LocalSend port",
        &[],
    ),
    method(
        "stopReceiving",
        "Stop accepting and release the port, so the LocalSend app can use it",
        &[],
    ),
    method(
        "send",
        "Send files to a device",
        &[
            ("device", "string, an alias from localsend.devices"),
            ("files", "array of absolute paths"),
        ],
    ),
];

const NIX: &[Method] = &[
    method(
        "status",
        "Flake update state, from the last check; answers from memory",
        &[],
    ),
    method(
        "check",
        "Resolve every flake input and report what could be updated",
        &[],
    ),
    method(
        "update",
        "Run the configured update command in a terminal",
        &[],
    ),
    method(
        "rebuild",
        "Run the configured rebuild command in a terminal",
        &[(
            "host",
            "optional string, a name from nix.hosts; replaces %HOST% in nix_rebuild_command",
        )],
    ),
    method(
        "hosts",
        "The hosts this flake defines, for the rebuild action",
        &[],
    ),
];

const SYSTEM: &[Method] = &[
    method(
        "power",
        "CPU power state: profile, governor, energy preference, turbo, and what is managing them",
        &[],
    ),
    method(
        "stayAwake",
        "Whether the machine is being held out of idle and sleep",
        &[],
    ),
    method(
        "hardware",
        "What machine this is, and how its battery has worn",
        &[],
    ),
    method(
        "firmware",
        "Firmware updates fwupd is offering",
        &[("refresh", "optional bool; skip the cached answer")],
    ),
    method(
        "setStayAwake",
        "Hold the machine awake, or let it idle again",
        &[
            ("enabled", "optional bool; omit to toggle"),
            ("reason", "optional string shown in systemd-inhibit --list"),
            ("notify", "optional bool, defaulting to true"),
        ],
    ),
];

const GROUPS: &[Group] = &[
    Group {
        name: "compositor",
        summary: "Normalized window, workspace, and monitor state",
        methods: COMPOSITOR,
    },
    Group {
        name: "tailscale",
        summary: "Tailnet status, machines, and Taildrop",
        methods: TAILSCALE,
    },
    Group {
        name: "capture",
        summary: "Screenshots, OCR, colour picking, and recording",
        methods: CAPTURE,
    },
    // Declared so the contract shape is stable and callers can feature-detect, but nothing here
    // is implemented yet. Each lands with its own build-out step rather than as a stub.
    Group {
        name: "localsend",
        summary: "LocalSend device discovery and transfers",
        methods: LOCALSEND,
    },
    Group {
        name: "dev",
        summary: "Project discovery and developer workflows",
        methods: &[],
    },
    Group {
        name: "nix",
        summary: "Flake update status and rebuilds",
        methods: NIX,
    },
    Group {
        name: "system",
        summary: "Power profiles and system state",
        methods: SYSTEM,
    },
];

/// Methods that answer even when their group is not available here.
///
/// A diagnostic is worth calling precisely when the thing it diagnoses is missing: `capture.status`
/// is how a caller finds out that grim is not installed, so refusing it because grim is not
/// installed would leave nobody able to ask. `capture.recording` is the same kind of question: the
/// shell polls it to draw its indicator, and "is anything recording" has an answer on a machine
/// with no grim.
const ALWAYS_ANSWERS: &[&str] = &[
    "capture.status",
    "capture.recording",
    "nix.status",
    "system.stayAwake",
    "system.hardware",
];

/// A group's availability is decided at call time, not at startup: a compositor can be restarted
/// and Tailscale can be installed without EpochOxide being restarted.
fn availability(group: &str) -> Availability {
    match group {
        "compositor" => match compositor::active() {
            Some(_) => Availability::Available,
            None => Availability::Unavailable("no supported compositor is responding".into()),
        },
        "tailscale" => {
            if tailscale::available() {
                Availability::Available
            } else {
                Availability::Unavailable("the tailscale CLI is not installed".into())
            }
        }
        "nix" => match nix::available() {
            Ok(()) => Availability::Available,
            Err(reason) => Availability::Unavailable(reason),
        },
        "system" => match power::available() {
            Ok(()) => Availability::Available,
            Err(reason) => Availability::Unavailable(reason),
        },
        "capture" => match capture::available() {
            Ok(()) => Availability::Available,
            Err(reason) => Availability::Unavailable(reason),
        },
        // Discovery needs the multicast port, which is the one thing that can stop this working
        // on an otherwise fine machine.
        "localsend" => match localsend::available() {
            Ok(()) => Availability::Available,
            Err(err) => Availability::Unavailable(err.to_string()),
        },
        _ => Availability::Planned,
    }
}

fn group(name: &str) -> Option<&'static Group> {
    GROUPS.iter().find(|group| group.name == name)
}

fn group_names() -> Vec<&'static str> {
    GROUPS.iter().map(|group| group.name).collect()
}

/// The full contract: version, groups, methods, and what is callable right now.
pub fn describe() -> Value {
    let groups: Vec<Value> = GROUPS
        .iter()
        .map(|group| {
            let status = availability(group.name);
            json!({
                "name": group.name,
                "summary": group.summary,
                "status": status.status(),
                "reason": status.reason(),
                "methods": group.methods.iter().map(|method| json!({
                    "name": format!("{}.{}", group.name, method.name),
                    "summary": method.summary,
                    "streaming": method.streaming,
                    "params": method.params.iter().map(|(name, kind)| json!({
                        "name": name,
                        "type": kind,
                    })).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({
        "version": VERSION,
        "major": VERSION_MAJOR,
        "minor": VERSION_MINOR,
        "groups": groups,
    })
}

/// Whether `method` streams. The server holds the connection open for these instead of writing
/// one response and moving on.
pub fn is_streaming(method: &str) -> bool {
    let Some((group_name, call)) = method.split_once('.') else {
        return false;
    };
    group(group_name)
        .is_some_and(|found| found.methods.iter().any(|m| m.name == call && m.streaming))
}

/// Run a streaming method, handing each payload to `emit`. Returns when `emit` fails, which is
/// how a disconnected client ends the stream.
pub fn stream(
    method: &str,
    _params: &Value,
    emit: impl FnMut(Value) -> anyhow::Result<()>,
) -> Result<(), ApiError> {
    match method {
        "compositor.subscribe" => {
            let mut emit = emit;
            compositor::watch(|state| emit(serde_json::to_value(state).unwrap_or(Value::Null)))
                .map_err(backend_error)
        }
        _ => Err(ApiError::new(
            ErrorCode::UnknownMethod,
            format!("\"{method}\" is not a streaming method"),
        )),
    }
}

/// The running receiver, or a typed error explaining that transfers are not being accepted.
fn receiving() -> Result<std::sync::Arc<localsend::server::Receiver>, ApiError> {
    localsend::receiver().ok_or_else(|| {
        ApiError::new(
            ErrorCode::Unavailable,
            "this machine is not accepting LocalSend transfers (localsend_receive is off, or the receiver failed to start)",
        )
    })
}

/// An optional directory parameter, `~` expanded and required to exist.
///
/// A path that is not there is refused rather than created: a typo would otherwise silently make
/// a directory and drop files somewhere nobody looks.
fn optional_directory(params: &Value, key: &str) -> Result<Option<std::path::PathBuf>, ApiError> {
    let Some(raw) = params.get(key).and_then(Value::as_str) else {
        return Ok(None);
    };
    if raw.is_empty() {
        return Ok(None);
    }
    let expanded = std::path::PathBuf::from(shellexpand::tilde(raw).to_string());
    if !expanded.is_dir() {
        return Err(ApiError::new(
            ErrorCode::InvalidParams,
            format!("{} is not a directory", expanded.display()),
        ));
    }
    Ok(Some(expanded))
}

/// A path parameter that does not have to exist yet.
///
/// Unlike [`optional_directory`], a missing directory here is created rather than refused: the
/// caller is naming where its own screenshot should go, and `~/Pictures/Screenshots` not existing
/// on a fresh machine is the normal case rather than a typo.
fn optional_path(params: &Value, key: &str) -> Option<std::path::PathBuf> {
    params
        .get(key)
        .and_then(Value::as_str)
        .filter(|raw| !raw.is_empty())
        .map(|raw| std::path::PathBuf::from(shellexpand::tilde(raw).to_string()))
}

fn param_bool(params: &Value, key: &str) -> Option<bool> {
    params.get(key).and_then(Value::as_bool)
}

/// A parameter that may arrive as a number or as the string a shell handed straight through.
fn param_number(params: &Value, key: &str) -> Result<Option<f64>, ApiError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => Ok(number.as_f64()),
        Some(Value::String(text)) if text.is_empty() => Ok(None),
        Some(Value::String(text)) => text.parse().map(Some).map_err(|_| {
            ApiError::new(
                ErrorCode::InvalidParams,
                format!("\"{key}\" must be a number, not \"{text}\""),
            )
        }),
        Some(other) => Err(ApiError::new(
            ErrorCode::InvalidParams,
            format!("\"{key}\" must be a number, not {other}"),
        )),
    }
}

fn param_str<'a>(params: &'a Value, key: &str) -> Result<&'a str, ApiError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ApiError::new(
                ErrorCode::InvalidParams,
                format!("missing required parameter \"{key}\""),
            )
        })
}

fn param_strings(params: &Value, key: &str) -> Result<Vec<String>, ApiError> {
    let items = params.get(key).and_then(Value::as_array).ok_or_else(|| {
        ApiError::new(
            ErrorCode::InvalidParams,
            format!("missing required parameter \"{key}\" (an array of strings)"),
        )
    })?;
    items
        .iter()
        .map(|item| {
            item.as_str().map(str::to_string).ok_or_else(|| {
                ApiError::new(
                    ErrorCode::InvalidParams,
                    format!("\"{key}\" must contain only strings"),
                )
            })
        })
        .collect()
}

/// Read the parameters `capture.screenshot` and `capture.ocr` share.
///
/// The two differ only in what they do with the frame afterwards, so what to point the camera at
/// is read once here rather than drifting apart in two places.
fn capture_request(params: &Value) -> Result<capture::Request, ApiError> {
    let mode = match params.get("mode").and_then(Value::as_str) {
        Some(mode) if !mode.is_empty() => capture::Mode::parse(mode)
            .map_err(|err| ApiError::new(ErrorCode::InvalidParams, err.to_string()))?,
        _ => capture::Mode::default(),
    };
    // A negative delay is a sign the caller meant something else; it cannot be honoured either
    // way, and Duration::from_secs_f64 panics on one.
    let delay = param_number(params, "delay")?.unwrap_or(0.0);
    if delay < 0.0 || !delay.is_finite() {
        return Err(ApiError::new(
            ErrorCode::InvalidParams,
            format!("\"delay\" must be a number of seconds, not {delay}"),
        ));
    }
    let text = |key: &str| {
        params
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    Ok(capture::Request {
        mode,
        output: text("output"),
        select: param_bool(params, "select").unwrap_or(false),
        cursor: param_bool(params, "cursor").unwrap_or(false),
        delay: std::time::Duration::from_secs_f64(delay),
        copy: param_bool(params, "copy"),
        save: param_bool(params, "save"),
        notify: param_bool(params, "notify"),
        directory: optional_path(params, "directory"),
        language: text("language"),
    })
}

fn value<T: Serialize>(data: T) -> Result<Value, ApiError> {
    serde_json::to_value(data)
        .map_err(|err| ApiError::new(ErrorCode::BackendError, err.to_string()))
}

/// Call `group.method`. `params` may be `Value::Null` for methods that take none.
///
/// `version` is the contract major the caller was built against; a mismatch is refused rather
/// than served with a shape the caller may not understand.
pub fn dispatch(method: &str, params: &Value, version: Option<u32>) -> Result<Value, ApiError> {
    if let Some(requested) = version {
        if requested != VERSION_MAJOR {
            return Err(ApiError::new(
                ErrorCode::VersionMismatch,
                format!("this build speaks API v{VERSION_MAJOR}, caller asked for v{requested}"),
            ));
        }
    }

    if method == "api.describe" {
        return Ok(describe());
    }

    let Some((group_name, call)) = method.split_once('.') else {
        return Err(ApiError::new(
            ErrorCode::UnknownMethod,
            format!("\"{method}\" is not a method name; expected \"group.method\""),
        )
        .with_detail(json!({ "groups": group_names() })));
    };

    let Some(found) = group(group_name) else {
        return Err(ApiError::new(
            ErrorCode::UnknownGroup,
            format!("no API group \"{group_name}\""),
        )
        .with_detail(json!({ "groups": group_names() })));
    };

    let status = availability(found.name);
    if !status.callable() && !ALWAYS_ANSWERS.contains(&method) {
        return Err(ApiError::new(
            ErrorCode::Unavailable,
            format!(
                "the \"{group_name}\" group is {} here: {}",
                status.status(),
                status.reason().unwrap_or("unavailable")
            ),
        ));
    }

    if !found.methods.iter().any(|m| m.name == call) {
        return Err(
            ApiError::new(ErrorCode::UnknownMethod, format!("no method \"{method}\"")).with_detail(
                json!({
                    "methods": found.methods.iter()
                        .map(|m| format!("{}.{}", found.name, m.name))
                        .collect::<Vec<_>>(),
                }),
            ),
        );
    }

    match (group_name, call) {
        ("compositor", "windows") => value(compositor::windows()),
        ("compositor", "activeWindow") => value(compositor::active_window()),
        ("compositor", "workspaces") => value(compositor::workspaces()),
        ("compositor", "monitors") => value(compositor::monitors()),
        ("compositor", "focusWindow") => {
            compositor::focus_window(param_str(params, "id")?).map_err(backend_error)?;
            Ok(json!({ "focused": true }))
        }
        ("compositor", "closeWindow") => {
            compositor::close_window(param_str(params, "id")?).map_err(backend_error)?;
            Ok(json!({ "closed": true }))
        }
        ("compositor", "focusWorkspace") => {
            compositor::focus_workspace(param_str(params, "id")?).map_err(backend_error)?;
            Ok(json!({ "focused": true }))
        }
        ("compositor", "subscribe") => Err(ApiError::new(
            ErrorCode::InvalidParams,
            "compositor.subscribe is a streaming method; it cannot be called as a single request",
        )),
        ("tailscale", "status") => value(tailscale::status().map_err(backend_error)?),
        ("tailscale", "machines") => value(tailscale::machines().map_err(backend_error)?),
        ("tailscale", "up") => {
            tailscale::up().map_err(backend_error)?;
            Ok(json!({ "running": true }))
        }
        ("tailscale", "down") => {
            tailscale::down().map_err(backend_error)?;
            Ok(json!({ "running": false }))
        }
        ("tailscale", "pendingFiles") => value(tailscale::pending_files().map_err(backend_error)?),
        ("tailscale", "receive") => {
            let directory = param_str(params, "directory")?;
            value(tailscale::receive(directory).map_err(backend_error)?)
        }
        ("capture", "status") => value(capture::status()),
        ("capture", "screenshot") => {
            value(capture::screenshot(&capture_request(params)?).map_err(backend_error)?)
        }
        ("capture", "ocr") => {
            value(capture::ocr(&capture_request(params)?).map_err(backend_error)?)
        }
        ("capture", "record") => {
            value(capture::record(&capture_request(params)?).map_err(backend_error)?)
        }
        ("capture", "stopRecording") => {
            value(capture::stop_recording(param_bool(params, "notify")).map_err(backend_error)?)
        }
        ("capture", "recording") => value(capture::recording().map_err(backend_error)?),
        ("system", "power") => value(power::status()),
        ("system", "hardware") => value(hardware::status()),
        ("system", "firmware") => value(
            hardware::firmware(param_bool(params, "refresh").unwrap_or(false))
                .map_err(backend_error)?,
        ),
        ("system", "stayAwake") => value(awake::status()),
        ("system", "setStayAwake") => {
            let reason = params.get("reason").and_then(Value::as_str);
            let notify = param_bool(params, "notify").unwrap_or(true);
            value(awake::set(param_bool(params, "enabled"), reason, notify).map_err(backend_error)?)
        }
        ("nix", "status") => value(nix::status()),
        ("nix", "check") => value(nix::check().map_err(backend_error)?),
        ("nix", "update") => {
            let command = nix::update().map_err(backend_error)?;
            Ok(json!({ "started": true, "command": command }))
        }
        ("nix", "rebuild") => {
            let host = params.get("host").and_then(Value::as_str);
            let command = nix::rebuild(host).map_err(backend_error)?;
            Ok(json!({ "started": true, "command": command }))
        }
        ("nix", "hosts") => value(nix::hosts().map_err(backend_error)?),
        ("localsend", "devices") => value(localsend::devices().map_err(backend_error)?),
        ("localsend", "status") => Ok(match localsend::receiver() {
            Some(receiver) => json!({
                "receiving": true,
                "alias": receiver.alias(),
                "fingerprint": receiver.fingerprint(),
                "port": receiver.port(),
                "download_dir": receiver.download_dir().display().to_string(),
                "pending": receiver.pending().len(),
            }),
            None => json!({ "receiving": false }),
        }),
        ("localsend", "startReceiving") => {
            localsend::start_receiver().map_err(backend_error)?;
            let port = localsend::receiver().map(|r| r.port());
            Ok(json!({ "receiving": true, "port": port }))
        }
        ("localsend", "stopReceiving") => {
            let was_running = localsend::stop_receiver().map_err(backend_error)?;
            Ok(json!({ "receiving": false, "stopped": was_running }))
        }
        ("localsend", "pending") => value(receiving()?.pending()),
        ("localsend", "received") => value(receiving()?.received()),
        ("localsend", "accept") => {
            let session = param_str(params, "session")?;
            let directory = optional_directory(params, "directory")?;
            let saving_to = directory
                .clone()
                .unwrap_or_else(|| receiving().map(|r| r.download_dir()).unwrap_or_default());
            receiving()?
                .accept(session, directory)
                .map_err(backend_error)?;
            Ok(json!({ "accepted": true, "directory": saving_to.display().to_string() }))
        }
        ("localsend", "decline") => {
            receiving()?
                .decline(param_str(params, "session")?)
                .map_err(backend_error)?;
            Ok(json!({ "declined": true }))
        }
        ("localsend", "send") => {
            let device = param_str(params, "device")?;
            let files = param_strings(params, "files")?;
            value(localsend::send(device, &files).map_err(backend_error)?)
        }
        ("tailscale", "send") => {
            let peer = param_str(params, "peer")?;
            let files = param_strings(params, "files")?;
            tailscale::send(peer, &files).map_err(backend_error)?;
            Ok(json!({ "sent": files.len(), "peer": peer }))
        }
        _ => Err(ApiError::new(
            ErrorCode::UnknownMethod,
            format!("no method \"{method}\""),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_reports_every_group_with_a_status() {
        let described = describe();
        assert_eq!(described["version"], VERSION);
        let groups = described["groups"].as_array().expect("groups");
        assert_eq!(groups.len(), GROUPS.len());
        for group in groups {
            let status = group["status"].as_str().expect("status");
            assert!(
                ["available", "planned", "unavailable"].contains(&status),
                "unexpected status {status}"
            );
        }
    }

    #[test]
    fn declared_but_unimplemented_groups_are_refused_not_faked() {
        let err = dispatch("nix.flakeStatus", &Value::Null, None).expect_err("should refuse");
        assert_eq!(err.code, ErrorCode::Unavailable);
    }

    #[test]
    fn an_unknown_group_lists_the_real_ones() {
        let err = dispatch("bogus.thing", &Value::Null, None).expect_err("should refuse");
        assert_eq!(err.code, ErrorCode::UnknownGroup);
        let detail = err.detail.expect("detail");
        assert!(detail["groups"]
            .as_array()
            .expect("groups")
            .iter()
            .any(|group| group == "compositor"));
    }

    #[test]
    fn a_method_name_without_a_group_is_rejected() {
        let err = dispatch("windows", &Value::Null, None).expect_err("should refuse");
        assert_eq!(err.code, ErrorCode::UnknownMethod);
    }

    #[test]
    fn a_foreign_major_version_is_refused() {
        let err = dispatch("api.describe", &Value::Null, Some(VERSION_MAJOR + 1))
            .expect_err("should refuse");
        assert_eq!(err.code, ErrorCode::VersionMismatch);
    }

    #[test]
    fn describe_needs_no_version_and_no_compositor() {
        let described = dispatch("api.describe", &Value::Null, Some(VERSION_MAJOR)).expect("ok");
        assert!(described["groups"].is_array());
    }

    #[test]
    fn a_diagnostic_answers_even_when_its_group_cannot() {
        // capture.status reports which capture tools are missing, so gating it behind those tools
        // being present would leave nobody able to ask.
        for method in ALWAYS_ANSWERS {
            let (group_name, call) = method.split_once('.').expect("group.method");
            let found = group(group_name).expect("a real group");
            assert!(
                found.methods.iter().any(|m| m.name == call),
                "{method} is not in the contract"
            );
            assert!(dispatch(method, &Value::Null, None).is_ok());
        }
    }

    #[test]
    fn an_unknown_screenshot_mode_is_a_parameter_error() {
        // Availability depends on grim being installed, so only assert where the group is live.
        if !availability("capture").callable() {
            return;
        }
        let err = dispatch("capture.screenshot", &json!({ "mode": "panorama" }), None)
            .expect_err("should refuse");
        assert_eq!(err.code, ErrorCode::InvalidParams);
    }

    #[test]
    fn a_number_parameter_accepts_the_string_a_shell_hands_through() {
        assert_eq!(
            param_number(&json!({ "delay": 3 }), "delay").unwrap(),
            Some(3.0)
        );
        assert_eq!(
            param_number(&json!({ "delay": "2.5" }), "delay").unwrap(),
            Some(2.5)
        );
        assert_eq!(param_number(&json!({}), "delay").unwrap(), None);
        assert!(param_number(&json!({ "delay": "soon" }), "delay").is_err());
    }

    #[test]
    fn missing_params_are_named() {
        // compositor availability depends on the machine, so assert only on the params path when
        // the group is callable here.
        if !availability("compositor").callable() {
            return;
        }
        let err = dispatch("compositor.focusWindow", &json!({}), None).expect_err("should refuse");
        assert_eq!(err.code, ErrorCode::InvalidParams);
        assert!(err.message.contains("id"));
    }
}
