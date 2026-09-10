use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

/// How the files provider gets its search results.
///
/// The in-memory index answers in microseconds where fd takes ~200ms over a large home directory,
/// but it holds every indexed path in RAM — roughly 1.6KB per entry, so a 700k-entry home costs
/// over a gigabyte against fd's near-zero. Which of those matters is the user's call, not ours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileIndex {
    /// Search with fd, and build the index only as a fallback when fd is not installed.
    #[default]
    Auto,
    /// Always build the index, and use it in preference to fd. Fast, memory-hungry.
    Always,
    /// Never build the index. The files provider goes quiet if fd is not installed.
    Never,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub socket: String,
    pub file_roots: Vec<String>,
    pub ignored_dirs: Vec<String>,
    pub menus_dir: String,
    pub launch_prefix: String,
    pub terminal_cmd: String,
    pub clipboard_max_items: usize,
    pub clipboard_image_dir: String,
    pub clipboard_text_editor: String,
    pub clipboard_image_editor: String,
    pub clipboard_ocr: bool,
    /// Accept incoming LocalSend transfers. The receiver binds a port and answers discovery, so
    /// it is opt-in-able rather than always on.
    pub localsend_receive: bool,
    /// Where accepted transfers are written.
    pub localsend_download_dir: String,
    /// Name shown to other devices. Empty means use the host name.
    pub localsend_alias: String,
    /// Where `capture.screenshot` saves. Created on the first shot rather than at startup.
    pub screenshot_dir: String,
    /// Name for a saved screenshot, expanded by `date`, so `%Y-%m-%d` means what it says. An
    /// extension picks the format: `.png` (default), `.jpg`, or `.ppm`.
    pub screenshot_filename: String,
    /// Put each screenshot on the clipboard as well as saving it.
    pub screenshot_copy: bool,
    /// Keep each screenshot as a file. With this off, shots are copied and left in the cache.
    pub screenshot_save: bool,
    /// Announce each screenshot to the session's notification server, which is the shell.
    pub screenshot_notify: bool,
    /// Tesseract language `capture.ocr` reads with. Several can be joined with `+`, e.g.
    /// `eng+deu`, as long as the data files for them are installed.
    pub ocr_language: String,
    /// Where `capture.record` writes. Created when the first recording starts.
    pub recording_dir: String,
    /// Name for a recording, expanded by `date`. The extension picks the container wf-recorder
    /// writes, so `.mp4` and `.mkv` both work.
    pub recording_filename: String,
    /// Announce a finished recording to the session's notification server.
    pub recording_notify: bool,
    /// The flake `capture`-adjacent Nix awareness watches, e.g. `~/nixconfig`. Empty turns the
    /// whole feature off; nothing is ever written to it.
    pub nix_flake: String,
    /// Minutes between automatic update checks. Zero turns the timer off, leaving `nix.check`.
    pub nix_check_interval_minutes: u64,
    /// What `nix.update` runs in a terminal, from the flake's directory.
    pub nix_update_command: String,
    /// What `nix.rebuild` runs. Empty by default: guessing a rebuild command means running the
    /// wrong one on someone's machine. `%HOST%` is replaced with the host being rebuilt.
    pub nix_rebuild_command: String,
    /// Colour temperature night mode warms the screen to, in kelvin. Lower is warmer; 6500 is
    /// neutral daylight.
    pub night_light_temperature: u32,
    /// Hosts to offer a rebuild for, each with the command that rebuilds it. Empty reads the
    /// names from the flake's `nixosConfigurations` and falls back to `nix_rebuild_command`.
    pub nix_hosts: Vec<NixHost>,
    /// Notify when an input gains an update it did not have at the previous check.
    pub nix_notify: bool,
    /// Frames per second to record at. A constant rate is what keeps the file playable: left to
    /// pick its own timing, wf-recorder writes a stream declaring 90000fps, and x264 derives an
    /// H.264 level from that which players refuse to decode -- the video opens, and shows black.
    /// Zero hands the timing back to wf-recorder.
    pub recording_framerate: u32,
    pub clipboard_capture_interval_ms: u64,
    pub runner_scan_path: bool,
    pub runner_commands: Vec<RunnerCommand>,
    pub provider_enabled: HashMap<String, bool>,
    pub provider_weights: HashMap<String, i32>,
    pub query_prefixes: HashMap<String, String>,
    pub icon_theme: String,
    pub icon_cache_dir: String,
    pub thumbnail_cache_enabled: bool,
    pub persistent_index: bool,
    pub file_index: FileIndex,
}

