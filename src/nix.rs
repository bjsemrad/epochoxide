//! Nix flake update awareness.
//!
//! Answers one question -- "can my system be updated?" -- without ever changing anything to find
//! out. Three rules shape the whole module:
//!
//! 1. **Nothing is written to the user's flake.** Checking resolves every input to what it would
//!    lock to today and writes that candidate lock to a temp file through
//!    `nix flake update --output-lock-file`, leaving `flake.lock` exactly as it was. Comparing the
//!    two locks is the whole check. A copy of the flake was the other option and is worse: it goes
//!    stale, and a `path:` input inside it stops resolving.
//! 2. **Updating is the user's action, never ours.** `update` and `rebuild` run commands the user
//!    configured, in their terminal, only when asked. Neither has a default that changes a system.
//! 3. **The answer is cached.** A check costs a network round trip per input, so the shell asks
//!    for `status` -- which answers from memory -- and a timer does the checking.

use crate::config::Config;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A host in the flake, and the command that rebuilds it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Host {
    pub name: String,
    /// The command that rebuilds this host, already resolved from config. Empty means nothing is
    /// configured for it, and the shell should not offer a rebuild that would do nothing.
    pub rebuild: String,
    /// True when the host was named in config rather than read out of the flake.
    pub configured: bool,
}

/// One flake input, as it is pinned now and as it would be pinned after an update.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Input {
    pub name: String,
    /// `github`, `git`, `tarball`, and so on, as the lock records it.
    pub kind: String,
    /// Where the input comes from, in the form the flake asked for it.
    pub source: String,
    pub current_rev: String,
    pub current_date: i64,
    pub latest_rev: String,
    pub latest_date: i64,
    pub update_available: bool,
}

/// What the last check found, plus everything the shell needs to render a panel.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Status {
    /// The configured flake, empty when the feature is off.
    pub flake: String,
    pub configured: bool,
    /// False when nix is missing, the path is not a flake, or nothing is configured.
    pub available: bool,
    pub reason: Option<String>,
    /// When `flake.lock` was last written, which is when the system was last updated.
    pub locked_at: i64,
    /// When the last check ran. Zero means it has not run yet.
    pub checked_at: i64,
    /// True while a check is running, so a panel can say so rather than look stuck.
    pub checking: bool,
    /// How many inputs have something newer.
    pub updates: usize,
    pub inputs: Vec<Input>,
    /// What the last check failed with, if it failed.
    pub error: Option<String>,
    pub update_command: String,
    pub rebuild_command: String,
    /// The hosts this flake defines, from the last check. Every host shares one `flake.lock`, so
    /// updates are flake-wide; what differs per host is which rebuild command to run.
    pub hosts: Vec<Host>,
    /// Minutes between automatic checks; zero when they are off.
    pub interval_minutes: u64,
}

#[derive(Debug, Clone)]
struct Settings {
    flake: PathBuf,
    interval: Duration,
    update_command: String,
    rebuild_command: String,
    hosts: Vec<crate::config::NixHost>,
    terminal: String,
    notify: bool,
}

impl Default for Settings {
    fn default() -> Self {
        let config = Config::default();
        Self::from_config(&config)
    }
}

impl Settings {
    fn from_config(config: &Config) -> Self {
        Self {
            flake: PathBuf::from(crate::config::expand(&config.nix_flake)),
            interval: Duration::from_secs(config.nix_check_interval_minutes * 60),
            update_command: config.nix_update_command.clone(),
            rebuild_command: config.nix_rebuild_command.clone(),
            hosts: config.nix_hosts.clone(),
            terminal: config.terminal_cmd.clone(),
            notify: config.nix_notify,
        }
    }

    fn configured(&self) -> bool {
        !self.flake.as_os_str().is_empty()
    }
}

static SETTINGS: OnceLock<Settings> = OnceLock::new();

/// The last completed check, and whether one is running now.
static STATE: Mutex<Option<Checked>> = Mutex::new(None);
static CHECKING: Mutex<bool> = Mutex::new(false);

