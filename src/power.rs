use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Power {
    pub available: bool,
    pub reason: Option<String>,
    pub profile: String,
    pub governor: String,
    pub governors: Vec<String>,
    pub energy_preference: String,
    pub energy_preferences: Vec<String>,
    pub turbo: Option<bool>,
    pub driver: String,
    pub manager: String,
    pub platform_profile: Option<String>,
    pub platform_profiles: Vec<String>,
    pub can_switch: bool,
}

const CPU0: &str = "/sys/devices/system/cpu/cpu0/cpufreq";
const PLATFORM_PROFILE: &str = "/sys/firmware/acpi/platform_profile";

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

fn manager() -> String {
    let Ok(output) = Command::new("systemctl")
        .arg("is-active")
        .args(MANAGERS)
        .output()
    else {
        return String::new();
    };
    // `is-active` exits non-zero when any unit is inactive, the normal case here, so the status is
    // ignored.
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
        assert_eq!(profile("powersave", "balance_performance"), "balanced");
        assert_eq!(profile("powersave", "default"), "balanced");
        assert_eq!(profile("powersave", "balance_power"), "power-saver");
        assert_eq!(profile("powersave", "power"), "power-saver");
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
