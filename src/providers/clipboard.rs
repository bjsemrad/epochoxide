use super::{run_shell, Provider};
use crate::{config::Config, fuzzy, types::{action_map, ActionCapability, Item, ProviderCapability}};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{fs, io::{Read, Write}, path::{Path, PathBuf}, process::{Command, Stdio}, sync::{Arc, Mutex}, thread};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
enum ClipKind { Text, Image }

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Clip {
    id: String,
    kind: ClipKind,
    content: String,
    image_path: Option<String>,
    mime: Option<String>,
    ocr: String,
    pinned: bool,
}

/// Owns clipboard state behind a `Mutex` so both `Provider` calls (query/activate, driven by
/// client requests) and the background watcher thread (driven by clipboard changes, independent
/// of any client) can capture new clips. Without this, history only ever advanced when a client
/// happened to be actively searching -- copying something while the launcher was closed (or just
/// idle) was silently lost until the next search re-checked the clipboard.
struct ClipboardStore {
    config: Config,
    items: Mutex<Vec<Clip>>,
    cache: PathBuf,
}

impl ClipboardStore {
    fn capture_current(&self) {
        if self.capture_text().unwrap_or(false) { return; }
        let Some(mime) = current_clipboard_image_mime() else { return; };
        let _ = self.capture_image(&mime);
    }

    fn capture_text(&self) -> Result<bool> {
        let out = Command::new("wl-paste").args(["--type", "text", "--no-newline"]).output()?;
        if !out.status.success() { return Ok(false); }
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        let mut items = self.items.lock().unwrap();
        if text.trim().is_empty() { return Ok(true); }
        if items.first().map(|i| i.content.as_str()) == Some(text.as_str()) { return Ok(true); }
        let id = format!("text-{:x}", stable_hash(text.as_bytes()));
        items.retain(|i| i.id != id);
        items.insert(0, Clip { id, kind: ClipKind::Text, content: text, image_path: None, mime: Some("text/plain".into()), ocr: String::new(), pinned: false });
        self.compact(&mut items)?;
        Ok(true)
    }

    fn capture_image(&self, mime: &str) -> Result<()> {
        let out = Command::new("wl-paste").args(["--type", mime]).output()?;
        if !out.status.success() || out.stdout.is_empty() { return Ok(()); }
        let id = format!("image-{:x}", stable_hash(&out.stdout));
        let mut items = self.items.lock().unwrap();
        if items.first().map(|i| i.id.as_str()) == Some(id.as_str()) { return Ok(()); }
        fs::create_dir_all(&self.config.clipboard_image_dir)?;
        let ext = image_extension(mime);
        let path = Path::new(&self.config.clipboard_image_dir).join(format!("{id}.{ext}"));
        fs::write(&path, &out.stdout)?;
        let ocr = if self.config.clipboard_ocr { run_ocr(&path).unwrap_or_default() } else { String::new() };
        items.retain(|i| i.id != id);
        items.insert(0, Clip {
            id,
            kind: ClipKind::Image,
            content: if ocr.is_empty() { "Image clipboard item".into() } else { ocr.lines().next().unwrap_or("Image clipboard item").to_string() },
            image_path: Some(path.display().to_string()),
            mime: Some(mime.to_string()),
            ocr,
            pinned: false,
        });
        self.compact(&mut items)
    }

    fn compact(&self, items: &mut Vec<Clip>) -> Result<()> {
        items.sort_by_key(|c| !c.pinned);
        items.truncate(self.config.clipboard_max_items);
        self.save(items)
    }

    fn save(&self, items: &[Clip]) -> Result<()> {
        if let Some(parent) = self.cache.parent() { fs::create_dir_all(parent)?; }
        fs::write(&self.cache, serde_json::to_vec(items)?)?;
        Ok(())
    }
}