#[derive(Debug, Clone)]
struct Checked {
    at: i64,
    inputs: Vec<Input>,
    hosts: Vec<Host>,
    error: Option<String>,
}

pub fn configure(config: &Config) {
    let _ = SETTINGS.set(Settings::from_config(config));
}

fn settings() -> Settings {
    SETTINGS.get().cloned().unwrap_or_default()
}

/// Whether update checking can work here, and why not when it cannot.
pub fn available() -> Result<(), String> {
    let settings = settings();
    if !settings.configured() {
        return Err("no flake configured (set nix_flake)".into());
    }
    if which::which("nix").is_err() {
        return Err("nix is not installed".into());
    }
    if !settings.flake.join("flake.nix").is_file() {
        return Err(format!(
            "{} is not a flake (no flake.nix)",
            settings.flake.display()
        ));
    }
    Ok(())
}

/// The cached answer. Cheap: no network, no nix, no writes.
pub fn status() -> Status {
    let settings = settings();
    let checked = STATE.lock().ok().and_then(|state| state.clone());
    let checking = CHECKING.lock().map(|flag| *flag).unwrap_or(false);
    let availability = available();
    Status {
        flake: settings.flake.display().to_string(),
        configured: settings.configured(),
        available: availability.is_ok(),
        reason: availability.err(),
        locked_at: lock_modified(&settings.flake),
        checked_at: checked.as_ref().map(|c| c.at).unwrap_or(0),
        checking,
        updates: checked
            .as_ref()
            .map(|c| c.inputs.iter().filter(|i| i.update_available).count())
            .unwrap_or(0),
        inputs: checked
            .as_ref()
            .map(|c| c.inputs.clone())
            .unwrap_or_default(),
        hosts: checked
            .as_ref()
            .map(|c| c.hosts.clone())
            .filter(|hosts| !hosts.is_empty())
            .unwrap_or_else(|| configured_hosts(&settings)),
        error: checked.and_then(|c| c.error),
        update_command: settings.update_command,
        rebuild_command: settings.rebuild_command,
        interval_minutes: settings.interval.as_secs() / 60,
    }
}

/// Resolve every input to what it would lock to today and compare.
///
/// Slow -- a network round trip per input -- and safe to run at any time: the only thing written
/// is a lock file in a temp directory, which is deleted before this returns.
pub fn check() -> Result<Status> {
    if let Err(reason) = available() {
        bail!("{reason}");
    }
    // Two checks at once would do the same work twice and race over the result.
    {
        let mut checking = CHECKING
            .lock()
            .map_err(|_| anyhow!("the nix check lock is poisoned"))?;
        if *checking {
            bail!("a check is already running");
        }
        *checking = true;
    }
    let outcome = run_check();
    if let Ok(mut checking) = CHECKING.lock() {
        *checking = false;
    }

    let (inputs, error) = match outcome {
        Ok(inputs) => (inputs, None),
        Err(err) => (Vec::new(), Some(format!("{err:#}"))),
    };
    // Hosts come along with the check because they are answered from the same flake and change
    // about as often; a failure to list them is not a failed check.
    let hosts = hosts().unwrap_or_default();
    let previous = STATE.lock().ok().and_then(|state| state.clone());
    if let Ok(mut state) = STATE.lock() {
        *state = Some(Checked {
            at: now(),
            inputs: inputs.clone(),
            hosts,
            error: error.clone(),
        });
    }
    announce(previous.as_ref(), &inputs);
    Ok(status())
}

