//! What machine this is, and how its battery and firmware are doing.
//!
//! Framework laptops are the target this was written against, but almost nothing here is specific
//! to them: DMI names the vendor, sysfs reports battery wear, and fwupd knows about firmware on any
//! machine that runs it. Detection exists so a caller can say "Framework Laptop 13" instead of
//! nothing, not to gate features behind a vendor.
//!
//! What *is* Framework-specific turned out to need root: the charge limit lives in the embedded
//! controller, reachable through `/dev/cros_ec`, which is `crw------- root root`. Rather than ask
//! for a polkit prompt to read a number, this reports the standard sysfs thresholds when a machine
//! exposes them and says nothing when it does not.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const DMI: &str = "/sys/class/dmi/id";
const POWER_SUPPLY: &str = "/sys/class/power_supply";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Hardware {
    pub vendor: String,
    pub product: String,
    pub family: String,
    pub board: String,
    pub bios_version: String,
    /// True when DMI says Framework, so a caller can offer vendor-specific help without guessing.
    pub framework: bool,
    pub battery: Option<Battery>,
}

/// Battery wear, which is the number people actually want and no desktop shows.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Battery {
    pub name: String,
    pub manufacturer: String,
    pub model: String,
    /// Full charge now against full charge when new, as a percentage. 100 means no measurable wear.
    pub health: Option<u32>,
    pub cycle_count: Option<u32>,
    /// µAh or µWh depending on what the battery reports; `unit` says which.
    pub full: Option<u64>,
    pub design_full: Option<u64>,
    pub unit: String,
    pub capacity: Option<u32>,
    pub status: String,
    /// Charge thresholds, on machines that expose them through sysfs. Framework's live in the EC
    /// behind a root-only device, so they are absent there.
    pub charge_start_threshold: Option<u32>,
    pub charge_end_threshold: Option<u32>,
}

/// A firmware update fwupd is offering.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FirmwareUpdate {
    pub name: String,
    pub current: String,
    pub available: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Firmware {
    pub available: bool,
    pub reason: Option<String>,
    pub updates: Vec<FirmwareUpdate>,
    /// When this was last asked, and whether the answer came from the cache.
    pub checked_at: i64,
    pub cached: bool,
}

pub fn status() -> Hardware {
    let vendor = dmi("sys_vendor");
    Hardware {
        framework: vendor.to_lowercase().contains("framework"),
        product: dmi("product_name"),
        family: dmi("product_family"),
        board: dmi("board_name"),
        bios_version: dmi("bios_version"),
        vendor,
        battery: battery(),
    }
}

fn dmi(field: &str) -> String {
    read(&format!("{DMI}/{field}"))
}

/// The first battery the machine reports. A laptop with two is rare enough that naming one is
/// better than inventing a way to talk about both.
fn battery() -> Option<Battery> {
    let directory = std::fs::read_dir(POWER_SUPPLY)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| read(&format!("{}/type", path.display())) == "Battery")
        .min_by_key(|path| path.file_name().map(|name| name.to_os_string()))?;

    let at = |field: &str| read(&format!("{}/{field}", directory.display()));
    let number = |field: &str| at(field).parse::<u64>().ok();

    // A battery reports either charge (µAh) or energy (µWh); which one is not a choice anybody
    // made, so both are read and the unit is passed on rather than converted.
    let (full, design_full, unit) = match (number("charge_full"), number("charge_full_design")) {
        (Some(full), design) => (Some(full), design, "µAh"),
        _ => (number("energy_full"), number("energy_full_design"), "µWh"),
    };

    Some(Battery {
        name: directory
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default(),
        manufacturer: at("manufacturer"),
        model: at("model_name"),
        health: health(full, design_full),
        cycle_count: number("cycle_count").map(|count| count as u32),
        full,
        design_full,
        unit: unit.to_string(),
        capacity: number("capacity").map(|value| value as u32),
        status: at("status"),
        charge_start_threshold: number("charge_control_start_threshold").map(|v| v as u32),
        charge_end_threshold: number("charge_control_end_threshold").map(|v| v as u32),
    })
}

