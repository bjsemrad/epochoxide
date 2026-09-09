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

use crate::{compositor, tailscale};
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

fn backend_error(err: impl std::fmt::Display) -> ApiError {
    ApiError::new(ErrorCode::BackendError, err.to_string())
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
    method(
        "send",
        "Send files to a peer with Taildrop",
        &[
            ("peer", "string, a name from tailscale.machines"),
            ("files", "array of absolute paths"),
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
    // Declared so the contract shape is stable and callers can feature-detect, but nothing here
    // is implemented yet. Each lands with its own build-out step rather than as a stub.
    Group {
        name: "capture",
        summary: "Screenshots, OCR, colour picking, and recording",
        methods: &[],
    },
    Group {
        name: "localsend",
        summary: "LocalSend device discovery and transfers",
        methods: &[],
    },
    Group {
        name: "dev",
        summary: "Project discovery and developer workflows",
        methods: &[],
    },
    Group {
        name: "nix",
        summary: "Flake update status and rebuilds",
        methods: &[],
    },
    Group {
        name: "system",
        summary: "Power profiles and system state",
        methods: &[],
    },
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
    if !status.callable() {
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
