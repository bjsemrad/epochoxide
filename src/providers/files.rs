use super::{run_shell, Provider};
use crate::{
    config::{expand, Config, FileIndex},
    fuzzy,
    types::{action_map, ActionCapability, Item, ItemType, ProviderCapability},
};
use anyhow::{Context, Result};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        mpsc::{self, Receiver},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

const SAVE_DEBOUNCE: Duration = Duration::from_secs(3);
const QUERY_CANDIDATE_LIMIT: usize = 5_000;

pub struct LazyFilesProvider {
    config: Config,
    inner: Arc<Mutex<Option<FilesProvider>>>,
}

impl LazyFilesProvider {
    pub fn new(config: Config) -> Self {
        let inner = Arc::new(Mutex::new(None));
        if wants_index(&config) {
            let load_inner = Arc::clone(&inner);
            let load_config = config.clone();
            thread::spawn(move || {
                let provider = FilesProvider::new(load_config);
                *load_inner.lock().unwrap() = Some(provider);
            });
        }
        Self { config, inner }
    }
}

/// The index is only built when the user asked for it, or when fd is missing and it is the only
/// way to answer at all. See [`FileIndex`] for the trade it makes.
fn wants_index(config: &Config) -> bool {
    match config.file_index {
        FileIndex::Always => true,
        FileIndex::Auto => fd_program().is_none(),
        FileIndex::Never => false,
    }
}

impl Provider for LazyFilesProvider {
    fn name(&self) -> &str {
        "files"
    }
    fn pretty_name(&self) -> &str {
        "Files"
    }

    fn query(&mut self, query: &str, limit: usize, exact: bool) -> Vec<Item> {
        // try_lock, not lock: a keystroke that lands while the watcher is draining should fall
        // back to fd rather than stall behind it.
        if let Ok(mut inner) = self.inner.try_lock() {
            if let Some(provider) = inner.as_mut() {
                return provider.query(query, limit, exact);
            }
        }
        // Reached while the index is still loading, or when it was never asked for. fd_query is
        // already empty-safe when fd is missing, which is the documented cost of `never`.
        fd_query(&self.config, query, limit, exact)
    }

    fn activate(
        &mut self,
        identifier: &str,
        action: &str,
        query: &str,
        arguments: &str,
    ) -> Result<()> {
        // Blocking is right here where it is wrong in query: activation is a one-shot the user is
        // waiting on, and skipping it because the index happened to be busy silently does nothing.
        let mut inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        match inner.as_mut() {
            Some(provider) => provider.activate(identifier, action, query, arguments),
            None => activate_path(identifier, action),
        }
    }

    fn events(&mut self) -> Vec<serde_json::Value> {
        let Ok(mut inner) = self.inner.try_lock() else {
            return Vec::new();
        };
        let Some(provider) = inner.as_mut() else {
            return Vec::new();
        };
        provider.events()
    }

