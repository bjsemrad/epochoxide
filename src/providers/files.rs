use super::{run_shell, Provider};
use crate::{config::{expand, Config}, fuzzy, types::{Item, ItemType}};
use anyhow::{Context, Result};
use std::{path::{Path, PathBuf}, process::{Command, Stdio}};

#[derive(Clone)]
struct IndexedFile { path: PathBuf, display: String, search: String }

pub struct FilesProvider { config: Config, files: Vec<IndexedFile> }

impl FilesProvider {
    pub fn new(config: Config) -> Self {
        let mut this = Self { config, files: Vec::new() };
        this.reindex();
        this
    }

    fn reindex(&mut self) {
        self.files.clear();
        let ignored = self.config.ignored_dirs.iter().map(|p| expand(p)).collect::<Vec<_>>();
        for root in &self.config.file_roots {
            let root = PathBuf::from(expand(root));
            if !root.exists() { continue; }
            for entry in walkdir::WalkDir::new(root).follow_links(false).into_iter().filter_entry(|e| {
                let p = e.path().display().to_string();
                !ignored.iter().any(|i| p == *i || p.contains(i))
            }).filter_map(|e| e.ok()) {
                let path = entry.path().to_path_buf();
                let display = path.display().to_string();
                let search = display.to_lowercase();
                self.files.push(IndexedFile { path, display, search });
            }
        }
    }
}

impl Provider for FilesProvider {
    fn name(&self) -> &'static str { "files" }
    fn pretty_name(&self) -> &'static str { "Files" }

    fn query(&mut self, query: &str, limit: usize, exact: bool) -> Vec<Item> {
        let mut out = Vec::new();
        for f in &self.files {
            if let Some((score, info)) = fuzzy::score(query, &f.search, exact, "text") {
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
        out.sort_by(|a, b| b.score.cmp(&a.score));
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
}

fn copy_text(text: &str) -> Result<()> {
    let mut child = Command::new("wl-copy").stdin(Stdio::piped()).spawn().or_else(|_| Command::new("xclip").args(["-selection", "clipboard"]).stdin(Stdio::piped()).spawn())?;
    use std::io::Write;
    child.stdin.as_mut().context("clipboard stdin unavailable")?.write_all(text.as_bytes())?;
    Ok(())
}
