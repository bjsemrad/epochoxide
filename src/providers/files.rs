use super::{run_shell, Provider};
use crate::{config::{expand, Config}, fuzzy, types::{action_map, ActionCapability, Item, ItemType, ProviderCapability}};
use anyhow::{Context, Result};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, path::{Path, PathBuf}, process::{Command, Stdio}, sync::mpsc::{self, Receiver}, time::{Duration, Instant}};

const SAVE_DEBOUNCE: Duration = Duration::from_secs(3);

#[derive(Clone, Serialize, Deserialize)]
struct IndexedFile { path: PathBuf, display: String, search: String, #[serde(skip)] mask: u64 }

pub struct FilesProvider {
    config: Config,
    files: HashMap<String, IndexedFile>,
    watcher: Option<RecommendedWatcher>,
    events: Option<Receiver<notify::Result<Event>>>,
    changed: bool,
    cache_path: PathBuf,
    ignored_dirs: Vec<String>,
    dirty: bool,
    last_saved: Option<Instant>,
}

impl FilesProvider {
    pub fn new(config: Config) -> Self {
        let cache_path = dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("epochoxide/file-index.json");
        let ignored_dirs = config.ignored_dirs.iter().map(|i| expand(i)).collect();
        let mut this = Self { config, files: HashMap::new(), watcher: None, events: None, changed: false, cache_path, ignored_dirs, dirty: false, last_saved: None };
        if !this.load_cache() { this.reindex(); }
        this.start_watcher();
        this
    }

    fn reindex(&mut self) {
        self.files.clear();
        for root in self.config.file_roots.clone() {
            let root = PathBuf::from(expand(&root));
            self.add_tree(&root);
        }
        let _ = self.save_cache();
    }

    fn load_cache(&mut self) -> bool {
        if !self.config.persistent_index { return false; }
        let Some(files) = fs::read_to_string(&self.cache_path).ok().and_then(|raw| serde_json::from_str::<HashMap<String, IndexedFile>>(&raw).ok()) else { return false; };
        self.files = files.into_iter()
            .filter(|(_, f)| f.path.exists() && !self.is_ignored(&f.path))
            .map(|(k, mut f)| { f.mask = fuzzy::mask(&f.search); (k, f) })
            .collect();
        !self.files.is_empty()
    }

    fn save_cache(&self) -> Result<()> {
        if !self.config.persistent_index { return Ok(()); }
        if let Some(parent) = self.cache_path.parent() { fs::create_dir_all(parent)?; }
        fs::write(&self.cache_path, serde_json::to_vec(&self.files)?)?;
        Ok(())
    }

    fn start_watcher(&mut self) {
        let (tx, rx) = mpsc::channel();
        let Ok(mut watcher) = notify::recommended_watcher(tx) else { return; };
        for root in &self.config.file_roots {
            let root = PathBuf::from(expand(root));
            if root.exists() {
                let _ = watcher.watch(&root, RecursiveMode::Recursive);
            }
        }
        self.watcher = Some(watcher);
        self.events = Some(rx);
    }

    fn drain_events(&mut self) {
        let mut events = Vec::new();
        if let Some(rx) = &self.events {
            while let Ok(event) = rx.try_recv() { events.push(event); }
        }
        if !events.is_empty() {
            for event in events.into_iter().flatten() { self.apply_event(event); }
            self.dirty = true;
        }
        if self.dirty && self.last_saved.map(|t| t.elapsed() >= SAVE_DEBOUNCE).unwrap_or(true) {
            self.last_saved = Some(Instant::now());
            if self.save_cache().is_ok() { self.dirty = false; }
        }
    }

    fn apply_event(&mut self, event: Event) {
        match event.kind {
            EventKind::Remove(_) => {
                for path in event.paths { self.remove_path(&path); self.changed = true; }
            }
            _ => {
                for path in event.paths {
                    if path.exists() { self.add_tree(&path); } else { self.remove_path(&path); }
                    self.changed = true;
                }
            }
        }
    }

