use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

const NOTIFY_SEND: &str = "notify-send";

const APP_NAME: &str = "EpochShell";

pub fn available() -> bool {
    which::which(NOTIFY_SEND).is_ok()
}

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
