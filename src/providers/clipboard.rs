use super::{run_shell, Provider};
use crate::{config::Config, fuzzy, types::Item};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{fs, io::Write, path::{Path, PathBuf}, process::{Command, Stdio}, time::{Duration, Instant}};

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

pub struct ClipboardProvider {
    config: Config,
    items: Vec<Clip>,
    cache: PathBuf,
    last_capture: Option<Instant>,
}

impl ClipboardProvider {
    pub fn new(config: Config) -> Result<Self> {
        let cache = dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("epochoxide/clipboard.json");
        let items = fs::read_to_string(&cache).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        Ok(Self { config, items, cache, last_capture: None })
    }

    fn capture_current(&mut self) {
        if self.last_capture.map(|t| t.elapsed() < Duration::from_millis(self.config.clipboard_capture_interval_ms)).unwrap_or(false) {
            return;
        }
        self.last_capture = Some(Instant::now());

        let Some(mime) = current_clipboard_mime() else { return; };
        if mime.starts_with("image/") {
            let _ = self.capture_image(&mime);
        } else if mime.starts_with("text/") || mime == "UTF8_STRING" || mime == "STRING" {
            let _ = self.capture_text();
        }
    }

    fn capture_text(&mut self) -> Result<()> {
        let out = Command::new("wl-paste").arg("--no-newline").output()?;
        if !out.status.success() { return Ok(()); }
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        if text.trim().is_empty() || self.items.first().map(|i| i.content.as_str()) == Some(text.as_str()) { return Ok(()); }
        let id = format!("text-{:x}", stable_hash(text.as_bytes()));
        self.items.retain(|i| i.id != id);
        self.items.insert(0, Clip { id, kind: ClipKind::Text, content: text, image_path: None, mime: Some("text/plain".into()), ocr: String::new(), pinned: false });
        self.compact()?;
        Ok(())
    }

    fn capture_image(&mut self, mime: &str) -> Result<()> {
        let out = Command::new("wl-paste").args(["--type", mime]).output()?;
        if !out.status.success() || out.stdout.is_empty() { return Ok(()); }
        let id = format!("image-{:x}", stable_hash(&out.stdout));
        if self.items.first().map(|i| i.id.as_str()) == Some(id.as_str()) { return Ok(()); }
        fs::create_dir_all(&self.config.clipboard_image_dir)?;
        let ext = image_extension(mime);
        let path = Path::new(&self.config.clipboard_image_dir).join(format!("{id}.{ext}"));
        fs::write(&path, &out.stdout)?;
        let ocr = if self.config.clipboard_ocr { run_ocr(&path).unwrap_or_default() } else { String::new() };
        self.items.retain(|i| i.id != id);
        self.items.insert(0, Clip {
            id,
            kind: ClipKind::Image,
            content: if ocr.is_empty() { "Image clipboard item".into() } else { ocr.lines().next().unwrap_or("Image clipboard item").to_string() },
            image_path: Some(path.display().to_string()),
            mime: Some(mime.to_string()),
            ocr,
            pinned: false,
        });
        self.compact()?;
        Ok(())
    }

    fn compact(&mut self) -> Result<()> {
        self.items.sort_by_key(|c| !c.pinned);
        self.items.truncate(self.config.clipboard_max_items);
        self.save()
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.cache.parent() { fs::create_dir_all(parent)?; }
        fs::write(&self.cache, serde_json::to_vec(&self.items)?)?;
        Ok(())
    }
}

impl Provider for ClipboardProvider {
    fn name(&self) -> &'static str { "clipboard" }
    fn pretty_name(&self) -> &'static str { "Clipboard" }

    fn query(&mut self, query: &str, limit: usize, exact: bool) -> Vec<Item> {
        self.capture_current();
        let mut out = Vec::new();
        for clip in &self.items {
            let searchable = format!("{} {}", clip.content, clip.ocr);
            if let Some((score, info)) = fuzzy::score(query, &searchable, exact, "text") {
                let mut item = Item::new(self.name(), &clip.id, clip.content.lines().next().unwrap_or_default());
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
                if clip.kind == ClipKind::Image && self.config.clipboard_ocr { item.actions.push("ocr".into()); }
                if clip.pinned { item.state.push("pinned".into()); }
                item.state.push(match clip.kind { ClipKind::Text => "text", ClipKind::Image => "image" }.into());
                item.score = score + if clip.pinned { 50_000 } else { 0 };
                item.fuzzyinfo = Some(info);
                out.push(item);
            }
        }
        out.sort_by(|a, b| b.score.cmp(&a.score));
        out.truncate(limit);
        out
    }

    fn activate(&mut self, identifier: &str, action: &str, _query: &str, _arguments: &str) -> Result<()> {
        match action {
            "copy" => {
                if let Some(c) = self.items.iter().find(|i| i.id == identifier) { copy_clip(c)?; }
                Ok(())
            }
            "edit" => {
                if let Some(index) = self.items.iter().position(|i| i.id == identifier) { self.edit_clip(index)?; }
                Ok(())
            }
            "ocr" => {
                if let Some(c) = self.items.iter_mut().find(|i| i.id == identifier) {
                    if let Some(path) = &c.image_path { c.ocr = run_ocr(Path::new(path)).unwrap_or_default(); }
                }
                self.save()
            }
            "remove" => { self.items.retain(|i| i.id != identifier); self.save() }
            "remove_all" => { self.items.clear(); self.save() }
            "pin" => { if let Some(c) = self.items.iter_mut().find(|i| i.id == identifier) { c.pinned = true; } self.compact() }
            "unpin" => { if let Some(c) = self.items.iter_mut().find(|i| i.id == identifier) { c.pinned = false; } self.compact() }
            _ => anyhow::bail!("unsupported clipboard action: {action}"),
        }
    }
}

impl ClipboardProvider {
    fn edit_clip(&mut self, index: usize) -> Result<()> {
        match self.items[index].kind {
            ClipKind::Text => {
                let path = std::env::temp_dir().join(format!("epochoxide-{}.txt", self.items[index].id));
                fs::write(&path, &self.items[index].content)?;
                run_editor(&self.config.clipboard_text_editor, &path)?;
                let edited = fs::read_to_string(&path)?;
                self.items[index].content = edited;
                self.items[index].id = format!("text-{:x}", stable_hash(self.items[index].content.as_bytes()));
                copy_text(&self.items[index].content)?;
                self.save()
            }
            ClipKind::Image => {
                let Some(path) = self.items[index].image_path.clone() else { return Ok(()); };
                let editor = if self.config.clipboard_image_editor.is_empty() { "xdg-open" } else { &self.config.clipboard_image_editor };
                run_editor(editor, Path::new(&path))
            }
        }
    }
}

fn current_clipboard_mime() -> Option<String> {
    let out = Command::new("wl-paste").arg("--list-types").output().ok()?;
    if !out.status.success() { return None; }
    let types = String::from_utf8_lossy(&out.stdout);
    types.lines().find(|t| t.starts_with("image/")).or_else(|| types.lines().find(|t| t.starts_with("text/") || *t == "UTF8_STRING" || *t == "STRING")).map(str::to_string)
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

#[cfg(test)]
mod tests {
    use super::{image_extension, stable_hash};

    #[test]
    fn image_mime_maps_to_extension() {
        assert_eq!(image_extension("image/jpeg"), "jpg");
        assert_eq!(image_extension("image/png"), "png");
    }

    #[test]
    fn hash_is_stable() {
        assert_eq!(stable_hash(b"abc"), stable_hash(b"abc"));
    }
}