    fn capability(&self) -> ProviderCapability {
        files_capability()
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct IndexedFile {
    path: PathBuf,
    display: String,
    search: String,
    #[serde(skip)]
    mask: u64,
}

pub struct FilesProvider {
    config: Config,
    files: HashMap<String, IndexedFile>,
    trigrams: HashMap<[u8; 3], Vec<u32>>,
    trigram_keys: Vec<String>,
    trigram_ids: HashMap<String, u32>,
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
        let cache_path = dirs::cache_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("epochoxide/file-index.json");
        let ignored_dirs = config.ignored_dirs.iter().map(|i| expand(i)).collect();
        let mut this = Self {
            config,
            files: HashMap::new(),
            trigrams: HashMap::new(),
            trigram_keys: Vec::new(),
            trigram_ids: HashMap::new(),
            watcher: None,
            events: None,
            changed: false,
            cache_path,
            ignored_dirs,
            dirty: false,
            last_saved: None,
        };
        if !this.load_cache() {
            this.reindex();
        }
        this.start_watcher();
        this
    }

    fn reindex(&mut self) {
        self.files.clear();
        self.trigrams.clear();
        self.trigram_keys.clear();
        self.trigram_ids.clear();
        for root in self.config.file_roots.clone() {
            let root = PathBuf::from(expand(&root));
            self.add_tree(&root);
        }
        let _ = self.save_cache();
    }

    fn load_cache(&mut self) -> bool {
        if !self.config.persistent_index {
            return false;
        }
        let Some(files) = fs::read_to_string(&self.cache_path)
            .ok()
            .and_then(|raw| serde_json::from_str::<HashMap<String, IndexedFile>>(&raw).ok())
        else {
            return false;
        };
        self.files = files
            .into_iter()
            .filter(|(_, f)| !self.is_ignored(&f.path))
            .map(|(k, mut f)| {
                f.mask = fuzzy::mask(&f.search);
                (k, f)
            })
            .collect();
        self.rebuild_trigrams();
        !self.files.is_empty()
    }

    fn rebuild_trigrams(&mut self) {
        let mut trigrams = HashMap::new();
        let mut trigram_keys = Vec::with_capacity(self.files.len());
        let mut trigram_ids = HashMap::with_capacity(self.files.len());
        for (id, (key, f)) in self.files.iter().enumerate() {
            let id = id as u32;
            trigram_keys.push(key.clone());
            trigram_ids.insert(key.clone(), id);
            add_trigrams(&mut trigrams, id, &f.search);
        }
        self.trigrams = trigrams;
        self.trigram_keys = trigram_keys;
        self.trigram_ids = trigram_ids;
    }

    fn save_cache(&self) -> Result<()> {
        if !self.config.persistent_index {
            return Ok(());
        }
        if let Some(parent) = self.cache_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&self.cache_path, serde_json::to_vec(&self.files)?)?;
        Ok(())
    }

    fn start_watcher(&mut self) {
        let (tx, rx) = mpsc::channel();
        let Ok(mut watcher) = notify::recommended_watcher(tx) else {
            return;
        };
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
            while let Ok(event) = rx.try_recv() {
                events.push(event);
            }
        }
        if !events.is_empty() {
            for event in events.into_iter().flatten() {
                self.apply_event(event);
            }
            self.dirty = true;
        }
        if self.dirty
            && self
                .last_saved
                .map(|t| t.elapsed() >= SAVE_DEBOUNCE)
                .unwrap_or(true)
        {
            self.last_saved = Some(Instant::now());
            if self.save_cache().is_ok() {
                self.dirty = false;
            }
        }
    }

    fn apply_event(&mut self, event: Event) {
        match event.kind {
            EventKind::Remove(_) => {
                for path in event.paths {
                    self.remove_path(&path);
                    self.changed = true;
                }
            }
            _ => {
                for path in event.paths {
                    if path.exists() {
                        self.add_tree(&path);
                    } else {
                        self.remove_path(&path);
                    }
                    self.changed = true;
                }
            }
        }
    }

    fn add_tree(&mut self, root: &Path) {
        if self.is_ignored(root) {
            return;
        }
        if root.is_file() {
            self.add_path(root);
            return;
        }
        if !root.is_dir() {
            return;
        }
        let ignored = self.ignored_dirs.clone();
        for entry in walkdir::WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| !is_ignored_path(e.path(), &ignored))
            .filter_map(|e| e.ok())
        {
            self.add_path(entry.path());
        }
    }

    fn add_path(&mut self, path: &Path) {
        let display = path.display().to_string();
        let search = display.to_lowercase();
        if self.files.get(&display).map(|f| f.search.as_str()) == Some(search.as_str()) {
            return;
        }
        let mask = fuzzy::mask(&search);
        let id = self.trigram_id(&display);
        add_trigrams(&mut self.trigrams, id, &search);
        self.files.insert(
            display.clone(),
            IndexedFile {
                path: path.to_path_buf(),
                display,
                search,
                mask,
            },
        );
    }

    fn remove_path(&mut self, path: &Path) {
        let display = path.display().to_string();
        let prefix = format!("{display}/");
        let removed = self
            .files
            .keys()
            .filter(|key| *key == &display || key.starts_with(&prefix))
            .cloned()
            .collect::<Vec<_>>();
        for key in removed {
            self.files.remove(&key);
        }
    }

    fn trigram_id(&mut self, key: &str) -> u32 {
        if let Some(id) = self.trigram_ids.get(key) {
            return *id;
        }
        let id = self.trigram_keys.len() as u32;
        self.trigram_keys.push(key.to_string());
        self.trigram_ids.insert(key.to_string(), id);
        id
    }

    fn candidate_keys(&self, query: &str) -> Option<Vec<&str>> {
        let query_trigrams = unique_trigrams(query);
        if query_trigrams.is_empty() {
            return None;
        }

        let mut lists = Vec::with_capacity(query_trigrams.len());
        for trigram in query_trigrams {
            let Some(ids) = self.trigrams.get(&trigram) else {
                return Some(Vec::new());
            };
            lists.push(ids);
        }

        let first = lists.iter().copied().min_by_key(|keys| keys.len())?;
        let other_sets = lists
            .into_iter()
            .filter(|keys| !std::ptr::eq(*keys, first))
            .map(|keys| keys.iter().copied().collect::<HashSet<_>>())
            .collect::<Vec<_>>();

        Some(
            first
                .iter()
                .filter(|id| other_sets.iter().all(|set| set.contains(id)))
                .filter_map(|id| self.trigram_keys.get(*id as usize).map(String::as_str))
                .take(QUERY_CANDIDATE_LIMIT)
                .collect(),
        )
    }

    fn is_ignored(&self, path: &Path) -> bool {
        is_ignored_path(path, &self.ignored_dirs)
    }
}