fn run_check() -> Result<Vec<Input>> {
    let settings = settings();
    let current = read_lock(&settings.flake.join("flake.lock"))
        .context("reading the flake's current lock")?;

    // The candidate lock goes to a temp directory: `--output-lock-file` is what keeps the user's
    // own flake.lock untouched while still resolving through nix rather than guessing.
    let scratch = std::env::temp_dir().join(format!("epochoxide-flake-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).with_context(|| format!("creating {}", scratch.display()))?;
    let candidate_path = scratch.join("candidate.lock");

    let output = Command::new("nix")
        .args(["flake", "update", "--refresh", "--no-warn-dirty"])
        .arg("--flake")
        .arg(&settings.flake)
        .arg("--output-lock-file")
        .arg(&candidate_path)
        .output()
        .context("running nix flake update")?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let _ = std::fs::remove_dir_all(&scratch);
        bail!(
            "nix could not resolve the flake's inputs: {}",
            last_line(&detail)
        );
    }

    let candidate = read_lock(&candidate_path).context("reading the candidate lock");
    let _ = std::fs::remove_dir_all(&scratch);
    let candidate = candidate?;
    Ok(compare(&current, &candidate))
}

/// A flake.lock, reduced to what the root flake actually asked for.
///
/// Only the root's own inputs are considered: they are what a user updates and what a version
/// number means to them. A transitive input moving on its own is invisible to `nix flake update`
/// at this level too.
#[derive(Debug, Clone, PartialEq)]
struct Lock {
    inputs: Vec<LockedInput>,
}

#[derive(Debug, Clone, PartialEq)]
struct LockedInput {
    name: String,
    kind: String,
    source: String,
    rev: String,
    date: i64,
}

fn read_lock(path: &Path) -> Result<Lock> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let value: Value =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    Ok(parse_lock(&value))
}

fn parse_lock(value: &Value) -> Lock {
    let nodes = value.get("nodes").and_then(Value::as_object);
    let root_name = value.get("root").and_then(Value::as_str).unwrap_or("root");
    let Some(nodes) = nodes else {
        return Lock { inputs: Vec::new() };
    };
    let root = nodes.get(root_name).and_then(|node| node.get("inputs"));
    let Some(root) = root.and_then(Value::as_object) else {
        return Lock { inputs: Vec::new() };
    };

    let mut inputs: Vec<LockedInput> = root
        .iter()
        .filter_map(|(name, target)| {
            // An input written as ["a", "b"] is a `follows`, which has no revision of its own.
            let id = target.as_str()?;
            let node = nodes.get(id)?;
            let locked = node.get("locked")?;
            Some(LockedInput {
                name: name.clone(),
                kind: string(locked, "type"),
                source: describe(node.get("original").unwrap_or(locked)),
                rev: string(locked, "rev"),
                date: locked
                    .get("lastModified")
                    .and_then(Value::as_i64)
                    .unwrap_or(0),
            })
        })
        .collect();
    inputs.sort_by(|a, b| a.name.cmp(&b.name));
    Lock { inputs }
}

/// A flake reference in the shape a person would have typed it.
fn describe(original: &Value) -> String {
    let kind = string(original, "type");
    let reference = |base: String| {
        let git_ref = string(original, "ref");
        let rev = string(original, "rev");
        match (git_ref.is_empty(), rev.is_empty()) {
            (false, _) => format!("{base}/{git_ref}"),
            (true, false) => format!("{base}/{rev}"),
            _ => base,
        }
    };
    match kind.as_str() {
        "github" | "gitlab" | "sourcehut" => reference(format!(
            "{kind}:{}/{}",
            string(original, "owner"),
            string(original, "repo")
        )),
        "git" => {
            let url = string(original, "url");
            let git_ref = string(original, "ref");
            if git_ref.is_empty() {
                url
            } else {
                format!("{url} ({git_ref})")
            }
        }
        "path" => string(original, "path"),
        "indirect" => string(original, "id"),
        _ => {
            let url = string(original, "url");
            if url.is_empty() {
                kind
            } else {
                url
            }
        }
    }
}