    fn add_tree(&mut self, root: &Path) {
        if self.is_ignored(root) { return; }
        if root.is_file() {
            self.add_path(root);
            return;
        }
        if !root.is_dir() { return; }
        let ignored = self.ignored_dirs.clone();
        for entry in walkdir::WalkDir::new(root).follow_links(false).into_iter().filter_entry(|e| !is_ignored_path(e.path(), &ignored)).filter_map(|e| e.ok()) {
            self.add_path(entry.path());
        }
    }

    fn add_path(&mut self, path: &Path) {
        let display = path.display().to_string();
        let search = display.to_lowercase();
        let mask = fuzzy::mask(&search);
        self.files.insert(display.clone(), IndexedFile { path: path.to_path_buf(), display, search, mask });
    }

    fn remove_path(&mut self, path: &Path) {
        let display = path.display().to_string();
        let prefix = format!("{display}/");
        self.files.retain(|key, _| key != &display && !key.starts_with(&prefix));
    }

    fn is_ignored(&self, path: &Path) -> bool {
        is_ignored_path(path, &self.ignored_dirs)
    }
}

fn is_ignored_path(path: &Path, ignored: &[String]) -> bool {
    ignored.iter().any(|i| {
        if i.starts_with('/') { path.starts_with(i) } else { path.components().any(|c| c.as_os_str() == i.as_str()) }
    })
}

impl Provider for FilesProvider {
    fn name(&self) -> &'static str { "files" }
    fn pretty_name(&self) -> &'static str { "Files" }

    fn query(&mut self, query: &str, limit: usize, exact: bool) -> Vec<Item> {
        self.drain_events();
        if query.is_empty() { return Vec::new(); }
        let query_lower = query.to_lowercase();
        let query_mask = fuzzy::mask(&query_lower);
        let mut out = Vec::new();
        for f in self.files.values() {
            if f.mask & query_mask != query_mask { continue; }
            if let Some((score, info)) = fuzzy::score_lower(&query_lower, &f.search, exact, "text") {
                let mut item = Item::new(self.name(), &f.display, &f.display);
                item.item_type = ItemType::File;
                item.preview = f.display.clone();
                item.preview_type = "file".into();
                item.icon = if f.path.is_dir() { "folder".into() } else { "text-x-generic".into() };
                item.actions = vec!["open".into(), "open_dir".into(), "copy_path".into(), "copy_file".into()];
                item.score = score;
                item.fuzzyinfo = Some(info);
                out.push(item);
            }
        }
        out.sort_by_key(|item| std::cmp::Reverse(item.score));
        out.truncate(limit);
        out
    }

    fn activate(&mut self, identifier: &str, action: &str, _query: &str, _arguments: &str) -> Result<()> {
        let path = Path::new(identifier);
        match action {
            "open" => run_shell(&format!("xdg-open '{}'", identifier.replace('\'', "'\\''"))),
            "open_dir" => {
                let dir = if path.is_dir() { path } else { path.parent().context("file has no parent")? };
                run_shell(&format!("xdg-open '{}'", dir.display().to_string().replace('\'', "'\\''")))
            }
            "copy_path" => copy_text(identifier),
            "copy_file" => {
                let data = std::fs::read_to_string(path).context("copy_file only supports UTF-8 text files")?;
                copy_text(&data)
            }
            "reindex" => { self.reindex(); Ok(()) }
            _ => anyhow::bail!("unsupported files action: {action}"),
        }
    }

    fn events(&mut self) -> Vec<serde_json::Value> {
        self.drain_events();
        if std::mem::take(&mut self.changed) {
            vec![serde_json::json!({"provider": self.name(), "kind": "index_changed"})]
        } else {
            Vec::new()
        }
    }