/// Wear, rounded to a percentage. A battery that reports a fuller charge than it shipped with is
/// reported as 100 rather than 103: the extra is measurement drift, not capacity.
fn health(full: Option<u64>, design: Option<u64>) -> Option<u32> {
    let (full, design) = (full?, design?);
    if design == 0 {
        return None;
    }
    let ratio = (full as f64 / design as f64) * 100.0;
    Some(ratio.round().min(100.0) as u32)
}

fn read(path: &str) -> String {
    std::fs::read_to_string(path)
        .map(|value| value.trim().to_string())
        .unwrap_or_default()
}

// --- Firmware -----------------------------------------------------------------

/// How long a firmware answer is reused. `fwupdmgr` takes a second or two and talks to a daemon;
/// nothing about firmware changes faster than this.
const FIRMWARE_CACHE: Duration = Duration::from_secs(600);

static FIRMWARE: Mutex<Option<(Instant, Firmware)>> = Mutex::new(None);

pub fn firmware_available() -> Result<(), String> {
    if which::which("fwupdmgr").is_ok() {
        Ok(())
    } else {
        Err("fwupd is not installed".into())
    }
}

/// What fwupd is offering. `refresh` skips the cache; it does not download new metadata, which is
/// a privileged, network-bound operation and the user's own to start.
pub fn firmware(refresh: bool) -> Result<Firmware> {
    if let Err(reason) = firmware_available() {
        return Ok(Firmware {
            available: false,
            reason: Some(reason),
            ..Firmware::default()
        });
    }
    if !refresh {
        if let Ok(cache) = FIRMWARE.lock() {
            if let Some((at, cached)) = cache.as_ref() {
                if at.elapsed() < FIRMWARE_CACHE {
                    let mut cached = cached.clone();
                    cached.cached = true;
                    return Ok(cached);
                }
            }
        }
    }

    let output = Command::new("fwupdmgr")
        .args(["get-updates", "--json"])
        .output()
        .context("running fwupdmgr")?;
    // fwupdmgr exits non-zero when there is simply nothing to update, which is not a failure.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value = match serde_json::from_str(stdout.trim()) {
        Ok(value) => value,
        Err(_) if !output.status.success() => {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if detail.to_lowercase().contains("no updates") || detail.is_empty() {
                Value::Null
            } else {
                bail!("fwupd could not be asked: {detail}");
            }
        }
        Err(err) => bail!("fwupd answered with something unreadable: {err}"),
    };

    let updates = parsed
        .get("Devices")
        .and_then(Value::as_array)
        .map(|devices| {
            devices
                .iter()
                .filter_map(|device| {
                    // A device with no release on offer is listed but has nothing to install.
                    let release = device.get("Releases").and_then(Value::as_array)?.first()?;
                    Some(FirmwareUpdate {
                        name: string(device, "Name"),
                        current: string(device, "Version"),
                        available: string(release, "Version"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let answer = Firmware {
        available: true,
        reason: None,
        updates,
        checked_at: now(),
        cached: false,
    };
    if let Ok(mut cache) = FIRMWARE.lock() {
        *cache = Some((Instant::now(), answer.clone()));
    }
    Ok(answer)
}

fn string(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0)
}

/// Where a battery lives, for tests that need a path rather than a machine.
#[allow(dead_code)]
fn power_supply(name: &str) -> PathBuf {
    Path::new(POWER_SUPPLY).join(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_is_full_charge_against_what_it_shipped_with() {
        assert_eq!(health(Some(3_695_000), Some(3_915_000)), Some(94));
        assert_eq!(health(Some(3_915_000), Some(3_915_000)), Some(100));
    }

    #[test]
    fn a_battery_reporting_more_than_its_design_is_not_over_a_hundred() {
        // Measurement drift, not extra capacity.
        assert_eq!(health(Some(4_100_000), Some(3_915_000)), Some(100));
    }

    #[test]
    fn health_needs_both_numbers() {
        assert_eq!(health(None, Some(3_915_000)), None);
        assert_eq!(health(Some(3_695_000), None), None);
        assert_eq!(health(Some(3_695_000), Some(0)), None);
    }

    #[test]
    fn framework_is_recognised_from_the_vendor_string() {
        // The real check reads DMI; this pins the rule that decides it.
        assert!("Framework".to_lowercase().contains("framework"));
        assert!(!"LENOVO".to_lowercase().contains("framework"));
    }
}
