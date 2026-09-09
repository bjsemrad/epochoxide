//! Desktop notifications.
//!
//! One place that knows how EpochOxide talks to the session's notification server, which on an
//! EpochShell desktop is the shell itself. Everything goes through `notify-send`: the daemon has
//! no D-Bus client, and libnotify is a dependency the Nix modules already carry.
//!
//! Sending is always best-effort. A missing `notify-send` is a machine without libnotify, not a
//! failed screenshot or a failed update check, so callers are told whether the notification went
//! out rather than having their own work fail with it.

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

const NOTIFY_SEND: &str = "notify-send";

/// The app name every EpochOxide notification carries, so the shell can group them.
const APP_NAME: &str = "EpochShell";

pub fn available() -> bool {
    which::which(NOTIFY_SEND).is_ok()
}

/// One notification.
///
/// `icon` is a freedesktop icon name; `image` is a file the shell shows as a thumbnail, which is
/// what puts the actual screenshot on a capture notification. `tag` replaces an earlier
/// notification with the same tag instead of stacking a new one up, so a run of captures or a
/// re-check does not bury the screen.
pub fn send(summary: &str, body: &str, icon: &str, image: Option<&Path>, tag: &str) -> Result<()> {
    if !available() {
        bail!("{NOTIFY_SEND} is not installed");
    }
    let mut command = Command::new(NOTIFY_SEND);
    command
        .arg(format!("--app-name={APP_NAME}"))
        .arg(format!("--icon={icon}"));
    if let Some(image) = image {
        command.arg(format!("--hint=string:image-path:{}", image.display()));
    }
    if !tag.is_empty() {
        command.arg(format!(
            "--hint=string:x-canonical-private-synchronous:{tag}"
        ));
    }
    let status = command
        .arg(summary)
        .arg(body)
        .status()
        .with_context(|| format!("running {NOTIFY_SEND}"))?;
    if !status.success() {
        bail!("{NOTIFY_SEND} exited with {status}");
    }
    Ok(())
}