    fn capability(&self) -> ProviderCapability {
        ProviderCapability {
            name: self.name().into(),
            name_pretty: self.pretty_name().into(),
            description: "Search indexed files and directories".into(),
            prefixes: Vec::new(),
            actions: action_map(&[
                ("open", ActionCapability::new("Open")),
                ("open_dir", ActionCapability::new("Open Directory")),
                ("copy_path", ActionCapability::new("Copy Path")),
                ("copy_file", ActionCapability::new("Copy File Content")),
                ("reindex", ActionCapability::new("Reindex").async_action()),
            ]),
            supports_query: true,
            supports_activate: true,
            supports_streaming: true,
            supports_subscriptions: true,
            emits_events: true,
        }
    }
}

fn copy_text(text: &str) -> Result<()> {
    let mut child = Command::new("wl-copy").stdin(Stdio::piped()).spawn().or_else(|_| Command::new("xclip").args(["-selection", "clipboard"]).stdin(Stdio::piped()).spawn())?;
    use std::io::Write;
    child.stdin.as_mut().context("clipboard stdin unavailable")?.write_all(text.as_bytes())?;
    super::reap(child);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{FilesProvider, Provider};
    use crate::config::{expand, Config};
    use std::path::{Path, PathBuf};

    fn test_provider() -> FilesProvider {
        let config = Config::default();
        let ignored_dirs = config.ignored_dirs.iter().map(|i| expand(i)).collect();
        FilesProvider { config, files: Default::default(), watcher: None, events: None, changed: false, cache_path: PathBuf::new(), ignored_dirs, dirty: false, last_saved: None }
    }

    #[test]
    fn empty_query_skips_the_scan_entirely() {
        let mut provider = test_provider();
        provider.files.insert("/tmp/foo".into(), super::IndexedFile { path: PathBuf::from("/tmp/foo"), display: "/tmp/foo".into(), search: "/tmp/foo".into(), mask: 0 });
        assert!(provider.query("", 20, false).is_empty());
    }

    #[test]
    fn ignores_common_directory_names() {
        let provider = test_provider();
        assert!(provider.is_ignored(Path::new("/home/me/project/target/debug")));
    }

    #[test]
    fn does_not_ignore_names_that_merely_contain_a_pattern() {
        let provider = test_provider();
        assert!(!provider.is_ignored(Path::new("/home/me/targeting-app/src/main.rs")));
    }
}

#[cfg(test)]
mod perf {
    use super::*;
    use std::time::Instant;

    fn build_tree(root: &Path, files: usize) -> PathBuf {
        let tags = ["lib", "bin", "conf", "data", "doc", "test", "cache", "asset"];
        for i in 0..files {
            let dir = root.join(format!("d{}", i / 500)).join(format!("s{}", i % 31));
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join(format!("{}-{i}.txt", tags[i % tags.len()]));
            fs::write(&path, b"x").unwrap();
        }
        root.to_path_buf()
    }

    fn run(label: &str, file_count: usize) {
        let tmp = tempfile::tempdir().unwrap();
        build_tree(tmp.path(), file_count);

        let config = Config { file_roots: vec![tmp.path().display().to_string()], persistent_index: false, ..Config::default() };

        let start = Instant::now();
        let mut provider = FilesProvider::new(config);
        let reindex = start.elapsed();
        let indexed = provider.files.len();
        eprintln!(
            "[{label}] reindex: {indexed} files in {reindex:?} ({:.0} files/sec)",
            indexed as f64 / reindex.as_secs_f64()
        );

        for q in ["lib", "conf-99", "nomatchxyz"] {
            let start = Instant::now();
            let results = provider.query(q, 20, false);
            eprintln!("[{label}] query {q:?}: {} results in {:?}", results.len(), start.elapsed());
        }

        let start = Instant::now();
        provider.remove_path(tmp.path());
        eprintln!("[{label}] remove_path(root): {:?} ({} files remaining)", start.elapsed(), provider.files.len());
    }

    #[test]
    #[ignore]
    fn perf_reindex_query_remove() {
        let file_count: usize = std::env::var("EO_PERF_FILES").ok().and_then(|v| v.parse().ok()).unwrap_or(150_000);
        run("perf", file_count);
    }
}
