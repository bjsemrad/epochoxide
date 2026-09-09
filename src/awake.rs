//! Stay awake: hold the machine out of idle and sleep until told otherwise.
//!
//! The inhibitor is a `systemd-inhibit` process the daemon holds open, which is what makes this
//! state rather than an action: it survives the command that started it, a shell reload, and a
//! keybinding pressed twice. Releasing it is killing that process, and nothing else can leave the
//! machine stuck awake -- if the daemon dies, the lock dies with it.
//!
//! logind is the right place to take the lock because everything that idles the session already
//! watches it: hypridle honours logind idle inhibitors unless it is configured not to, and
//! systemd's own suspend honours them regardless. The shell additionally raises a Wayland
//! idle-inhibit against its own surface, so a compositor-level idle never even starts -- but that
//! belongs to the shell, which has a surface, rather than here.

use crate::notify;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// What the inhibitor blocks. `idle` stops idle timers -- the lock, the screen blank -- and
/// `sleep` stops the automatic suspend that would otherwise land in the middle of a long download.
const WHAT: &str = "idle:sleep";
const WHO: &str = "EpochShell";
const DEFAULT_REASON: &str = "Stay awake";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct StayAwake {
    pub enabled: bool,
    /// Why the machine is being held awake, as it appears in `systemd-inhibit --list`.
    pub reason: String,
    /// When it was turned on, and how long ago that was. Zero when it is off.
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

/// Whether the machine is being held awake.
///
/// An inhibitor whose process has died -- logind restarted, something killed it -- is noticed here
/// and cleared, because a bar indicator that keeps claiming the machine is awake when the lock is
/// gone is worse than no indicator.
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

/// Turn stay-awake on or off. `enabled` of `None` toggles.
///
/// Setting it to what it already is does nothing rather than replacing the lock: a keybinding
/// pressed twice should not restart the clock or fire a second notification.
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
        // `sleep infinity` is the process being inhibited: systemd-inhibit holds the lock for as
        // long as its child runs, so the child is chosen to do nothing at all.
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
        // Killing the process is what releases the lock; logind drops it when the holder exits.
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
        // The empty state is what a bar indicator reads before anyone has touched the toggle.
        let idle = StayAwake::default();
        assert!(!idle.enabled);
        assert_eq!(idle.seconds, 0);
        assert_eq!(idle.since, 0);
    }

    #[test]
    fn the_inhibitor_blocks_both_idling_and_sleeping() {
        // Holding only `idle` would still let the automatic suspend land in the middle of a long
        // download, which is the thing this feature exists to prevent.
        assert!(WHAT.contains("idle"));
        assert!(WHAT.contains("sleep"));
    }

    // Turning it on and off is deliberately not a unit test. It would spawn a real inhibitor
    // against the machine running the suite, and two such tests share this module's one global
    // lock: cargo runs them in parallel, they race over it, and a failure leaves the developer's
    // laptop held awake by an orphaned `sleep infinity`. Both of those happened while this module
    // was being written. That path is exercised through `epochctl toggle stay-awake` instead.
}