/// Runs `wl-paste --watch` and emits one line per clipboard change. This mirrors Elephant's
/// proven watcher path; the line is just a change signal, the real clipboard is read separately.
fn spawn_watcher(store: Arc<ClipboardStore>) {
    thread::spawn(move || {
        let Ok(mut child) = Command::new("wl-paste")
            .args(["--watch", "echo", "clipboard-changed"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        else { return; };
        let Some(mut stdout) = child.stdout.take() else { return; };
        let mut buf = [0u8; 4096];
        loop {
            match stdout.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => store.capture_current(),
            }
        }
        let _ = child.wait();
    });
}

pub struct ClipboardProvider { store: Arc<ClipboardStore> }

impl ClipboardProvider {
    pub fn new(config: Config) -> Result<Self> {
        let cache = dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("epochoxide/clipboard.json");
        let items = fs::read_to_string(&cache).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        let store = Arc::new(ClipboardStore { config, items: Mutex::new(items), cache });
        spawn_watcher(Arc::clone(&store));
        Ok(Self { store })
    }
}

impl Provider for ClipboardProvider {
    fn name(&self) -> &str { "clipboard" }
    fn pretty_name(&self) -> &str { "Clipboard" }

    fn query(&mut self, query: &str, limit: usize, exact: bool) -> Vec<Item> {
        self.store.capture_current();
        let mut out = Vec::new();
        let items = self.store.items.lock().unwrap();
        for (index, clip) in items.iter().enumerate() {
            let searchable = format!("{} {}", clip.content, clip.ocr);
            if let Some((score, info)) = fuzzy::score(query, &searchable, exact, "text") {
                let mut item = Item::new(self.name(), &clip.id, clip.content.trim_start().lines().next().unwrap_or_default());
                item.subtext = match clip.kind {
                    ClipKind::Text => clip.content.chars().take(160).collect(),
                    ClipKind::Image => clip.ocr.chars().take(160).collect::<String>().if_empty("Image"),
                };
                item.icon = match clip.kind { ClipKind::Text => "edit-paste", ClipKind::Image => "image-x-generic" }.into();
                item.actions = vec!["copy".into(), "edit".into(), "remove".into(), if clip.pinned { "unpin".into() } else { "pin".into() }];
                item.mimetype = clip.mime.clone().unwrap_or_default();
                if let Some(path) = &clip.image_path {
                    item.preview = path.clone();
                    item.preview_type = "file".into();
                } else {
                    item.preview = clip.content.clone();
                    item.preview_type = "text".into();
                }
                if clip.kind == ClipKind::Image && self.store.config.clipboard_ocr { item.actions.push("ocr".into()); }
                if clip.pinned { item.state.push("pinned".into()); }
                item.state.push(match clip.kind { ClipKind::Text => "text", ClipKind::Image => "image" }.into());
                item.score = clipboard_score(items.len(), index, clip.pinned, score);
                item.fuzzyinfo = Some(info);
                out.push(item);
            }
        }
        out.sort_by_key(|item| std::cmp::Reverse(item.score));
        out.truncate(limit);
        out
    }

    fn activate(&mut self, identifier: &str, action: &str, _query: &str, _arguments: &str) -> Result<()> {
        match action {
            "copy" => {
                let items = self.store.items.lock().unwrap();
                if let Some(c) = items.iter().find(|i| i.id == identifier) { copy_clip(c)?; }
                Ok(())
            }
            "edit" => {
                let index = self.store.items.lock().unwrap().iter().position(|i| i.id == identifier);
                if let Some(index) = index { self.edit_clip(index)?; }
                Ok(())
            }
            "ocr" => {
                let mut items = self.store.items.lock().unwrap();
                if let Some(c) = items.iter_mut().find(|i| i.id == identifier) {
                    if let Some(path) = &c.image_path { c.ocr = run_ocr(Path::new(path)).unwrap_or_default(); }
                }
                self.store.save(&items)
            }
            "remove" => {
                let mut items = self.store.items.lock().unwrap();
                items.retain(|i| i.id != identifier);
                self.store.save(&items)
            }
            "remove_all" => {
                let mut items = self.store.items.lock().unwrap();
                items.clear();
                self.store.save(&items)
            }
            "pin" => {
                let mut items = self.store.items.lock().unwrap();
                if let Some(c) = items.iter_mut().find(|i| i.id == identifier) { c.pinned = true; }
                self.store.compact(&mut items)
            }
            "unpin" => {
                let mut items = self.store.items.lock().unwrap();
                if let Some(c) = items.iter_mut().find(|i| i.id == identifier) { c.pinned = false; }
                self.store.compact(&mut items)
            }
            _ => anyhow::bail!("unsupported clipboard action: {action}"),
        }
    }

    fn capability(&self) -> ProviderCapability {
        ProviderCapability {
            name: self.name().into(),
            name_pretty: self.pretty_name().into(),
            description: "Search text/image clipboard history with optional OCR".into(),
            icon: String::new(),
            prefixes: Vec::new(),
            actions: action_map(&[
                ("copy", ActionCapability::new("Copy")),
                ("edit", ActionCapability::new("Edit").async_action()),
                ("ocr", ActionCapability::new("OCR").async_action()),
                ("pin", ActionCapability::new("Pin")),
                ("unpin", ActionCapability::new("Unpin")),
                ("remove", ActionCapability::new("Remove").destructive()),
                ("remove_all", ActionCapability::new("Remove All").destructive()),
            ]),
            supports_query: true,
            supports_activate: true,
            supports_streaming: true,
            supports_subscriptions: false,
            emits_events: false,
        }
    }
}

impl ClipboardProvider {
    fn edit_clip(&mut self, index: usize) -> Result<()> {
        let kind = self.store.items.lock().unwrap()[index].kind.clone();
        match kind {
            ClipKind::Text => {
                let (id, content) = {
                    let items = self.store.items.lock().unwrap();
                    (items[index].id.clone(), items[index].content.clone())
                };
                let path = std::env::temp_dir().join(format!("epochoxide-{id}.txt"));
                fs::write(&path, &content)?;
                run_editor(&self.store.config.clipboard_text_editor, &path)?;
                let edited = fs::read_to_string(&path)?;
                let mut items = self.store.items.lock().unwrap();
                items[index].content = edited;
                items[index].id = format!("text-{:x}", stable_hash(items[index].content.as_bytes()));
                let content = items[index].content.clone();
                self.store.save(&items)?;
                drop(items);
                copy_text(&content)
            }
            ClipKind::Image => {
                let path = self.store.items.lock().unwrap()[index].image_path.clone();
                let Some(path) = path else { return Ok(()); };
                let editor = if self.store.config.clipboard_image_editor.is_empty() { "xdg-open" } else { &self.store.config.clipboard_image_editor };
                run_editor(editor, Path::new(&path))
            }
        }
    }
}

fn current_clipboard_image_mime() -> Option<String> {
    let out = Command::new("wl-paste").arg("--list-types").output().ok()?;
    if !out.status.success() { return None; }
    let types = String::from_utf8_lossy(&out.stdout);
    types.lines().find(|t| t.starts_with("image/")).map(str::to_string)
}

fn copy_clip(clip: &Clip) -> Result<()> {
    match clip.kind {
        ClipKind::Text => copy_text(&clip.content),
        ClipKind::Image => {
            let path = clip.image_path.as_ref().context("image clip has no file")?;
            let mime = clip.mime.as_deref().unwrap_or("image/png");
            run_shell(&format!("wl-copy --type '{}' < '{}'", shell_quote(mime), shell_quote(path)))
        }
    }
}

fn copy_text(text: &str) -> Result<()> {
    let mut child = Command::new("wl-copy").stdin(Stdio::piped()).spawn()?;
    child.stdin.as_mut().context("clipboard stdin unavailable")?.write_all(text.as_bytes())?;
    super::reap(child);
    Ok(())
}

fn run_editor(editor: &str, path: &Path) -> Result<()> {
    Command::new("sh").arg("-c").arg(format!("{} '{}'", editor, shell_quote(&path.display().to_string()))).status()?;
    Ok(())
}

fn run_ocr(path: &Path) -> Result<String> {
    let out = Command::new("tesseract").arg(path).arg("stdout").output()?;
    if !out.status.success() { return Ok(String::new()); }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn image_extension(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        _ => "png",
    }
}

fn shell_quote(s: &str) -> String { s.replace('\'', "'\\''") }

fn stable_hash(bytes: &[u8]) -> u64 {
    let mut hash = 1469598103934665603u64;
    for b in bytes { hash ^= *b as u64; hash = hash.wrapping_mul(1099511628211); }
    hash
}

trait IfEmpty { fn if_empty(self, fallback: &str) -> String; }

impl IfEmpty for String {
    fn if_empty(self, fallback: &str) -> String {
        if self.is_empty() { fallback.to_string() } else { self }
    }
}

fn clipboard_score(total: usize, index: usize, pinned: bool, fuzzy_score: i32) -> i32 {
    let recency = total.saturating_sub(index) as i32;
    (if pinned { 50_000 } else { 0 }) + recency * 100 + fuzzy_score.min(99)
}

#[cfg(test)]
mod tests {
    use super::{clipboard_score, image_extension, stable_hash};

    #[test]
    fn image_mime_maps_to_extension() {
        assert_eq!(image_extension("image/jpeg"), "jpg");
        assert_eq!(image_extension("image/png"), "png");
    }

    #[test]
    fn hash_is_stable() {
        assert_eq!(stable_hash(b"abc"), stable_hash(b"abc"));
    }

    #[test]
    fn clipboard_score_prefers_recency_over_fuzzy_score() {
        assert!(clipboard_score(2, 0, false, 1) > clipboard_score(2, 1, false, 10_000));
    }

    #[test]
    fn clipboard_score_keeps_pinned_above_unpinned() {
        assert!(clipboard_score(2, 1, true, 1) > clipboard_score(2, 0, false, 10_000));
    }
}
