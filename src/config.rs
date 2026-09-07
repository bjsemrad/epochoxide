use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{fs, path::{Path, PathBuf}};

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
    pub clipboard_capture_interval_ms: u64,
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
    clipboard_capture_interval_ms: Option<u64>,
}

impl Default for Config {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        Self {
            socket: default_socket(),
            file_roots: vec![home.join("Documents").display().to_string(), home.join("Downloads").display().to_string()],
            ignored_dirs: vec![home.join(".cache").display().to_string(), ".git".to_string()],
            menus_dir: home.join(".config/epochoxide/menus").display().to_string(),
            launch_prefix: String::new(),
            terminal_cmd: String::new(),
            clipboard_max_items: 100,
            clipboard_image_dir: dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("epochoxide/clipboard/images").display().to_string(),
            clipboard_text_editor: std::env::var("EDITOR").unwrap_or_else(|_| "xdg-open".to_string()),
            clipboard_image_editor: String::new(),
            clipboard_ocr: false,
            clipboard_capture_interval_ms: 250,
        }
    }
}

impl Config {
    pub fn load(path: Option<&str>) -> Result<Self> {
        let Some(path) = path.map(PathBuf::from).or_else(default_config_path) else {
            return Ok(Self::default());
        };
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let partial: PartialConfig = toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        let mut cfg = Self::default();
        if let Some(v) = partial.socket { cfg.socket = v; }
        if let Some(v) = partial.file_roots { cfg.file_roots = v; }
        if let Some(v) = partial.ignored_dirs { cfg.ignored_dirs = v; }
        if let Some(v) = partial.menus_dir { cfg.menus_dir = v; }
        if let Some(v) = partial.launch_prefix { cfg.launch_prefix = v; }
        if let Some(v) = partial.terminal_cmd { cfg.terminal_cmd = v; }
        if let Some(v) = partial.clipboard_max_items { cfg.clipboard_max_items = v; }
        if let Some(v) = partial.clipboard_image_dir { cfg.clipboard_image_dir = v; }
        if let Some(v) = partial.clipboard_text_editor { cfg.clipboard_text_editor = v; }
        if let Some(v) = partial.clipboard_image_editor { cfg.clipboard_image_editor = v; }
        if let Some(v) = partial.clipboard_ocr { cfg.clipboard_ocr = v; }
        if let Some(v) = partial.clipboard_capture_interval_ms { cfg.clipboard_capture_interval_ms = v; }
        cfg.expand_paths();
        Ok(cfg)
    }

    fn expand_paths(&mut self) {
        self.file_roots = self.file_roots.iter().map(|p| expand(p)).collect();
        self.ignored_dirs = self.ignored_dirs.iter().map(|p| expand(p)).collect();
        self.menus_dir = expand(&self.menus_dir);
        self.clipboard_image_dir = expand(&self.clipboard_image_dir);
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
        .map(|dir| PathBuf::from(dir).join("epochoxide.sock").display().to_string())
        .unwrap_or_else(|_| "/tmp/epochoxide.sock".to_string())
}