/// Matching against the whole path means every file under a matching directory scores identically
/// off that one directory name, so a query like "proj" buries the files actually called that under
/// everything in ~/projects. Lift matches that land in the entry's own name above those.
fn name_match_bonus(search: &str, start: usize) -> i32 {
    // fd prints directories with a trailing slash, which would otherwise make every directory's
    // own name look like an ancestor component and cost it the bonus.
    let path = search.strip_suffix('/').unwrap_or(search);
    let name_start = path.rfind('/').map(|slash| slash + 1).unwrap_or(0);
    if start >= name_start {
        2_000
    } else {
        0
    }
}

fn is_ignored_path(path: &Path, ignored: &[String]) -> bool {
    ignored.iter().any(|i| {
        if i.starts_with('/') {
            path.starts_with(i)
        } else {
            path.components().any(|c| c.as_os_str() == i.as_str())
        }
    })
}

impl Provider for FilesProvider {
    fn name(&self) -> &str {
        "files"
    }
    fn pretty_name(&self) -> &str {
        "Files"
    }

    fn query(&mut self, query: &str, limit: usize, exact: bool) -> Vec<Item> {
        self.drain_events();
        if query.is_empty() {
            return Vec::new();
        }
        let query_lower = query.to_lowercase();
        let query_mask = fuzzy::mask(&query_lower);
        let mut out = Vec::new();
        let candidates = self.candidate_keys(&query_lower);
        let files: Box<dyn Iterator<Item = &IndexedFile> + '_> = if let Some(keys) = &candidates {
            Box::new(keys.iter().filter_map(|key| self.files.get(*key)))
        } else {
            Box::new(self.files.values())
        };
        for f in files {
            if f.mask & query_mask != query_mask {
                continue;
            }
            if let Some((score, info)) = fuzzy::score_lower(&query_lower, &f.search, exact, "text")
            {
                let score = score + name_match_bonus(&f.search, info.start);
                let mut item = Item::new(self.name(), &f.display, &f.display);
                item.item_type = ItemType::File;
                item.preview = f.display.clone();
                item.preview_type = "file".into();
                item.icon = "text-x-generic".into();
                item.actions = vec![
                    "open".into(),
                    "open_dir".into(),
                    "copy_path".into(),
                    "copy_file".into(),
                ];
                item.score = score;
                item.fuzzyinfo = Some(info);
                out.push(item);
            }
        }
        out.sort_by_key(|item| std::cmp::Reverse(item.score));
        out.truncate(limit);
        out
    }

    fn activate(
        &mut self,
        identifier: &str,
        action: &str,
        _query: &str,
        _arguments: &str,
    ) -> Result<()> {
        if action == "reindex" {
            self.reindex();
            return Ok(());
        }
        activate_path(identifier, action)
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
        files_capability()
    }
}

