//! CPU power state.
//!
//! Read-only, and deliberately so. What a laptop is doing about power is worth showing; changing
//! it is a different problem, because whatever daemon is managing the CPU will simply put its own
//! decision back a few seconds later unless it is asked through its own override. Reporting needs
//! none of that.
//!
//! Everything here comes from sysfs, which means no daemon has to be installed and nothing needs
//! root. The managing daemon is identified only so a caller can say *who* is deciding -- the
//! numbers are true whether or not anything is managing them.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Command;

/// The normalized profile, and the raw knobs it was read from.
///
/// The raw fields are kept alongside the normalized name because they are what someone actually
/// debugging a warm laptop wants to see, and because "balanced" means nothing on its own.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Power {
    pub available: bool,
    pub reason: Option<String>,
    /// `performance`, `balanced`, `power-saver`, or empty when the knobs do not describe one.
    pub profile: String,
    pub governor: String,
    pub governors: Vec<String>,
    /// intel_pstate's energy/performance preference, empty where the driver has none.
    pub energy_preference: String,
    pub energy_preferences: Vec<String>,
    /// Whether turbo/boost is allowed. `None` when the machine exposes no such knob.
    pub turbo: Option<bool>,
    pub driver: String,
    /// The daemon deciding all this: `auto-cpufreq`, `power-profiles-daemon`, `tuned`, or empty
    /// when nothing is managing the CPU.
    pub manager: String,
    /// The ACPI platform profile, on machines that expose one.
    pub platform_profile: Option<String>,
    pub platform_profiles: Vec<String>,
    /// Always false in this build: nothing here changes anything.
    pub can_switch: bool,
}

const CPU0: &str = "/sys/devices/system/cpu/cpu0/cpufreq";
const PLATFORM_PROFILE: &str = "/sys/firmware/acpi/platform_profile";

/// Daemons that manage CPU frequency, in the order they are reported when several are somehow
/// running at once.
const MANAGERS: &[&str] = &["auto-cpufreq", "power-profiles-daemon", "tuned"];

pub fn available() -> Result<(), String> {
    if Path::new(CPU0).is_dir() {
        Ok(())
    } else {
        Err("this machine exposes no cpufreq state".into())
    }
}

pub fn status() -> Power {
    if let Err(reason) = available() {
        return Power {
            available: false,
            reason: Some(reason),
            ..Power::default()
        };
    }

    let governor = read(&format!("{CPU0}/scaling_governor"));
    let energy_preference = read(&format!("{CPU0}/energy_performance_preference"));
    Power {
        available: true,
        reason: None,
        profile: profile(&governor, &energy_preference),
        governors: words(&format!("{CPU0}/scaling_available_governors")),
        energy_preferences: words(&format!("{CPU0}/energy_performance_available_preferences")),
        turbo: turbo(),
        driver: read(&format!("{CPU0}/scaling_driver")),
        manager: manager(),
        platform_profile: Some(read(PLATFORM_PROFILE)).filter(|value| !value.is_empty()),
        platform_profiles: words(&format!("{PLATFORM_PROFILE}_choices")),
        can_switch: false,
        governor,
        energy_preference,
    }
}

/// Reduce the governor and the energy preference to one word.
///
/// The two together are what a person means by "power profile": `performance` says the CPU will
/// not clock down, while `powersave` is the governor every laptop idles at and says nothing on its
/// own -- what separates balanced from frugal there is the energy preference.
fn profile(governor: &str, energy_preference: &str) -> String {
    match (governor, energy_preference) {
        ("performance", _) => "performance".into(),
        ("powersave", "performance") => "performance".into(),
        ("powersave", "power" | "balance_power") => "power-saver".into(),
        ("powersave", _) => "balanced".into(),
        ("conservative", _) => "power-saver".into(),
        ("ondemand" | "schedutil", _) => "balanced".into(),
        _ => String::new(),
    }
}

/// Whether turbo is allowed. Intel spells it as an inversion, everyone else as a boost flag.
fn turbo() -> Option<bool> {
    let intel = read("/sys/devices/system/cpu/intel_pstate/no_turbo");
    if !intel.is_empty() {
        return Some(intel != "1");
    }
    let boost = read("/sys/devices/system/cpu/cpufreq/boost");
    if !boost.is_empty() {
        return Some(boost == "1");
    }
    None
}

/// Which daemon is managing the CPU, if any. One systemctl call answers for all of them.
fn manager() -> String {
    let Ok(output) = Command::new("systemctl")
        .arg("is-active")
        .args(MANAGERS)
        .output()
    else {
        return String::new();
    };
    // `is-active` prints one line per unit, in the order they were asked for, and exits non-zero
    // when any of them is inactive -- which is the normal case here, so the status is ignored.
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .zip(MANAGERS)
        .find(|(state, _)| state.trim() == "active")
        .map(|(_, name)| (*name).to_string())
        .unwrap_or_default()
}

fn read(path: &str) -> String {
    std::fs::read_to_string(path)
        .map(|value| value.trim().to_string())
        .unwrap_or_default()
}

fn words(path: &str) -> Vec<String> {
    read(path).split_whitespace().map(str::to_string).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn performance_is_the_governor_that_will_not_clock_down() {
        assert_eq!(profile("performance", "performance"), "performance");
        assert_eq!(profile("performance", ""), "performance");
    }

    #[test]
    fn powersave_is_decided_by_the_energy_preference() {
        // Every laptop idles at the powersave governor; on its own it says nothing about intent.
        assert_eq!(profile("powersave", "balance_performance"), "balanced");
        assert_eq!(profile("powersave", "default"), "balanced");
        assert_eq!(profile("powersave", "balance_power"), "power-saver");
        assert_eq!(profile("powersave", "power"), "power-saver");
        // A driver pinned to maximum through the preference is not saving anything.
        assert_eq!(profile("powersave", "performance"), "performance");
    }

    #[test]
    fn a_governor_nobody_here_knows_is_left_unnamed() {
        assert_eq!(profile("userspace", ""), "");
        assert_eq!(profile("", ""), "");
    }

    #[test]
    fn schedutil_and_ondemand_are_balanced() {
        assert_eq!(profile("schedutil", ""), "balanced");
        assert_eq!(profile("ondemand", ""), "balanced");
    }
}
