//! Screen capture.
//!
//! Screenshots are taken with `grim`, regions are chosen with `slurp`, the result is put on the
//! clipboard with `wl-copy`, and the user is told about it with `notify-send` -- which the shell
//! itself answers, since EpochShell is the session's notification server.
//!
//! Three things are deliberately kept out of here:
//!
//! 1. **Compositor knowledge.** Window capture needs a rectangle, and that comes from the
//!    normalized [`compositor`] layer, not from `hyprctl`. A compositor that does not report
//!    on-screen geometry says so, and the caller is told to draw a region instead of being handed
//!    coordinates that mean something else.
//! 2. **UI.** Nothing here draws. The notification is the whole of the feedback, and it carries
//!    the file path so the shell can render a thumbnail of the shot.
//! 3. **Policy.** Whether to copy, save, or notify is the caller's to decide; the config only
//!    supplies the defaults for the flags a caller leaves unset.

use crate::compositor;
use crate::config::Config;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

/// What to point the camera at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// A rectangle the user drags out.
    #[default]
    Region,
    /// One window, taken from compositor geometry.
    Window,
    /// One whole monitor -- the focused one unless another is named.
    #[serde(alias = "screen", alias = "monitor", alias = "output")]
    Fullscreen,
    /// Every monitor, as one image of the whole output layout.
    #[serde(alias = "everything")]
    All,
}

impl Mode {
    pub fn parse(name: &str) -> Result<Self> {
        match name.trim().to_lowercase().as_str() {
            "region" | "area" | "select" => Ok(Self::Region),
            "window" | "active" => Ok(Self::Window),
            "fullscreen" | "screen" | "monitor" | "output" | "display" => Ok(Self::Fullscreen),
            "all" | "everything" | "desktop" => Ok(Self::All),
            other => bail!("unknown screenshot mode \"{other}\" (region, window, fullscreen, all)"),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Region => "region",
            Self::Window => "window",
            Self::Fullscreen => "fullscreen",
            Self::All => "all",
        }
    }
}

/// One screenshot request. Every `Option` means "use the configured default".
#[derive(Debug, Clone, Default)]
pub struct Request {
    pub mode: Mode,
    /// Monitor name for `Fullscreen`, from `compositor.monitors`.
    pub output: Option<String>,
    /// In `Window` mode, click a window instead of capturing the focused one.
    pub select: bool,
    /// Include the mouse pointer.
    pub cursor: bool,
    /// Wait this long before capturing, after any selection is made.
    pub delay: Duration,
    pub copy: Option<bool>,
    pub save: Option<bool>,
    pub notify: Option<bool>,
    /// Where to save, overriding `screenshot_dir`.
    pub directory: Option<PathBuf>,
}

/// What a capture produced. A cancelled selection is a result, not an error: pressing Escape is
/// how people change their mind, and a keybinding should not report a failure for it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Shot {
    pub cancelled: bool,
    pub mode: String,
    /// Where the image is. Present even when `saved` is false, because a copy-only shot still
    /// leaves a scratch file behind for the notification thumbnail to use.
    pub path: Option<String>,
    pub saved: bool,
    pub copied: bool,
    pub notified: bool,
    /// The captured rectangle as `x,y WxH`, when one was chosen.
    pub geometry: Option<String>,
    pub output: Option<String>,
    /// Title of the captured window, in window mode.
    pub window: Option<String>,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
}

impl Shot {
    fn cancelled(mode: Mode) -> Self {
        Self {
            cancelled: true,
            mode: mode.as_str().to_string(),
            path: None,
            saved: false,
            copied: false,
            notified: false,
            geometry: None,
            output: None,
            window: None,
            width: 0,
            height: 0,
            bytes: 0,
        }
    }
}

/// One external tool capture needs, and what stops working without it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Tool {
    pub name: String,
    pub purpose: String,
    pub required: bool,
    pub path: Option<String>,
}