fn files_capability() -> ProviderCapability {
    ProviderCapability {
        name: "files".into(),
        name_pretty: "Files".into(),
        description: "Search indexed files and directories".into(),
        icon: String::new(),
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

fn fd_query(config: &Config, query: &str, limit: usize, exact: bool) -> Vec<Item> {
    if query.trim().is_empty() {
        return Vec::new();
    }
    let Some(program) = fd_program() else {
        return Vec::new();
    };
    let ignored = config
        .ignored_dirs
        .iter()
        .map(|i| expand(i))
        .collect::<Vec<_>>();
    let mut command = Command::new(program);
    command.arg(fd_pattern(query));
    for root in &config.file_roots {
        command.arg(expand(root));
    }
    command.args([
        "--ignore-vcs",
        "--full-path",
        "--ignore-case",
        "--type",
        "file",
        "--type",
        "directory",
    ]);
    // Bare names are gitignore-style globs fd can prune the walk with; the rooted entries are
    // left to is_ignored_path below, since fd anchors a slash-bearing glob to the search root.
    for dir in ignored.iter().filter(|dir| !dir.contains('/')) {
        command.args(["--exclude", dir]);
    }
    command.args([
        "--max-results",
        &QUERY_CANDIDATE_LIMIT.max(limit).to_string(),
    ]);

    let Ok(out) = command.output() else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }

    let query_lower = query.to_lowercase();
    let mut items = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|path| !is_ignored_path(Path::new(path), &ignored))
        .filter_map(|path| {
            let search = path.to_lowercase();
            let (score, info) = fuzzy::score_lower(&query_lower, &search, exact, "text")?;
            let score = score + name_match_bonus(&search, info.start);
            let mut item = Item::new("files", path, path);
            item.item_type = ItemType::File;
            item.preview = path.to_string();
            item.preview_type = "file".into();
            item.icon = "text-x-generic".into();
            item.actions = vec![
                "open".into(),
                "open_dir".into(),
                "copy_path".into(),
                "copy_file".into(),
            ];
            item.score = score;
            item.fuzzyinfo = Some(info);
            Some(item)
        })
        .collect::<Vec<_>>();
    items.sort_by_key(|item| std::cmp::Reverse(item.score));
    items.truncate(limit);
    items
}

/// fd matches its pattern against the basename only, so an unqualified query like "epoch" finds
/// the directories named that way but none of the files inside them. `--full-path` matches the
/// whole path instead, which is what the in-process index has always scored against; the pattern
/// is a regex there, so the query has to be escaped to stay the literal text the user typed.
fn fd_pattern(query: &str) -> String {
    query
        .chars()
        .fold(String::with_capacity(query.len() * 2), |mut pattern, c| {
            if !c.is_alphanumeric() && !matches!(c, '_' | '-' | '/') {
                pattern.push('\\');
            }
            pattern.push(c);
            pattern
        })
}

fn fd_program() -> Option<&'static str> {
    ["fd", "fdfind"].into_iter().find(|program| {
        Command::new(program)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
    })
}

