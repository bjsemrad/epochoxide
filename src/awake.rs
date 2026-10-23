use crate::notify;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const WHAT: &str = "idle:sleep";
const WHO: &str = "EpochShell";
const DEFAULT_REASON: &str = "Stay awake";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct StayAwake {
    pub enabled: bool,
    pub reason: String,
    pub since: i64,
    pub seconds: u64,
    pub available: bool,
    pub unavailable_reason: Option<String>,
    pub notified: bool,
}

struct Inhibitor {
    child: Child,
    reason: String,
    since: Instant,
    at: i64,
}

static INHIBITOR: Mutex<Option<Inhibitor>> = Mutex::new(None);

pub fn available() -> Result<(), String> {
    if which::which("systemd-inhibit").is_ok() {
        Ok(())
    } else {
        Err("systemd-inhibit is not installed".into())
    }
}

fn held() -> Result<std::sync::MutexGuard<'static, Option<Inhibitor>>> {
    INHIBITOR
        .lock()
        .map_err(|_| anyhow!("the stay-awake lock is poisoned"))
}

pub fn status() -> StayAwake {
    let availability = available();
    let mut slot = match held() {
        Ok(slot) => slot,
        Err(_) => return StayAwake::default(),
    };
    if let Some(inhibitor) = slot.as_mut() {
        if inhibitor.child.try_wait().ok().flatten().is_some() {
            *slot = None;
        }
    }
    match slot.as_ref() {
        Some(inhibitor) => StayAwake {
            enabled: true,
            reason: inhibitor.reason.clone(),
            since: inhibitor.at,
            seconds: inhibitor.since.elapsed().as_secs(),
            available: availability.is_ok(),
            unavailable_reason: availability.err(),
            notified: false,
        },
        None => StayAwake {
            enabled: false,
            available: availability.is_ok(),
            unavailable_reason: availability.err(),
            ..StayAwake::default()
        },
    }
}

pub fn set(enabled: Option<bool>, reason: Option<&str>, notify_user: bool) -> Result<StayAwake> {
    if let Err(unavailable) = available() {
        bail!("{unavailable}");
    }
    let current = status().enabled;
    let wanted = enabled.unwrap_or(!current);
    if wanted == current {
        return Ok(status());
    }

    if wanted {
        let reason = reason
            .map(str::trim)
            .filter(|reason| !reason.is_empty())
            .unwrap_or(DEFAULT_REASON)
            .to_string();
        // systemd-inhibit holds the lock for as long as its child runs, so the child does nothing.
        let child = Command::new("systemd-inhibit")
            .arg(format!("--what={WHAT}"))
            .arg(format!("--who={WHO}"))
            .arg(format!("--why={reason}"))
            .arg("--mode=block")
            .args(["sleep", "infinity"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("running systemd-inhibit")?;
        *held()? = Some(Inhibitor {
            child,
            reason,
            since: Instant::now(),
            at: now(),
        });
    } else if let Some(mut inhibitor) = held()?.take() {
        let _ = inhibitor.child.kill();
        let _ = inhibitor.child.wait();
    }

    let mut state = status();
    if notify_user {
        let summary = if state.enabled {
            "Staying awake"
        } else {
            "Sleeping normally again"
        };
        let body = if state.enabled {
            format!("Idle and sleep are held off · {}", state.reason)
        } else {
            "Idle timers and sleep are back to normal".to_string()
        };
        let icon = if state.enabled {
            "preferences-desktop-screensaver"
        } else {
            "system-suspend"
        };
        state.notified = notify::send(summary, &body, icon, None, "epoch-stay-awake").is_ok();
    }
    Ok(state)
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_held_until_something_asks() {
        let idle = StayAwake::default();
        assert!(!idle.enabled);
        assert_eq!(idle.seconds, 0);
        assert_eq!(idle.since, 0);
    }

    #[test]
    fn the_inhibitor_blocks_both_idling_and_sleeping() {
        assert!(WHAT.contains("idle"));
        assert!(WHAT.contains("sleep"));
    }
}