fn string(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Pair each current input with its candidate. An input the update would drop, or one it would
/// add, is not an update to anything and is left out.
fn compare(current: &Lock, candidate: &Lock) -> Vec<Input> {
    current
        .inputs
        .iter()
        .map(|input| {
            let latest = candidate
                .inputs
                .iter()
                .find(|other| other.name == input.name);
            let latest_rev = latest.map(|l| l.rev.clone()).unwrap_or_default();
            let latest_date = latest.map(|l| l.date).unwrap_or(0);
            // Revisions are the truth; dates only order them. An input with no revision at all --
            // a tarball, say -- is compared by date instead.
            let update_available = if !input.rev.is_empty() && !latest_rev.is_empty() {
                input.rev != latest_rev
            } else {
                latest_date > input.date
            };
            Input {
                name: input.name.clone(),
                kind: input.kind.clone(),
                source: input.source.clone(),
                current_rev: input.rev.clone(),
                current_date: input.date,
                latest_rev,
                latest_date,
                update_available,
            }
        })
        .collect()
}

/// Tell the user when something new turned up, and only then.
///
/// A check runs every hour; saying "23 updates available" every hour is how a notifier teaches
/// people to ignore it. This fires when an input has something newer that it did not have at the
/// last check.
fn announce(previous: Option<&Checked>, inputs: &[Input]) {
    let settings = settings();
    if !settings.notify || !crate::notify::available() {
        return;
    }
    let was: Vec<&str> = previous
        .map(|checked| {
            checked
                .inputs
                .iter()
                .filter(|input| input.update_available)
                .map(|input| input.name.as_str())
                .collect()
        })
        .unwrap_or_default();
    let now: Vec<&str> = inputs
        .iter()
        .filter(|input| input.update_available)
        .map(|input| input.name.as_str())
        .collect();
    let fresh: Vec<&str> = now
        .iter()
        .filter(|name| !was.contains(name))
        .copied()
        .collect();
    if fresh.is_empty() {
        return;
    }
    let summary = if now.len() == 1 {
        "1 flake input can be updated".to_string()
    } else {
        format!("{} flake inputs can be updated", now.len())
    };
    let body = if fresh.len() > 4 {
        format!("{}, and {} more", fresh[..4].join(", "), fresh.len() - 4)
    } else {
        fresh.join(", ")
    };
    let _ = crate::notify::send(&summary, &body, "system-software-update", None, "epoch-nix");
}

/// The hosts this flake defines, and how each is rebuilt.
///
/// Configured hosts come first and are authoritative: a rebuild is usually an alias or a script
/// that already knows its target, so the command is stored per host rather than derived from one
/// template. Anything the flake defines that config did not name is listed too, falling back to
/// `nix_rebuild_command` with `%HOST%` substituted -- and left without a command when there is no
/// fallback either, so the shell can offer only what will actually run.
///
/// Worth being clear about what a host is here: every host in a flake shares one `flake.lock`, so
/// "which hosts have updates" has the same answer for all of them. What differs is which rebuild
/// to run, which is why hosts sit next to the rebuild action rather than next to the inputs.
pub fn hosts() -> Result<Vec<Host>> {
    let settings = settings();
    let mut hosts = configured_hosts(&settings);
    if let Err(reason) = available() {
        // Configured hosts are still worth answering with: they need no flake to be listed.
        if hosts.is_empty() {
            bail!("{reason}");
        }
        return Ok(hosts);
    }
    for name in discover(&settings)? {
        if hosts.iter().any(|host| host.name == name) {
            continue;
        }
        hosts.push(Host {
            rebuild: fallback_command(&settings, &name),
            name,
            configured: false,
        });
    }
    Ok(hosts)
}

fn configured_hosts(settings: &Settings) -> Vec<Host> {
    settings
        .hosts
        .iter()
        .filter(|host| !host.name.trim().is_empty())
        .map(|host| Host {
            rebuild: if host.rebuild.trim().is_empty() {
                fallback_command(settings, &host.name)
            } else {
                host.rebuild.clone()
            },
            name: host.name.clone(),
            configured: true,
        })
        .collect()
}

/// `nix_rebuild_command` with `%HOST%` filled in, or nothing when none is configured.
fn fallback_command(settings: &Settings, host: &str) -> String {
    if settings.rebuild_command.trim().is_empty() {
        return String::new();
    }
    settings.rebuild_command.replace(HOST_PLACEHOLDER, host)
}

/// Ask the flake which hosts it defines. Only attribute names are evaluated, so nothing is built.
fn discover(settings: &Settings) -> Result<Vec<String>> {
    let output = Command::new("nix")
        .args([
            "eval",
            "--json",
            "--no-write-lock-file",
            "--no-warn-dirty",
            "--apply",
            "builtins.attrNames",
        ])
        .arg(format!("{}#nixosConfigurations", settings.flake.display()))
        .output()
        .context("running nix eval")?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        // A flake with no nixosConfigurations is a normal flake, not a broken one.
        if detail.contains("does not provide attribute") {
            return Ok(Vec::new());
        }
        bail!(
            "nix could not list the flake's hosts: {}",
            last_line(&detail)
        );
    }
    serde_json::from_slice(&output.stdout).context("reading the host list nix printed")
}