/// Everything a caller needs to explain the capture setup: where shots land, what the defaults
/// are, which tools are installed, and whether window capture can work at all here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Status {
    pub directory: String,
    pub filename: String,
    pub copy: bool,
    pub save: bool,
    pub notify: bool,
    pub tools: Vec<Tool>,
    /// False when the running compositor does not report window geometry, which is what
    /// `mode: "window"` needs.
    pub window_capture: bool,
    pub compositor: Option<String>,
}

const GRIM: &str = "grim";
const SLURP: &str = "slurp";
const WL_COPY: &str = "wl-copy";
const NOTIFY_SEND: &str = "notify-send";

/// How many copy-only screenshots to keep in the scratch directory.
///
/// A shot that is not being saved still has to exist as a file: `wl-copy` reads it, and the
/// notification points at it for its thumbnail. Deleting it the moment the copy is done races the
/// shell reading it, so the files are kept and the oldest are pruned instead.
const SCRATCH_KEPT: usize = 20;

#[derive(Debug, Clone)]
struct Settings {
    directory: PathBuf,
    filename: String,
    copy: bool,
    save: bool,
    notify: bool,
}

impl Default for Settings {
    fn default() -> Self {
        let config = Config::default();
        Self {
            directory: PathBuf::from(crate::config::expand(&config.screenshot_dir)),
            filename: config.screenshot_filename,
            copy: config.screenshot_copy,
            save: config.screenshot_save,
            notify: config.screenshot_notify,
        }
    }
}

static SETTINGS: OnceLock<Settings> = OnceLock::new();

pub fn configure(config: &Config) {
    let _ = SETTINGS.set(Settings {
        directory: PathBuf::from(crate::config::expand(&config.screenshot_dir)),
        filename: config.screenshot_filename.clone(),
        copy: config.screenshot_copy,
        save: config.screenshot_save,
        notify: config.screenshot_notify,
    });
}

fn settings() -> Settings {
    SETTINGS.get().cloned().unwrap_or_default()
}

/// Whether capture can work here. Reported by `api.describe` so a shell can hide capture actions
/// rather than offering something that will fail.
pub fn available() -> Result<(), String> {
    if which(GRIM).is_none() {
        return Err(format!("{GRIM} is not installed"));
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return Err("no Wayland display in this session".into());
    }
    Ok(())
}

pub fn status() -> Status {
    let settings = settings();
    let regions = compositor::window_regions();
    Status {
        directory: settings.directory.display().to_string(),
        filename: settings.filename,
        copy: settings.copy,
        save: settings.save,
        notify: settings.notify,
        tools: vec![
            tool(GRIM, "screen capture", true),
            tool(SLURP, "region and window selection", false),
            tool(WL_COPY, "copying shots to the clipboard", false),
            tool(NOTIFY_SEND, "capture notifications", false),
        ],
        window_capture: regions.is_some(),
        compositor: compositor::active().map(|backend| backend.name().to_string()),
    }
}