fn activate_path(identifier: &str, action: &str) -> Result<()> {
    let path = Path::new(identifier);
    match action {
        "open" => run_shell(&format!("xdg-open '{}'", identifier.replace('\'', "'\\''"))),
        "open_dir" => {
            let dir = if path.is_dir() {
                path
            } else {
                path.parent().context("file has no parent")?
            };
            run_shell(&format!(
                "xdg-open '{}'",
                dir.display().to_string().replace('\'', "'\\''")
            ))
        }
        "copy_path" => copy_text(identifier),
        "copy_file" => {
            if let Some(mime) = image_mime(path) {
                return copy_file_as(path, mime);
            }
            let data = std::fs::read_to_string(path)
                .context("copy_file only supports UTF-8 text files")?;
            copy_text(&data)
        }
        "reindex" => Ok(()),
        _ => anyhow::bail!("unsupported files action: {action}"),
    }
}

fn add_trigrams(index: &mut HashMap<[u8; 3], Vec<u32>>, id: u32, search: &str) {
    for trigram in unique_trigrams(search) {
        let ids = index.entry(trigram).or_default();
        if ids.last().copied() != Some(id) {
            ids.push(id);
        }
    }
}

fn unique_trigrams(s: &str) -> Vec<[u8; 3]> {
    let bytes = s.as_bytes();
    if bytes.len() < 3 {
        return Vec::new();
    }

    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for window in bytes.windows(3) {
        let trigram = [window[0], window[1], window[2]];
        if seen.insert(trigram) {
            out.push(trigram);
        }
    }
    out
}

fn copy_text(text: &str) -> Result<()> {
    let mut child = Command::new("wl-copy")
        .stdin(Stdio::piped())
        .spawn()
        .or_else(|_| {
            Command::new("xclip")
                .args(["-selection", "clipboard"])
                .stdin(Stdio::piped())
                .spawn()
        })?;
    use std::io::Write;
    child
        .stdin
        .as_mut()
        .context("clipboard stdin unavailable")?
        .write_all(text.as_bytes())?;
    super::reap(child);
    Ok(())
}

fn copy_file_as(path: &Path, mime: &str) -> Result<()> {
    let data = fs::read(path)?;
    let mut child = Command::new("wl-copy")
        .args(["--type", mime])
        .stdin(Stdio::piped())
        .spawn()?;
    use std::io::Write;
    child
        .stdin
        .as_mut()
        .context("clipboard stdin unavailable")?
        .write_all(&data)?;
    super::reap(child);
    Ok(())
}