/// One host in the flake, and how it is rebuilt.
///
/// The command is per host rather than one template with the host substituted in, because a
/// rebuild is usually an alias or a script that already knows its target -- `rebuild-thor`,
/// `deploy odin` -- and rewriting those from a template gets the wrong machine.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NixHost {
    pub name: String,
    /// What rebuilds this host. Empty falls back to `nix_rebuild_command`, with `%HOST%` replaced.
    #[serde(default)]
    pub rebuild: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RunnerCommand {
    pub name: String,
    pub command: String,
    pub keywords: Vec<String>,
    pub icon: Option<String>,
    pub terminal: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct PartialConfig {
    socket: Option<String>,
    file_roots: Option<Vec<String>>,
    ignored_dirs: Option<Vec<String>>,
    menus_dir: Option<String>,
    launch_prefix: Option<String>,
    terminal_cmd: Option<String>,
    clipboard_max_items: Option<usize>,
    clipboard_image_dir: Option<String>,
    clipboard_text_editor: Option<String>,
    clipboard_image_editor: Option<String>,
    clipboard_ocr: Option<bool>,
    localsend_receive: Option<bool>,
    localsend_download_dir: Option<String>,
    localsend_alias: Option<String>,
    screenshot_dir: Option<String>,
    screenshot_filename: Option<String>,
    screenshot_copy: Option<bool>,
    screenshot_save: Option<bool>,
    screenshot_notify: Option<bool>,
    ocr_language: Option<String>,
    recording_dir: Option<String>,
    recording_filename: Option<String>,
    recording_notify: Option<bool>,
    recording_framerate: Option<u32>,
    nix_flake: Option<String>,
    nix_check_interval_minutes: Option<u64>,
    nix_update_command: Option<String>,
    nix_rebuild_command: Option<String>,
    nix_hosts: Option<Vec<NixHost>>,
    night_light_temperature: Option<u32>,
    nix_notify: Option<bool>,
    clipboard_capture_interval_ms: Option<u64>,
    runner_scan_path: Option<bool>,
    runner_commands: Option<Vec<RunnerCommand>>,
    provider_enabled: Option<HashMap<String, bool>>,
    provider_weights: Option<HashMap<String, i32>>,
    query_prefixes: Option<HashMap<String, String>>,
    icon_theme: Option<String>,
    icon_cache_dir: Option<String>,
    thumbnail_cache_enabled: Option<bool>,
    persistent_index: Option<bool>,
    file_index: Option<FileIndex>,
}

impl Default for Config {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        Self {
            socket: default_socket(),
            file_roots: vec![home.display().to_string()],
            ignored_dirs: default_ignored_dirs(&home),
            menus_dir: home.join(".config/epochoxide/menus").display().to_string(),
            launch_prefix: String::new(),
            terminal_cmd: String::new(),
            clipboard_max_items: 100,
            clipboard_image_dir: dirs::cache_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("epochoxide/clipboard/images")
                .display()
                .to_string(),
            clipboard_text_editor: std::env::var("EDITOR")
                .unwrap_or_else(|_| "xdg-open".to_string()),
            clipboard_image_editor: String::new(),
            clipboard_ocr: false,
            localsend_receive: true,
            localsend_download_dir: "~/Downloads".into(),
            localsend_alias: String::new(),
            screenshot_dir: home.join("Pictures/Screenshots").display().to_string(),
            screenshot_filename: "screenshot-%Y%m%d-%H%M%S.png".into(),
            screenshot_copy: true,
            screenshot_save: true,
            screenshot_notify: true,
            ocr_language: "eng".into(),
            recording_dir: home.join("Videos/Recordings").display().to_string(),
            recording_filename: "recording-%Y%m%d-%H%M%S.mp4".into(),
            recording_notify: true,
            recording_framerate: 30,
            nix_flake: String::new(),
            nix_check_interval_minutes: 60,
            nix_update_command: "nix flake update".into(),
            nix_rebuild_command: String::new(),
            nix_hosts: Vec::new(),
            night_light_temperature: 4000,
            nix_notify: true,
            clipboard_capture_interval_ms: 250,
            runner_scan_path: true,
            runner_commands: Vec::new(),
            provider_enabled: default_provider_enabled(),
            provider_weights: default_provider_weights(),
            query_prefixes: default_query_prefixes(),
            icon_theme: String::new(),
            icon_cache_dir: dirs::cache_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("epochoxide/icons")
                .display()
                .to_string(),
            thumbnail_cache_enabled: true,
            persistent_index: true,
            file_index: FileIndex::default(),
        }
    }
}