/// Take a screenshot, then copy it, save it, and announce it as the request asks.
pub fn screenshot(request: &Request) -> Result<Shot> {
    let settings = settings();
    let copy = request.copy.unwrap_or(settings.copy);
    let save = request.save.unwrap_or(settings.save);
    let notify = request.notify.unwrap_or(settings.notify);

    require(GRIM)?;

    // Selection happens before the delay so the delay is a chance to arrange what is on screen,
    // not dead time before the user is even asked what to capture.
    let mut window = None;
    let mut output = None;
    let geometry = match request.mode {
        Mode::Region => match select_region()? {
            Some(geometry) => Some(geometry),
            None => return Ok(Shot::cancelled(request.mode)),
        },
        Mode::Window => match window_region(request.select)? {
            Some(region) => {
                window = Some(region.title.clone());
                Some(region.geometry())
            }
            None => return Ok(Shot::cancelled(request.mode)),
        },
        Mode::Fullscreen => {
            // Falling back to the whole layout is better than refusing: a single-monitor session
            // whose compositor does not report a focused output still gets its screenshot.
            output = request.output.clone().or_else(compositor::focused_monitor);
            None
        }
        Mode::All => None,
    };

    if !request.delay.is_zero() {
        std::thread::sleep(request.delay);
    }

    let destination = if save {
        let directory = request
            .directory
            .clone()
            .unwrap_or_else(|| settings.directory.clone());
        std::fs::create_dir_all(&directory)
            .with_context(|| format!("creating {}", directory.display()))?;
        unique(&directory, &filename(&settings.filename))
    } else {
        let scratch = scratch_dir()?;
        prune(&scratch, SCRATCH_KEPT);
        unique(&scratch, &filename(&settings.filename))
    };

    grim(
        &destination,
        geometry.as_deref(),
        output.as_deref(),
        request.cursor,
    )?;

    let bytes = std::fs::metadata(&destination)
        .map(|meta| meta.len())
        .unwrap_or(0);
    let (width, height) = png_size(&destination).unwrap_or((0, 0));

    // A copy that fails is worth reporting rather than swallowing: the user asked for the shot to
    // be on the clipboard, and a silent "it worked" would send them pasting into nothing.
    let copied = if copy {
        copy_image(&destination).context("copying the screenshot to the clipboard")?;
        true
    } else {
        false
    };

    let shot = Shot {
        cancelled: false,
        mode: request.mode.as_str().to_string(),
        path: Some(destination.display().to_string()),
        saved: save,
        copied,
        // A missing notify-send is not a failed screenshot, so the notification is best-effort and
        // the result says whether it actually went out.
        notified: notify && announce(&destination, save, copied, width, height).is_ok(),
        geometry,
        output,
        window,
        width,
        height,
        bytes,
    };
    Ok(shot)
}

// --- Capture -----------------------------------------------------------------

fn grim(
    destination: &Path,
    geometry: Option<&str>,
    output: Option<&str>,
    cursor: bool,
) -> Result<()> {
    let mut command = Command::new(GRIM);
    if cursor {
        command.arg("-c");
    }
    if let Some(geometry) = geometry {
        command.args(["-g", geometry]);
    }
    if let Some(output) = output {
        command.args(["-o", output]);
    }
    // grim decides the format from the extension only for `-` output, so it is named explicitly.
    if let Some(format) = image_format(destination) {
        command.args(["-t", format]);
    }
    command.arg(destination);
    let out = command
        .output()
        .with_context(|| format!("running {GRIM}"))?;
    if !out.status.success() {
        let detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let detail = if detail.is_empty() {
            format!("exited with {}", out.status)
        } else {
            detail
        };
        bail!("{GRIM} failed: {detail}");
    }
    Ok(())
}

/// Ask the user to drag out a rectangle. `None` means they cancelled.
fn select_region() -> Result<Option<String>> {
    require(SLURP)?;
    slurp(&[], None)
}

/// The window to capture: the focused one, or one the user clicks when `select` is set.
fn window_region(select: bool) -> Result<Option<compositor::WindowRegion>> {
    let regions = compositor::window_regions().ok_or_else(|| {
        let backend = compositor::active()
            .map(|backend| backend.name().to_string())
            .unwrap_or_else(|| "this compositor".to_string());
        anyhow!(
            "{backend} does not report where its windows are on screen, \
             so a window cannot be captured by itself -- use region instead"
        )
    })?;
    if regions.is_empty() {
        bail!("no windows are on screen to capture");
    }
    if !select {
        return Ok(Some(
            regions
                .iter()
                .find(|region| region.focused)
                .cloned()
                // Nothing focused is normal right after the launcher takes focus as a layer
                // surface, and one window on screen is unambiguous anyway.
                .or_else(|| regions.first().cloned())
                .expect("regions is not empty"),
        ));
    }

    require(SLURP)?;
    let boxes: Vec<String> = regions.iter().map(|region| region.geometry()).collect();
    let Some(chosen) = slurp(&["-r"], Some(&boxes.join("\n")))? else {
        return Ok(None);
    };
    Ok(regions
        .iter()
        .find(|region| region.geometry() == chosen)
        .cloned()
        // slurp lets the user drag a fresh rectangle even in restricted mode, so a selection that
        // matches no window is still a selection -- it is just not a window.
        .or_else(|| parse_geometry(&chosen).ok()))
}