fn image_mime(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "bmp" => Some("image/bmp"),
        "webp" => Some("image/webp"),
        "svg" => Some("image/svg+xml"),
        "tif" | "tiff" => Some("image/tiff"),
        "ico" => Some("image/vnd.microsoft.icon"),
        "avif" => Some("image/avif"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{FilesProvider, Provider};
    use crate::config::{expand, Config};
    use std::path::{Path, PathBuf};

    fn test_provider() -> FilesProvider {
        let config = Config::default();
        let ignored_dirs = config.ignored_dirs.iter().map(|i| expand(i)).collect();
        FilesProvider {
            config,
            files: Default::default(),
            trigrams: Default::default(),
            trigram_keys: Default::default(),
            trigram_ids: Default::default(),
            watcher: None,
            events: None,
            changed: false,
            cache_path: PathBuf::new(),
            ignored_dirs,
            dirty: false,
            last_saved: None,
        }
    }

    #[test]
    fn empty_query_skips_the_scan_entirely() {
        let mut provider = test_provider();
        provider.files.insert(
            "/tmp/foo".into(),
            super::IndexedFile {
                path: PathBuf::from("/tmp/foo"),
                display: "/tmp/foo".into(),
                search: "/tmp/foo".into(),
                mask: 0,
            },
        );
        assert!(provider.query("", 20, false).is_empty());
    }

    #[test]
    fn trigram_candidates_intersect_query_trigrams() {
        let mut provider = test_provider();
        provider.add_path(Path::new("/tmp/lib-alpha.txt"));
        provider.add_path(Path::new("/tmp/lib-beta.txt"));
        provider.add_path(Path::new("/tmp/bin-alpha.txt"));

        let candidates = provider.candidate_keys("lib").unwrap();
        assert_eq!(candidates.len(), 2);
        assert!(candidates.iter().all(|path| path.contains("lib")));
    }

    #[test]
    fn missing_trigram_has_no_candidates() {
        let mut provider = test_provider();
        provider.add_path(Path::new("/tmp/lib-alpha.txt"));

        assert_eq!(provider.candidate_keys("xyz"), Some(Vec::new()));
    }

    #[test]
    fn fd_pattern_matches_paths_not_just_basenames() {
        // "epoch" has to reach /home/me/EpochOxide/src/main.rs, not only the directory itself.
        assert_eq!(super::fd_pattern("epoch"), "epoch");
        assert_eq!(super::fd_pattern("src/main"), "src/main");
    }

    #[test]
    fn fd_pattern_escapes_regex_metacharacters() {
        assert_eq!(super::fd_pattern("config.toml"), r"config\.toml");
        assert_eq!(super::fd_pattern("main(1)+"), r"main\(1\)\+");
    }

    #[test]
    fn image_mime_maps_rendered_image_extensions() {
        assert_eq!(super::image_mime(Path::new("photo.png")), Some("image/png"));
        assert_eq!(
            super::image_mime(Path::new("photo.JPG")),
            Some("image/jpeg")
        );
        assert_eq!(
            super::image_mime(Path::new("photo.webp")),
            Some("image/webp")
        );
    }

    #[test]
    fn name_matches_outrank_ancestor_directory_matches() {
        let dir_only = super::name_match_bonus("/home/me/projects/epochoxide/src/main.rs", 9);
        let own_name = super::name_match_bonus("/home/me/src/projects.rs", 13);
        assert_eq!(dir_only, 0);
        assert!(own_name > dir_only);
        // fd prints directories with a trailing slash; the bonus has to survive it.
        assert_eq!(super::name_match_bonus("/home/me/notes/", 9), own_name);
    }

    #[test]
    fn index_is_built_only_when_asked_for_or_needed() {
        use crate::config::FileIndex;
        let always = Config {
            file_index: FileIndex::Always,
            ..Config::default()
        };
        let never = Config {
            file_index: FileIndex::Never,
            ..Config::default()
        };
        let auto = Config {
            file_index: FileIndex::Auto,
            ..Config::default()
        };
        assert!(super::wants_index(&always));
        assert!(!super::wants_index(&never));
        // Auto tracks fd: it indexes exactly when fd cannot answer for it.
        assert_eq!(super::wants_index(&auto), super::fd_program().is_none());
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
        let tags = [
            "lib", "bin", "conf", "data", "doc", "test", "cache", "asset",
        ];
        for i in 0..files {
            let dir = root
                .join(format!("d{}", i / 500))
                .join(format!("s{}", i % 31));
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join(format!("{}-{i}.txt", tags[i % tags.len()]));
            fs::write(&path, b"x").unwrap();
        }
        root.to_path_buf()
    }

    fn run(label: &str, file_count: usize) {
        let tmp = tempfile::tempdir().unwrap();
        build_tree(tmp.path(), file_count);

        let config = Config {
            file_roots: vec![tmp.path().display().to_string()],
            persistent_index: false,
            ..Config::default()
        };

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
            eprintln!(
                "[{label}] query {q:?}: {} results in {:?}",
                results.len(),
                start.elapsed()
            );
        }

        let start = Instant::now();
        provider.remove_path(tmp.path());
        eprintln!(
            "[{label}] remove_path(root): {:?} ({} files remaining)",
            start.elapsed(),
            provider.files.len()
        );
    }

    #[test]
    #[ignore]
    fn perf_reindex_query_remove() {
        let file_count: usize = std::env::var("EO_PERF_FILES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(150_000);
        run("perf", file_count);
    }
}
