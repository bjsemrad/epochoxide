use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileIndex {
    #[default]
    Auto,
    Always,
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
    pub localsend_receive: bool,
    pub localsend_download_dir: String,
    pub localsend_alias: String,
    pub screenshot_dir: String,
    pub screenshot_filename: String,
    pub screenshot_copy: bool,
    pub screenshot_save: bool,
    pub screenshot_notify: bool,
    pub screenshot_annotate: bool,
    pub ocr_language: String,
    pub recording_dir: String,
    pub wallpaper_dirs: Vec<String>,
    pub wallpaper_backend: String,
    pub wallpaper_fit_mode: String,
    pub recording_filename: String,
    pub recording_notify: bool,
    pub nix_flake: String,
    pub nix_check_interval_minutes: u64,
    pub nix_update_command: String,
    pub nix_rebuild_command: String,
    pub night_light_temperature: u32,
    pub nix_hosts: Vec<NixHost>,
    pub nix_notify: bool,
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NixHost {
    pub name: String,
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
    screenshot_annotate: Option<bool>,
    ocr_language: Option<String>,
    recording_dir: Option<String>,
    wallpaper_dirs: Option<Vec<String>>,
    wallpaper_fit_mode: Option<String>,
    wallpaper_backend: Option<String>,
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
            screenshot_annotate: false,
            ocr_language: "eng".into(),
            recording_dir: home.join("Videos/Recordings").display().to_string(),
            wallpaper_dirs: vec![
                home.join(".config/hypr").display().to_string(),
                home.join("Pictures/Wallpapers").display().to_string(),
                home.join("Wallpapers").display().to_string(),
                home.join(".local/share/wallpapers").display().to_string(),
            ],
            // hyprpaper 0.8.4 renders "stretch", "fit" and "fill" as cover over IPC (hyprctl and
            // hyprpaper disagree on the enum's string), so cover is the only filling mode that
            // works.
            wallpaper_fit_mode: "cover".into(),
            wallpaper_backend: "auto".into(),
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
        if let Some(v) = partial.screenshot_annotate {
            cfg.screenshot_annotate = v;
        }
        if let Some(v) = partial.ocr_language {
            cfg.ocr_language = v;
        }
        if let Some(v) = partial.wallpaper_dirs {
            cfg.wallpaper_dirs = v;
        }
        if let Some(v) = partial.wallpaper_fit_mode {
            cfg.wallpaper_fit_mode = v;
        }
        if let Some(v) = partial.wallpaper_backend {
            cfg.wallpaper_backend = v;
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
        self.wallpaper_dirs = self.wallpaper_dirs.iter().map(|p| expand(p)).collect();
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