/// Run slurp, optionally feeding it the boxes it should restrict the selection to.
///
/// slurp exits non-zero when the user presses Escape, which is a cancellation rather than a
/// failure; it is told apart from a real error by having said nothing on stdout.
fn slurp(args: &[&str], boxes: Option<&str>) -> Result<Option<String>> {
    let mut command = Command::new(SLURP);
    command.args(args);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.stdin(if boxes.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    let mut child = command
        .spawn()
        .with_context(|| format!("running {SLURP}"))?;
    if let Some(boxes) = boxes {
        child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("{SLURP} stdin unavailable"))?
            .write_all(boxes.as_bytes())?;
        drop(child.stdin.take());
    }
    let out = child.wait_with_output()?;
    let selection = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if selection.is_empty() {
        let detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
        // "selection cancelled" is slurp's own wording for Escape.
        if out.status.success() || detail.is_empty() || detail.contains("cancelled") {
            return Ok(None);
        }
        bail!("{SLURP} failed: {detail}");
    }
    Ok(Some(selection))
}

/// Parse slurp's `x,y WxH` back into a rectangle.
fn parse_geometry(geometry: &str) -> Result<compositor::WindowRegion> {
    let invalid = || anyhow!("could not read the selected geometry \"{geometry}\"");
    let (position, size) = geometry.trim().split_once(' ').ok_or_else(invalid)?;
    let (x, y) = position.split_once(',').ok_or_else(invalid)?;
    let (width, height) = size.split_once('x').ok_or_else(invalid)?;
    Ok(compositor::WindowRegion {
        id: String::new(),
        app_id: String::new(),
        title: String::new(),
        monitor: String::new(),
        focused: false,
        x: x.trim().parse().map_err(|_| invalid())?,
        y: y.trim().parse().map_err(|_| invalid())?,
        width: width.trim().parse().map_err(|_| invalid())?,
        height: height.trim().parse().map_err(|_| invalid())?,
    })
}

// --- Afterwards ---------------------------------------------------------------

fn copy_image(path: &Path) -> Result<()> {
    require(WL_COPY)?;
    let data = std::fs::read(path)?;
    let mime = match image_format(path) {
        Some("jpeg") => "image/jpeg",
        Some("ppm") => "image/x-portable-pixmap",
        _ => "image/png",
    };
    let mut child = Command::new(WL_COPY)
        .args(["--type", mime])
        .stdin(Stdio::piped())
        .spawn()
        .with_context(|| format!("running {WL_COPY}"))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| anyhow!("clipboard stdin unavailable"))?
        .write_all(&data)?;
    drop(child.stdin.take());
    // wl-copy forks a background process to hold the selection and exits, so waiting here costs
    // nothing and turns "the clipboard tool is broken" into a reported failure.
    let status = child.wait()?;
    if !status.success() {
        bail!("{WL_COPY} exited with {status}");
    }
    Ok(())
}

/// Tell the session a shot was taken. The image path goes along as a hint so the shell can show
/// the shot itself rather than a generic camera icon.
fn announce(path: &Path, saved: bool, copied: bool, width: u32, height: u32) -> Result<()> {
    require(NOTIFY_SEND)?;
    let summary = match (saved, copied) {
        (true, true) => "Screenshot saved and copied",
        (true, false) => "Screenshot saved",
        (false, true) => "Screenshot copied",
        (false, false) => "Screenshot taken",
    };
    let mut body = if saved {
        path.file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default()
    } else {
        String::new()
    };
    if width > 0 && height > 0 {
        if !body.is_empty() {
            body.push_str(" · ");
        }
        body.push_str(&format!("{width}×{height}"));
    }
    let status = Command::new(NOTIFY_SEND)
        .arg("--app-name=EpochShell")
        .arg("--icon=camera-photo")
        .arg(format!("--hint=string:image-path:{}", path.display()))
        // Successive shots replace each other in the toast stack instead of stacking up.
        .arg("--hint=string:x-canonical-private-synchronous:epoch-screenshot")
        .arg(summary)
        .arg(body)
        .status()
        .with_context(|| format!("running {NOTIFY_SEND}"))?;
    if !status.success() {
        bail!("{NOTIFY_SEND} exited with {status}");
    }
    Ok(())
}