impl Config {
    pub fn resolved_path(path: Option<&str>) -> Option<PathBuf> {
        path.map(PathBuf::from).or_else(default_config_path)
    }

    pub fn load(path: Option<&str>) -> Result<Self> {
        let Some(path) = Self::resolved_path(path) else {
            return Ok(Self::default());
        };
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let partial: PartialConfig =
            toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        let mut cfg = Self::default();
        if let Some(v) = partial.socket {
            cfg.socket = v;
        }
        if let Some(v) = partial.file_roots {
            cfg.file_roots = v;
        }
        if let Some(v) = partial.ignored_dirs {
            cfg.ignored_dirs = v;
        }
        if let Some(v) = partial.menus_dir {
            cfg.menus_dir = v;
        }
        if let Some(v) = partial.launch_prefix {
            cfg.launch_prefix = v;
        }
        if let Some(v) = partial.terminal_cmd {
            cfg.terminal_cmd = v;
        }
        if let Some(v) = partial.clipboard_max_items {
            cfg.clipboard_max_items = v;
        }
        if let Some(v) = partial.clipboard_image_dir {
            cfg.clipboard_image_dir = v;
        }
        if let Some(v) = partial.clipboard_text_editor {
            cfg.clipboard_text_editor = v;
        }
        if let Some(v) = partial.clipboard_image_editor {
            cfg.clipboard_image_editor = v;
        }
        if let Some(v) = partial.localsend_receive {
            cfg.localsend_receive = v;
        }
        if let Some(v) = partial.localsend_download_dir {
            cfg.localsend_download_dir = v;
        }
        if let Some(v) = partial.localsend_alias {
            cfg.localsend_alias = v;
        }
        if let Some(v) = partial.clipboard_ocr {
            cfg.clipboard_ocr = v;
        }
        if let Some(v) = partial.screenshot_dir {
            cfg.screenshot_dir = v;
        }
        if let Some(v) = partial.screenshot_filename {
            cfg.screenshot_filename = v;
        }
        if let Some(v) = partial.screenshot_copy {
            cfg.screenshot_copy = v;
        }
        if let Some(v) = partial.screenshot_save {
            cfg.screenshot_save = v;
        }
        if let Some(v) = partial.screenshot_notify {
            cfg.screenshot_notify = v;
        }
        if let Some(v) = partial.ocr_language {
            cfg.ocr_language = v;
        }
        if let Some(v) = partial.recording_dir {
            cfg.recording_dir = v;
        }
        if let Some(v) = partial.recording_filename {
            cfg.recording_filename = v;
        }
        if let Some(v) = partial.recording_notify {
            cfg.recording_notify = v;
        }
        if let Some(v) = partial.recording_framerate {
            cfg.recording_framerate = v;
        }
        if let Some(v) = partial.nix_flake {
            cfg.nix_flake = v;
        }
        if let Some(v) = partial.nix_check_interval_minutes {
            cfg.nix_check_interval_minutes = v;
        }
        if let Some(v) = partial.nix_update_command {
            cfg.nix_update_command = v;
        }
        if let Some(v) = partial.nix_rebuild_command {
            cfg.nix_rebuild_command = v;
        }
        if let Some(v) = partial.nix_hosts {
            cfg.nix_hosts = v;
        }
        if let Some(v) = partial.night_light_temperature {
            cfg.night_light_temperature = v;
        }
        if let Some(v) = partial.nix_notify {
            cfg.nix_notify = v;
        }
        if let Some(v) = partial.clipboard_capture_interval_ms {
            cfg.clipboard_capture_interval_ms = v;
        }
        if let Some(v) = partial.runner_scan_path {
            cfg.runner_scan_path = v;
        }
        if let Some(v) = partial.runner_commands {
            cfg.runner_commands = v;
        }
        if let Some(v) = partial.provider_enabled {
            cfg.provider_enabled.extend(v);
        }
        if let Some(v) = partial.provider_weights {
            cfg.provider_weights.extend(v);
        }
        if let Some(v) = partial.query_prefixes {
            cfg.query_prefixes.extend(v);
        }
        if let Some(v) = partial.icon_theme {
            cfg.icon_theme = v;
        }
        if let Some(v) = partial.icon_cache_dir {
            cfg.icon_cache_dir = v;
        }
        if let Some(v) = partial.thumbnail_cache_enabled {
            cfg.thumbnail_cache_enabled = v;
        }
        if let Some(v) = partial.persistent_index {
            cfg.persistent_index = v;
        }
        if let Some(v) = partial.file_index {
            cfg.file_index = v;
        }
        cfg.expand_paths();
        Ok(cfg)
    }

