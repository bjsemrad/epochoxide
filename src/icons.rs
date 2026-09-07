use crate::config::Config;
use std::fs;
use std::path::{Path, PathBuf};

pub fn resolve(icon: &str, config: &Config) -> Option<String> {
    if icon.is_empty() { return None; }
    let icon_path = Path::new(icon);
    if icon_path.is_absolute() && icon_path.exists() { return Some(icon.to_string()); }

    let names = icon_names(icon);
    for base in icon_bases(config) {
        for name in &names {
            if let Some(found) = find_icon(&base, name, 4) { return Some(found.display().to_string()); }
        }
    }
    None
}

fn icon_bases(config: &Config) -> Vec<PathBuf> {
    let mut bases = Vec::new();
    if let Some(data) = dirs::data_dir() { bases.push(data.join("icons")); }
    if !config.icon_cache_dir.is_empty() { bases.push(PathBuf::from(&config.icon_cache_dir)); }
    bases.push(PathBuf::from("/usr/share/icons"));
    bases.push(PathBuf::from("/usr/share/pixmaps"));
    bases
}

fn icon_names(icon: &str) -> Vec<String> {
    if icon.contains('.') { vec![icon.to_string()] } else { ["png", "svg", "xpm"].into_iter().map(|ext| format!("{icon}.{ext}")).collect() }
}

fn find_icon(dir: &Path, name: &str, depth: usize) -> Option<PathBuf> {
    if depth == 0 || !dir.is_dir() { return None; }
    let direct = dir.join(name);
    if direct.exists() { return Some(direct); }
    for entry in fs::read_dir(dir).ok()?.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_icon(&path, name, depth - 1) { return Some(found); }
        }
    }
    None
}

pub fn thumbnail(source: &str, config: &Config) -> Option<String> {
    if !config.thumbnail_cache_enabled || source.is_empty() { return None; }

    let dir = PathBuf::from(&config.icon_cache_dir);
    if !dir.is_dir() { return None; }

    let stamp = stamp_for(source);
    let cached = dir.join(format!("{stamp}.png"));
    if cached.exists() { return Some(cached.display().to_string()); }

    let src = Path::new(source);
    match src.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase().as_str() {
        "svg" => render_with(source, &cached, &["rsvg-convert", "-w", "48", "-h", "48", "-o"]),
        _ => render_with(source, &cached, &["convert", "-thumbnail", "48x48", "-background", "none"]),
    }
}

fn render_with(source: &str, cached: &Path, template: &[&str]) -> Option<String> {
    let shell = match *template.first()? {
        "convert" => format!("convert {} -thumbnail 48x48 -background none {}", shell_args(Path::new(source)), shell_args(cached)),
        _ => format!("rsvg-convert -w 48 -h 48 -o {} {}", shell_args(cached), shell_args(Path::new(source))),
    };
    let out = std::process::Command::new("sh").arg("-c").arg(shell).output().ok()?;
    if out.status.success() && cached.exists() { Some(cached.display().to_string()) } else { None }
}

fn shell_args(s: &Path) -> String {
    format!("'{}'", s.display().to_string().replace('\'', "'\\''"))
}

fn stamp_for(source: &str) -> String {
    let mut h = 0x811c9dc5u32;
    for b in source.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    format!("{h:08x}")
}