/// Run the configured update command in the user's terminal.
///
/// Nothing here has a default that changes a system: an unconfigured command is refused rather
/// than guessed at, because guessing wrong means running the wrong rebuild on someone's machine.
pub fn update() -> Result<String> {
    let settings = settings();
    run_in_terminal(&settings, &settings.update_command, "nix_update_command")
}

/// Rebuild, optionally naming which host to rebuild.
///
/// The command comes from that host's own configuration, so an alias that already targets a
/// machine is run as written rather than rewritten from a template.
pub fn rebuild(host: Option<&str>) -> Result<String> {
    let settings = settings();
    let wanted = host.unwrap_or("").trim().to_string();
    let known = hosts().unwrap_or_default();

    let chosen = if wanted.is_empty() {
        // One host is unambiguous; several are not, and picking one for the user is how the wrong
        // machine gets rebuilt.
        match known.as_slice() {
            [only] => only.clone(),
            [] => Host {
                name: String::new(),
                rebuild: settings.rebuild_command.clone(),
                configured: false,
            },
            many => bail!(
                "which host? this flake knows {}",
                many.iter()
                    .map(|host| host.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    } else {
        known
            .iter()
            .find(|host| host.name == wanted)
            .cloned()
            .ok_or_else(|| {
                anyhow!(
                    "no host \"{wanted}\" ({})",
                    if known.is_empty() {
                        "this flake defines none".to_string()
                    } else {
                        known
                            .iter()
                            .map(|host| host.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                )
            })?
    };

    if chosen.rebuild.trim().is_empty() {
        bail!(
            "no rebuild command for {} (set nix_rebuild_command, or a rebuild for it in nix_hosts)",
            if chosen.name.is_empty() {
                "this flake"
            } else {
                &chosen.name
            }
        );
    }
    run_in_terminal(&settings, &chosen.rebuild, "nix_rebuild_command")
}

/// What a rebuild command writes where the host name goes.
const HOST_PLACEHOLDER: &str = "%HOST%";

fn run_in_terminal(settings: &Settings, command: &str, key: &str) -> Result<String> {
    if command.trim().is_empty() {
        bail!("no {key} is configured");
    }
    if !settings.configured() {
        bail!("no flake configured (set nix_flake)");
    }
    let terminal = if settings.terminal.trim().is_empty() {
        default_terminal().ok_or_else(|| {
            anyhow!("no terminal found; set terminal_cmd to the terminal to run this in")
        })?
    } else {
        settings.terminal.clone()
    };

    // The command runs in the flake's directory, which is what makes `--flake .#host` and a bare
    // `nix flake update` mean what the user expects. It is then held open: a rebuild takes minutes
    // and prints the only record of what it did, and a terminal that vanishes on the last line
    // takes that with it.
    let script = format!(
        "cd {} && {command}\nstatus=$?\nprintf '\\n[exited %s] press enter to close ' \"$status\"\nread _",
        shell_quote(&settings.flake)
    );
    // Through the user's own shell, interactively, because a rebuild command is usually an alias
    // -- `nixswitch`, `rebuild-thor` -- and aliases live in the shell's rc file. `sh -c` would
    // report "command not found" for something the user runs by hand every day.
    let shell = user_shell();
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "{terminal} {shell} -i -c {}",
            shell_quote_str(&script)
        ))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("running {terminal}"))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(command.to_string())
}

/// The user's login shell, which is where their aliases are defined.
fn user_shell() -> String {
    if let Ok(shell) = std::env::var("SHELL") {
        if !shell.trim().is_empty() {
            return shell;
        }
    }
    // A systemd user service does not always inherit SHELL, so passwd is the fallback.
    let user = std::env::var("USER").unwrap_or_default();
    if !user.is_empty() {
        if let Some(line) = Command::new("getent")
            .args(["passwd", &user])
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        {
            if let Some(shell) = line.rsplit(':').next() {
                if !shell.is_empty() {
                    return shell.to_string();
                }
            }
        }
    }
    "sh".to_string()
}

fn default_terminal() -> Option<String> {
    for candidate in ["ghostty", "kitty", "alacritty", "foot", "wezterm", "xterm"] {
        if which::which(candidate).is_ok() {
            // Every one of these takes the command after -e.
            return Some(format!("{candidate} -e"));
        }
    }
    None
}

fn shell_quote(path: &Path) -> String {
    shell_quote_str(&path.display().to_string())
}

fn shell_quote_str(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Start the background checker. Does nothing when no flake is configured or the interval is zero.
pub fn watch() {
    let settings = settings();
    if !settings.configured() || settings.interval.is_zero() {
        return;
    }
    std::thread::spawn(move || {
        // A check at startup would compete with everything else a session does when it starts, and
        // the answer is not urgent: the first one happens a minute in.
        std::thread::sleep(Duration::from_secs(60));
        loop {
            if let Err(err) = check() {
                eprintln!("nix: update check failed: {err:#}");
            }
            std::thread::sleep(settings.interval);
        }
    });
}

fn lock_modified(flake: &Path) -> i64 {
    std::fs::metadata(flake.join("flake.lock"))
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0)
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0)
}

fn last_line(text: &str) -> String {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("nix said nothing")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lock(nixpkgs_rev: &str, home_rev: &str) -> Value {
        json!({
            "version": 7,
            "root": "root",
            "nodes": {
                "root": { "inputs": { "nixpkgs": "nixpkgs", "home-manager": "home-manager", "self": "self" } },
                "nixpkgs": {
                    "locked": { "type": "github", "owner": "NixOS", "repo": "nixpkgs", "rev": nixpkgs_rev, "lastModified": 1788881743 },
                    "original": { "type": "github", "owner": "NixOS", "repo": "nixpkgs", "ref": "nixos-unstable" }
                },
                "home-manager": {
                    "locked": { "type": "git", "url": "https://github.com/nix-community/home-manager", "rev": home_rev, "lastModified": 1788000000 },
                    "original": { "type": "git", "url": "https://github.com/nix-community/home-manager" }
                },
                "self": { "locked": { "type": "path" } }
            }
        })
    }

    #[test]
    fn only_the_roots_own_inputs_are_read() {
        let parsed = parse_lock(&lock("aaa", "bbb"));
        let names: Vec<&str> = parsed.inputs.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["home-manager", "nixpkgs", "self"]);
    }

    #[test]
    fn a_follows_input_has_no_revision_to_compare() {
        // `inputs.foo.follows = "nixpkgs"` is recorded as an array, not a node id.
        let mut value = lock("aaa", "bbb");
        value["nodes"]["root"]["inputs"]["foo"] = json!(["nixpkgs"]);
        let parsed = parse_lock(&value);
        assert!(parsed.inputs.iter().all(|input| input.name != "foo"));
    }

    #[test]
    fn a_moved_revision_is_an_update() {
        let current = parse_lock(&lock("aaa", "bbb"));
        let candidate = parse_lock(&lock("ccc", "bbb"));
        let compared = compare(&current, &candidate);
        let nixpkgs = compared
            .iter()
            .find(|i| i.name == "nixpkgs")
            .expect("nixpkgs");
        assert!(nixpkgs.update_available);
        assert_eq!(nixpkgs.latest_rev, "ccc");
        let home = compared
            .iter()
            .find(|i| i.name == "home-manager")
            .expect("home-manager");
        assert!(!home.update_available);
    }

    #[test]
    fn an_input_the_candidate_does_not_have_is_not_an_update() {
        // A dropped input has nothing newer to move to, and reporting it as updatable would send
        // the user to run an update that changes nothing.
        let current = parse_lock(&lock("aaa", "bbb"));
        let mut dropped = lock("aaa", "bbb");
        dropped["nodes"]["root"]["inputs"]
            .as_object_mut()
            .expect("inputs")
            .remove("nixpkgs");
        let compared = compare(&current, &parse_lock(&dropped));
        let nixpkgs = compared
            .iter()
            .find(|i| i.name == "nixpkgs")
            .expect("nixpkgs");
        assert!(!nixpkgs.update_available);
        assert_eq!(nixpkgs.latest_rev, "");
    }

    #[test]
    fn an_input_with_no_revision_is_compared_by_date() {
        let mut current = lock("aaa", "bbb");
        current["nodes"]["nixpkgs"]["locked"] = json!({ "type": "tarball", "lastModified": 100 });
        let mut candidate = lock("aaa", "bbb");
        candidate["nodes"]["nixpkgs"]["locked"] = json!({ "type": "tarball", "lastModified": 200 });
        let compared = compare(&parse_lock(&current), &parse_lock(&candidate));
        assert!(
            compared
                .iter()
                .find(|i| i.name == "nixpkgs")
                .expect("nixpkgs")
                .update_available
        );
    }

    fn host(name: &str, rebuild: &str) -> crate::config::NixHost {
        crate::config::NixHost {
            name: name.into(),
            rebuild: rebuild.into(),
        }
    }

    fn settings_with(hosts: Vec<crate::config::NixHost>, fallback: &str) -> Settings {
        Settings {
            flake: PathBuf::from("/tmp/flake"),
            interval: Duration::from_secs(0),
            update_command: "nix flake update".into(),
            rebuild_command: fallback.into(),
            hosts,
            terminal: String::new(),
            notify: false,
        }
    }

    #[test]
    fn a_configured_host_keeps_its_own_command() {
        // An alias already knows its target; rewriting it from a template rebuilds the wrong box.
        let settings = settings_with(
            vec![host("thor", "rebuild-thor")],
            "sudo nixos-rebuild switch --flake .#%HOST%",
        );
        let hosts = configured_hosts(&settings);
        assert_eq!(hosts[0].rebuild, "rebuild-thor");
        assert!(hosts[0].configured);
    }

    #[test]
    fn a_host_without_its_own_command_falls_back_to_the_template() {
        let settings = settings_with(
            vec![host("odin", "")],
            "sudo nixos-rebuild switch --flake .#%HOST%",
        );
        let hosts = configured_hosts(&settings);
        assert_eq!(hosts[0].rebuild, "sudo nixos-rebuild switch --flake .#odin");
    }

    #[test]
    fn a_host_with_no_command_anywhere_is_offered_nothing_to_run() {
        let settings = settings_with(vec![host("loki", "")], "");
        assert_eq!(configured_hosts(&settings)[0].rebuild, "");
    }

    #[test]
    fn a_reference_reads_the_way_it_was_written() {
        assert_eq!(
            describe(
                &json!({ "type": "github", "owner": "NixOS", "repo": "nixpkgs", "ref": "nixos-unstable" })
            ),
            "github:NixOS/nixpkgs/nixos-unstable"
        );
        assert_eq!(
            describe(
                &json!({ "type": "git", "url": "https://github.com/ghostty-org/ghostty", "ref": "refs/tags/v1.3.1" })
            ),
            "https://github.com/ghostty-org/ghostty (refs/tags/v1.3.1)"
        );
        assert_eq!(
            describe(&json!({ "type": "indirect", "id": "nixpkgs" })),
            "nixpkgs"
        );
    }

}