// --- Files --------------------------------------------------------------------

/// Where copy-only shots live: they are scratch, not part of the user's screenshot collection.
fn scratch_dir() -> Result<PathBuf> {
    let directory = dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("epochoxide/screenshots");
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("creating {}", directory.display()))?;
    Ok(directory)
}

/// Keep the `keep` newest files in `directory` and delete the rest.
fn prune(directory: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, entry.path()))
        })
        .collect();
    if files.len() <= keep {
        return;
    }
    files.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    for (_, path) in files.into_iter().skip(keep) {
        let _ = std::fs::remove_file(path);
    }
}

/// Expand the configured filename template through `date`, which is what makes `%Y-%m-%d` mean
/// what the user expects in their own timezone.
fn filename(template: &str) -> String {
    let expanded = Command::new("date")
        .arg(format!("+{template}"))
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default();
    // A template that expands to nothing, or to something with a path separator in it, would put
    // the file somewhere the caller did not ask for.
    let name = if expanded.is_empty() || expanded.contains('/') {
        format!("screenshot-{}", epoch_seconds())
    } else {
        expanded
    };
    if image_format(Path::new(&name)).is_some() {
        name
    } else {
        format!("{name}.png")
    }
}

fn epoch_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

/// `directory/name`, with `-1`, `-2`, … appended until nothing is overwritten.
fn unique(directory: &Path, name: &str) -> PathBuf {
    let candidate = directory.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let path = Path::new(name);
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_else(|| name.to_string());
    let extension = path
        .extension()
        .map(|extension| format!(".{}", extension.to_string_lossy()))
        .unwrap_or_default();
    for index in 1..1000 {
        let candidate = directory.join(format!("{stem}-{index}{extension}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    directory.join(format!("{stem}-{}{extension}", epoch_seconds()))
}

/// The grim format name for a path's extension, or `None` when it names no image format.
fn image_format(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_string_lossy().to_lowercase().as_str() {
        "png" => Some("png"),
        "jpg" | "jpeg" => Some("jpeg"),
        "ppm" => Some("ppm"),
        _ => None,
    }
}

/// Read a PNG's dimensions out of its IHDR chunk, so the notification can say how big the shot is
/// without decoding the image.
fn png_size(path: &Path) -> Option<(u32, u32)> {
    use std::io::Read;
    let mut header = [0u8; 24];
    std::fs::File::open(path)
        .ok()?
        .read_exact(&mut header)
        .ok()?;
    if &header[..8] != b"\x89PNG\r\n\x1a\n" || &header[12..16] != b"IHDR" {
        return None;
    }
    let read = |at: usize| {
        u32::from_be_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]])
    };
    Some((read(16), read(20)))
}

// --- Tools --------------------------------------------------------------------

fn which(binary: &str) -> Option<PathBuf> {
    which::which(binary).ok()
}

fn tool(name: &str, purpose: &str, required: bool) -> Tool {
    Tool {
        name: name.to_string(),
        purpose: purpose.to_string(),
        required,
        path: which(name).map(|path| path.display().to_string()),
    }
}