    fn expand_paths(&mut self) {
        self.file_roots = self.file_roots.iter().map(|p| expand(p)).collect();
        self.ignored_dirs = self.ignored_dirs.iter().map(|p| expand(p)).collect();
        self.menus_dir = expand(&self.menus_dir);
        self.clipboard_image_dir = expand(&self.clipboard_image_dir);
        self.screenshot_dir = expand(&self.screenshot_dir);
        self.recording_dir = expand(&self.recording_dir);
        self.nix_flake = expand(&self.nix_flake);
        self.icon_cache_dir = expand(&self.icon_cache_dir);
    }
}

pub fn expand(path: &str) -> String {
    shellexpand::tilde(path).into_owned()
}

fn default_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join(Path::new("epochoxide/config.toml")))
}

fn default_socket() -> String {
    std::env::var("XDG_RUNTIME_DIR")
        .map(|dir| {
            PathBuf::from(dir)
                .join("epochoxide.sock")
                .display()
                .to_string()
        })
        .unwrap_or_else(|_| "/tmp/epochoxide.sock".to_string())
}

fn default_ignored_dirs(home: &Path) -> Vec<String> {
    [
        home.join(".cache").display().to_string(),
        home.join(".local/share/Trash").display().to_string(),
        home.join(".cargo/registry").display().to_string(),
        home.join(".rustup").display().to_string(),
        home.join(".npm").display().to_string(),
        home.join(".pnpm-store").display().to_string(),
        home.join(".var/app").display().to_string(),
        ".git".to_string(),
        "node_modules".to_string(),
        "target".to_string(),
        "dist".to_string(),
        "build".to_string(),
        ".direnv".to_string(),
    ]
    .into()
}

fn default_provider_enabled() -> HashMap<String, bool> {
    [
        "apps",
        "files",
        "runner",
        "clipboard",
        "windows",
        "calc",
        "menus",
    ]
    .into_iter()
    .map(|p| (p.to_string(), true))
    .collect()
}

fn default_provider_weights() -> HashMap<String, i32> {
    [
        ("apps", 20_000),
        ("runner", 12_000),
        ("calc", 10_000),
        ("windows", 6_000),
        ("files", 0),
        ("clipboard", 0),
    ]
    .into_iter()
    .map(|(p, w)| (p.to_string(), w))
    .collect()
}

/// Menus get no default prefix: each one registers under its own name, so a shortcut for it is a
/// `"?" = "keybinds"` line the user adds here alongside the built-in providers.
fn default_query_prefixes() -> HashMap<String, String> {
    [
        (">", "runner"),
        ("/", "files"),
        ("#", "clipboard"),
        ("@", "windows"),
        ("=", "calc"),
    ]
    .into_iter()
    .map(|(prefix, provider)| (prefix.to_string(), provider.to_string()))
    .collect()
}
