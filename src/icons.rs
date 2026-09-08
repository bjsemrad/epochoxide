use crate::config::Config;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub struct IconResolver {
    search_dirs: Vec<PathBuf>,
    cache: Mutex<HashMap<String, Option<String>>>,
}

impl IconResolver {
    pub fn new(config: &Config) -> Self {
        Self {
            search_dirs: theme_search_dirs(config),
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn resolve(&self, icon: &str) -> Option<String> {
        if icon.is_empty() {
            return None;
        }
        let icon_path = Path::new(icon);
        if icon_path.is_absolute() {
            return icon_path.exists().then(|| icon.to_string());
        }

        if let Some(cached) = self.cache.lock().unwrap().get(icon) {
            return cached.clone();
        }
        let names = icon_names(icon);
        let found = self
            .search_dirs
            .iter()
            .find_map(|dir| names.iter().map(|name| dir.join(name)).find(|p| p.exists()))
            .map(|p| p.display().to_string());
        self.cache
            .lock()
            .unwrap()
            .insert(icon.to_string(), found.clone());
        found
    }
}

fn theme_search_dirs(config: &Config) -> Vec<PathBuf> {
    let mut theme_bases = Vec::new();
    if let Some(data) = dirs::data_dir() {
        theme_bases.push(data.join("icons"));
    }
    if !config.icon_cache_dir.is_empty() {
        theme_bases.push(PathBuf::from(&config.icon_cache_dir));
    }
    theme_bases.push(PathBuf::from("/usr/share/icons"));

    let start = if config.icon_theme.is_empty() {
        "hicolor"
    } else {
        config.icon_theme.as_str()
    };
    let mut queue = vec![start.to_string()];
    let mut seen = HashSet::new();
    let mut dirs = Vec::new();
    while let Some(theme) = queue.pop() {
        if !seen.insert(theme.clone()) {
            continue;
        }
        for base in &theme_bases {
            let theme_root = base.join(&theme);
            if let Some(index) = read_theme_index(&theme_root) {
                dirs.extend(index.directories.iter().map(|d| theme_root.join(d)));
                queue.extend(index.inherits);
                break;
            }
        }
    }
    if seen.insert("hicolor".to_string()) {
        for base in &theme_bases {
            let theme_root = base.join("hicolor");
            if let Some(index) = read_theme_index(&theme_root) {
                dirs.extend(index.directories.iter().map(|d| theme_root.join(d)));
            }
        }
    }
    dirs.extend(theme_bases);
    dirs.push(PathBuf::from("/usr/share/pixmaps"));
    dirs
}

struct ThemeIndex {
    inherits: Vec<String>,
    directories: Vec<String>,
}

fn read_theme_index(theme_dir: &Path) -> Option<ThemeIndex> {
    let raw = fs::read_to_string(theme_dir.join("index.theme")).ok()?;
    let mut in_icon_theme = false;
    let mut inherits = Vec::new();
    let mut directories = Vec::new();
    for line in raw.lines().map(str::trim) {
        if line == "[Icon Theme]" {
            in_icon_theme = true;
            continue;
        }
        if line.starts_with('[') {
            in_icon_theme = false;
            continue;
        }
        if !in_icon_theme {
            continue;
        }
        let Some((key, val)) = line.split_once('=') else {
            continue;
        };
        match key {
            "Inherits" => inherits = split_list(val),
            "Directories" => directories = split_list(val),
            _ => {}
        }
    }
    Some(ThemeIndex {
        inherits,
        directories,
    })
}

fn split_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn icon_names(icon: &str) -> Vec<String> {
    if icon.contains('.') {
        vec![icon.to_string()]
    } else {
        ["png", "svg", "xpm"]
            .into_iter()
            .map(|ext| format!("{icon}.{ext}"))
            .collect()
    }
}

pub fn thumbnail(source: &str, config: &Config) -> Option<String> {
    if !config.thumbnail_cache_enabled || source.is_empty() {
        return None;
    }

    let dir = PathBuf::from(&config.icon_cache_dir);
    if !dir.is_dir() {
        return None;
    }

    let stamp = stamp_for(source);
    let cached = dir.join(format!("{stamp}.png"));
    if cached.exists() {
        return Some(cached.display().to_string());
    }

    let src = Path::new(source);
    match src
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "svg" => render_with(
            source,
            &cached,
            &["rsvg-convert", "-w", "48", "-h", "48", "-o"],
        ),
        _ => render_with(
            source,
            &cached,
            &["convert", "-thumbnail", "48x48", "-background", "none"],
        ),
    }
}

fn render_with(source: &str, cached: &Path, template: &[&str]) -> Option<String> {
    let shell = match *template.first()? {
        "convert" => format!(
            "convert {} -thumbnail 48x48 -background none {}",
            shell_args(Path::new(source)),
            shell_args(cached)
        ),
        _ => format!(
            "rsvg-convert -w 48 -h 48 -o {} {}",
            shell_args(cached),
            shell_args(Path::new(source))
        ),
    };
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(shell)
        .output()
        .ok()?;
    if out.status.success() && cached.exists() {
        Some(cached.display().to_string())
    } else {
        None
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn theme_index_parses_inherits_and_directories() {
        let dir = tempfile::tempdir().unwrap();
        let theme_dir = dir.path().join("MyTheme");
        fs::create_dir_all(&theme_dir).unwrap();
        fs::write(theme_dir.join("index.theme"), "[Icon Theme]\nName=MyTheme\nInherits=hicolor,breeze\nDirectories=48x48/apps,scalable/apps\n\n[48x48/apps]\nSize=48\n").unwrap();

        let index = read_theme_index(&theme_dir).unwrap();
        assert_eq!(index.inherits, vec!["hicolor", "breeze"]);
        assert_eq!(index.directories, vec!["48x48/apps", "scalable/apps"]);
    }

    #[test]
    fn resolver_caches_after_first_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let apps_dir = dir.path().join("MyTheme/48x48/apps");
        fs::create_dir_all(&apps_dir).unwrap();
        fs::write(
            dir.path().join("MyTheme/index.theme"),
            "[Icon Theme]\nDirectories=48x48/apps\n",
        )
        .unwrap();
        fs::write(apps_dir.join("firefox.png"), b"").unwrap();

        let config = Config {
            icon_theme: "MyTheme".into(),
            icon_cache_dir: dir.path().display().to_string(),
            ..Config::default()
        };
        let resolver = IconResolver::new(&config);

        let found = resolver
            .resolve("firefox")
            .expect("should find icon on first lookup");
        fs::remove_file(apps_dir.join("firefox.png")).unwrap();
        let cached = resolver
            .resolve("firefox")
            .expect("cached result should survive file removal");
        assert_eq!(found, cached);
    }
}