fn require(binary: &str) -> Result<()> {
    if which(binary).is_some() {
        return Ok(());
    }
    bail!("{binary} is not installed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_take_the_names_people_actually_type() {
        assert_eq!(Mode::parse("region").unwrap(), Mode::Region);
        assert_eq!(Mode::parse("Window").unwrap(), Mode::Window);
        assert_eq!(Mode::parse("monitor").unwrap(), Mode::Fullscreen);
        assert_eq!(Mode::parse(" all ").unwrap(), Mode::All);
        assert!(Mode::parse("panorama").is_err());
    }

    #[test]
    fn a_geometry_round_trips_through_slurps_spelling() {
        let region = parse_geometry("1920,40 1280x800").expect("parsed");
        assert_eq!((region.x, region.y), (1920, 40));
        assert_eq!((region.width, region.height), (1280, 800));
        assert_eq!(region.geometry(), "1920,40 1280x800");
    }

    #[test]
    fn a_negative_origin_is_a_real_position_not_an_error() {
        // A monitor left of the primary one has negative coordinates in the output layout.
        let region = parse_geometry("-2060,46 2102x1388").expect("parsed");
        assert_eq!((region.x, region.y), (-2060, 46));
    }

    #[test]
    fn malformed_geometry_is_refused() {
        assert!(parse_geometry("").is_err());
        assert!(parse_geometry("1920,40").is_err());
        assert!(parse_geometry("1920 1280x800").is_err());
    }

    #[test]
    fn a_filename_template_always_produces_an_image_name() {
        assert_eq!(filename("shot.png"), "shot.png");
        assert_eq!(filename("shot.jpg"), "shot.jpg");
        // No extension, so one is added rather than leaving grim to guess.
        assert_eq!(filename("shot"), "shot.png");
    }

    #[test]
    fn a_template_that_would_escape_the_directory_falls_back() {
        let name = filename("../../%Y/shot.png");
        assert!(!name.contains('/'), "{name} still has a path separator");
        assert!(name.ends_with(".png"));
    }

    #[test]
    fn a_date_template_is_expanded() {
        let name = filename("screenshot-%Y.png");
        assert!(
            name.starts_with("screenshot-2"),
            "{name} kept its literal %Y"
        );
    }

    #[test]
    fn names_do_not_collide_with_what_is_already_there() {
        let directory = tempfile::tempdir().expect("tempdir");
        let first = unique(directory.path(), "shot.png");
        assert_eq!(first.file_name().unwrap(), "shot.png");
        std::fs::write(&first, b"").expect("write");
        let second = unique(directory.path(), "shot.png");
        assert_eq!(second.file_name().unwrap(), "shot-1.png");
    }

    #[test]
    fn pruning_keeps_the_newest_and_leaves_a_small_directory_alone() {
        let directory = tempfile::tempdir().expect("tempdir");
        for index in 0..5 {
            std::fs::write(directory.path().join(format!("{index}.png")), b"").expect("write");
            // Distinct modification times, which is what the prune orders on.
            std::thread::sleep(Duration::from_millis(10));
        }
        prune(directory.path(), 10);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 5);
        prune(directory.path(), 2);
        let left: Vec<String> = std::fs::read_dir(directory.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(left.len(), 2);
        assert!(
            left.contains(&"4.png".to_string()),
            "newest was pruned: {left:?}"
        );
    }

    #[test]
    fn png_dimensions_come_from_the_header() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("shot.png");
        let mut header = Vec::from(b"\x89PNG\r\n\x1a\n");
        header.extend_from_slice(&13u32.to_be_bytes());
        header.extend_from_slice(b"IHDR");
        header.extend_from_slice(&2102u32.to_be_bytes());
        header.extend_from_slice(&1388u32.to_be_bytes());
        std::fs::write(&path, &header).expect("write");
        assert_eq!(png_size(&path), Some((2102, 1388)));
    }

    #[test]
    fn a_file_that_is_not_a_png_reports_no_size() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("shot.png");
        std::fs::write(&path, b"not an image at all, but long enough to read").expect("write");
        assert_eq!(png_size(&path), None);
    }

    #[test]
    fn the_image_format_follows_the_extension() {
        assert_eq!(image_format(Path::new("a.png")), Some("png"));
        assert_eq!(image_format(Path::new("a.JPEG")), Some("jpeg"));
        assert_eq!(image_format(Path::new("a.txt")), None);
    }
}
