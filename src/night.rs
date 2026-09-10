//! Night mode: warm the screen and leave it warm.
//!
//! Held as a process, the way stay-awake and recording are: every tool that does this holds a
//! `wlr-gamma-control` object for as long as it runs and hands the screen back when it exits.
//! That makes turning it off a kill rather than another command, and means nothing can leave a
//! session tinted after the daemon goes away.
//!
//! Which tool is a detail: `hyprsunset` works on niri as well as Hyprland, because niri implements
//! the same gamma-control protocol, but `gammastep` and `wlsunset` are looked for too so a machine
//! with either is not left out.

use crate::config::Config;
use crate::notify;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Tools that can hold a colour temperature, in the order they are tried, with how each is asked
/// for one. `{k}` is the temperature in kelvin.
const TOOLS: &[(&str, &[&str])] = &[
    ("hyprsunset", &["-t", "{k}"]),
    ("gammastep", &["-O", "{k}"]),
    // wlsunset has no one-shot mode: giving it the same day and night temperature is how it is
    // held at one value.
    ("wlsunset", &["-t", "{k}", "-T", "{k}"]),
];

/// What a screen is set to when night mode is off. Nothing sets this; it is what the tools mean by
/// neutral, and is reported so a caller can show the difference.
const DAYLIGHT: u32 = 6500;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct NightLight {
    pub enabled: bool,
    /// Kelvin. Lower is warmer; this is the configured value whether or not it is being applied.
    pub temperature: u32,
    /// What neutral looks like, for a caller that wants to show both.
    pub daylight: u32,
    /// The tool holding the screen, empty when nothing is.
    pub tool: String,
    pub available: bool,
    pub unavailable_reason: Option<String>,
    pub since: i64,
    pub seconds: u64,
    pub notified: bool,
}

struct Held {
    child: Child,
    tool: String,
    temperature: u32,
    since: Instant,
    at: i64,
}

static HELD: Mutex<Option<Held>> = Mutex::new(None);
static TEMPERATURE: OnceLock<u32> = OnceLock::new();

pub fn configure(config: &Config) {
    let _ = TEMPERATURE.set(config.night_light_temperature);
}

fn configured_temperature() -> u32 {
    *TEMPERATURE
        .get()
        .unwrap_or(&Config::default().night_light_temperature)
}

/// The first tool installed, and the arguments it wants.
fn tool() -> Option<(&'static str, &'static [&'static str])> {
    TOOLS
        .iter()
        .find(|(name, _)| which::which(name).is_ok())
        .map(|(name, args)| (*name, *args))
}

pub fn available() -> Result<(), String> {
    if tool().is_some() {
        Ok(())
    } else {
        Err(format!(
            "none of {} is installed",
            TOOLS
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }
}

fn held() -> Result<std::sync::MutexGuard<'static, Option<Held>>> {
    HELD.lock()
        .map_err(|_| anyhow!("the night light lock is poisoned"))
}

/// Whether the screen is being warmed, and by what.
///
/// A tool that has died on its own is noticed here and cleared: the screen went back to normal when
/// it exited, so continuing to report night mode as on would be a lie a bar indicator repeats.
pub fn status() -> NightLight {
    let availability = available();
    let mut slot = match held() {
        Ok(slot) => slot,
        Err(_) => return NightLight::default(),
    };
    if let Some(active) = slot.as_mut() {
        if active.child.try_wait().ok().flatten().is_some() {
            *slot = None;
        }
    }
    let base = NightLight {
        daylight: DAYLIGHT,
        available: availability.is_ok(),
        unavailable_reason: availability.err(),
        ..NightLight::default()
    };
    match slot.as_ref() {
        Some(active) => NightLight {
            enabled: true,
            temperature: active.temperature,
            tool: active.tool.clone(),
            since: active.at,
            seconds: active.since.elapsed().as_secs(),
            ..base
        },
        None => NightLight {
            enabled: false,
            temperature: configured_temperature(),
            ..base
        },
    }
}

/// Turn night mode on or off. `enabled` of `None` toggles.
///
/// Asking for a different temperature while it is already on restarts the tool, because none of
/// them can be re-aimed once running.
pub fn set(
    enabled: Option<bool>,
    temperature: Option<u32>,
    notify_user: bool,
) -> Result<NightLight> {
    let Some((name, arguments)) = tool() else {
        bail!("{}", available().unwrap_err());
    };
    let current = status();
    let wanted = enabled.unwrap_or(!current.enabled);
    let temperature = validate(temperature.unwrap_or(configured_temperature()))?;

    if wanted && current.enabled && current.temperature == temperature {
        return Ok(current);
    }

    // Off first either way: turning it off is the whole job, and a temperature change is a restart.
    if let Some(mut active) = held()?.take() {
        let _ = active.child.kill();
        let _ = active.child.wait();
    }

    if wanted {
        let arguments: Vec<String> = arguments
            .iter()
            .map(|argument| argument.replace("{k}", &temperature.to_string()))
            .collect();
        let child = Command::new(name)
            .args(&arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("running {name}"))?;
        *held()? = Some(Held {
            child,
            tool: name.to_string(),
            temperature,
            since: Instant::now(),
            at: now(),
        });
    }

    let mut state = status();
    if notify_user {
        let (summary, body) = if state.enabled {
            ("Night mode on", format!("Screen warmed to {temperature}K"))
        } else {
            ("Night mode off", "Screen back to normal".to_string())
        };
        let icon = if state.enabled {
            "weather-clear-night"
        } else {
            "weather-clear"
        };
        state.notified = notify::send(summary, &body, icon, None, "epoch-night-light").is_ok();
    }
    Ok(state)
}

/// Kelvin a screen can actually be set to.
///
/// The range is what the tools accept; a number outside it is a typo -- 400 instead of 4000 -- and
/// refusing it is better than a screen that goes an alarming colour with no explanation.
fn validate(temperature: u32) -> Result<u32> {
    if !(1000..=20000).contains(&temperature) {
        bail!("{temperature}K is not a screen temperature (1000-20000)");
    }
    Ok(temperature)
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
    fn a_temperature_outside_what_a_screen_does_is_refused() {
        // 400 is 4000 with a missing zero, and it is worth catching rather than rendering.
        assert!(validate(400).is_err());
        assert!(validate(0).is_err());
        assert!(validate(50000).is_err());
        assert_eq!(validate(4000).unwrap(), 4000);
        assert_eq!(validate(1000).unwrap(), 1000);
        assert_eq!(validate(20000).unwrap(), 20000);
    }

    #[test]
    fn each_tool_is_told_the_temperature() {
        for (name, arguments) in TOOLS {
            let rendered: Vec<String> = arguments
                .iter()
                .map(|argument| argument.replace("{k}", "4000"))
                .collect();
            assert!(
                rendered.iter().any(|argument| argument == "4000"),
                "{name} is never given the temperature"
            );
        }
    }

    #[test]
    fn nothing_is_warming_the_screen_until_something_asks() {
        let idle = NightLight::default();
        assert!(!idle.enabled);
        assert_eq!(idle.seconds, 0);
        assert!(idle.tool.is_empty());
    }

    // Turning it on is not a unit test for the same reason stay-awake's is not: it would tint the
    // screen of whatever machine ran the suite, and a failure would leave it that way. That path is
    // exercised through `epochctl toggle night-light`.
}
