use crate::compositor;
use crate::config::Config;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Region,
    Window,
    #[serde(alias = "screen", alias = "monitor", alias = "output")]
    Fullscreen,
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

#[derive(Debug, Clone, Default)]
pub struct Request {
    pub mode: Mode,
    pub output: Option<String>,
    pub select: bool,
    pub cursor: bool,
    pub delay: Duration,
    pub copy: Option<bool>,
    pub save: Option<bool>,
    pub notify: Option<bool>,
    pub annotate: Option<bool>,
    pub directory: Option<PathBuf>,
    pub language: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Shot {
    pub cancelled: bool,
    pub mode: String,
    pub path: Option<String>,
    pub saved: bool,
    pub copied: bool,
    pub notified: bool,
    pub geometry: Option<String>,
    pub output: Option<String>,
    pub window: Option<String>,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
    #[serde(default)]
    pub annotating: bool,
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
            annotating: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Text {
    pub cancelled: bool,
    pub mode: String,
    pub text: String,
    pub characters: usize,
    pub lines: usize,
    pub copied: bool,
    pub notified: bool,
    pub language: String,
    pub geometry: Option<String>,
    pub path: Option<String>,
    pub saved: bool,
}

impl Text {
    fn cancelled(mode: Mode, language: &str) -> Self {
        Self {
            cancelled: true,
            mode: mode.as_str().to_string(),
            text: String::new(),
            characters: 0,
            lines: 0,
            copied: false,
            notified: false,
            language: language.to_string(),
            geometry: None,
            path: None,
            saved: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Session {
    pub recording: bool,
    pub cancelled: bool,
    pub mode: String,
    pub path: Option<String>,
    pub geometry: Option<String>,
    pub output: Option<String>,
    pub seconds: u64,
    pub bytes: u64,
    pub notified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Tool {
    pub name: String,
    pub purpose: String,
    pub required: bool,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Status {
    pub directory: String,
    pub filename: String,
    pub copy: bool,
    pub save: bool,
    pub notify: bool,
    pub tools: Vec<Tool>,
    pub window_capture: bool,
    pub ocr: bool,
    pub ocr_language: String,
    pub record: bool,
    #[serde(default)]
    pub annotate: bool,
    pub recording_directory: String,
    pub compositor: Option<String>,
}

const GRIM: &str = "grim";
const SLURP: &str = "slurp";
const WL_COPY: &str = "wl-copy";
const TESSERACT: &str = "tesseract";
const WF_RECORDER: &str = "wf-recorder";
const SATTY: &str = "satty";

/// Copy-only shots stay on disk briefly: wl-copy and the notification thumbnail read the file, so
/// deleting it at once races them.
const SCRATCH_KEPT: usize = 20;

#[derive(Debug, Clone)]
struct Settings {
    directory: PathBuf,
    filename: String,
    copy: bool,
    save: bool,
    notify: bool,
    annotate: bool,
    ocr_language: String,
    recording_directory: PathBuf,
    recording_filename: String,
    recording_notify: bool,
    recording_framerate: u32,
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
            annotate: config.screenshot_annotate,
            ocr_language: config.ocr_language,
            recording_directory: PathBuf::from(crate::config::expand(&config.recording_dir)),
            recording_filename: config.recording_filename,
            recording_notify: config.recording_notify,
            recording_framerate: config.recording_framerate,
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
        annotate: config.screenshot_annotate,
        ocr_language: config.ocr_language.clone(),
        recording_directory: PathBuf::from(crate::config::expand(&config.recording_dir)),
        recording_filename: config.recording_filename.clone(),
        recording_notify: config.recording_notify,
        recording_framerate: config.recording_framerate,
    });
}

fn settings() -> Settings {
    SETTINGS.get().cloned().unwrap_or_default()
}

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
            tool("notify-send", "capture notifications", false),
            tool(TESSERACT, "reading text out of a capture", false),
            tool(WF_RECORDER, "screen recording", false),
            tool(SATTY, "annotating screenshots", false),
        ],
        window_capture: regions.is_some(),
        ocr: which(TESSERACT).is_some(),
        ocr_language: settings.ocr_language,
        record: which(WF_RECORDER).is_some(),
        annotate: which(SATTY).is_some(),
        recording_directory: settings.recording_directory.display().to_string(),
        compositor: compositor::active().map(|backend| backend.name().to_string()),
    }
}

struct Frame {
    path: PathBuf,
    geometry: Option<String>,
    output: Option<String>,
    window: Option<String>,
    width: u32,
    height: u32,
    bytes: u64,
}

#[derive(Debug, Clone, Default)]
struct Target {
    geometry: Option<String>,
    output: Option<String>,
    window: Option<String>,
}

fn choose_target(request: &Request) -> Result<Option<Target>> {
    let mut target = Target::default();
    match request.mode {
        Mode::Region => match select_region()? {
            Some(geometry) => target.geometry = Some(geometry),
            None => return Ok(None),
        },
        Mode::Window => match window_region(request.select)? {
            Some(region) => {
                target.window = Some(region.title.clone());
                target.geometry = Some(region.geometry());
            }
            None => return Ok(None),
        },
        Mode::Fullscreen => {
            target.output = request.output.clone().or_else(compositor::focused_monitor);
        }
        Mode::All => {}
    }
    Ok(Some(target))
}

fn capture(request: &Request, save: bool) -> Result<Option<Frame>> {
    let settings = settings();
    require(GRIM)?;

    let Some(target) = choose_target(request)? else {
        return Ok(None);
    };
    let Target {
        geometry,
        output,
        window,
    } = target;

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
        unique(&directory, &filename(&settings.filename, "png"))
    } else {
        let scratch = scratch_dir()?;
        prune(&scratch, SCRATCH_KEPT);
        unique(&scratch, &filename(&settings.filename, "png"))
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
    Ok(Some(Frame {
        path: destination,
        geometry,
        output,
        window,
        width,
        height,
        bytes,
    }))
}

pub fn screenshot(request: &Request) -> Result<Shot> {
    let settings = settings();
    let copy = request.copy.unwrap_or(settings.copy);
    let save = request.save.unwrap_or(settings.save);
    let notify = request.notify.unwrap_or(settings.notify);
    let annotate = request.annotate.unwrap_or(settings.annotate);

    if annotate {
        return annotate_shot(request, copy, save);
    }

    let Some(frame) = capture(request, save)? else {
        return Ok(Shot::cancelled(request.mode));
    };

    let copied = if copy {
        copy_image(&frame.path).context("copying the screenshot to the clipboard")?;
        true
    } else {
        false
    };

    let shot = Shot {
        cancelled: false,
        mode: request.mode.as_str().to_string(),
        path: Some(frame.path.display().to_string()),
        saved: save,
        copied,
        notified: notify && announce_shot(&frame, save, copied).is_ok(),
        geometry: frame.geometry,
        output: frame.output,
        window: frame.window,
        width: frame.width,
        height: frame.height,
        bytes: frame.bytes,
        annotating: false,
    };
    Ok(shot)
}

fn annotate_shot(request: &Request, copy: bool, save: bool) -> Result<Shot> {
    let settings = settings();
    require(SATTY)?;

    let Some(frame) = capture(request, false)? else {
        return Ok(Shot::cancelled(request.mode));
    };

    let output = if save {
        let directory = request
            .directory
            .clone()
            .unwrap_or_else(|| settings.directory.clone());
        std::fs::create_dir_all(&directory)
            .with_context(|| format!("creating {}", directory.display()))?;
        Some(unique(&directory, &filename(&settings.filename, "png")))
    } else {
        None
    };
    let can_copy = copy && which(WL_COPY).is_some();

    let mut child = Command::new(SATTY)
        .args(satty_args(&frame.path, output.as_deref(), can_copy))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("running {SATTY}"))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });

    Ok(Shot {
        cancelled: false,
        mode: request.mode.as_str().to_string(),
        path: Some(frame.path.display().to_string()),
        saved: false,
        copied: false,
        notified: false,
        geometry: frame.geometry,
        output: frame.output,
        window: frame.window,
        width: frame.width,
        height: frame.height,
        bytes: frame.bytes,
        annotating: true,
    })
}

fn satty_args(input: &Path, output: Option<&Path>, copy: bool) -> Vec<String> {
    let mut args = vec![
        "--filename".to_string(),
        input.display().to_string(),
        "--early-exit".to_string(),
        "--initial-tool".to_string(),
        "arrow".to_string(),
    ];
    if let Some(output) = output {
        args.push("--output-filename".to_string());
        args.push(output.display().to_string());
    }
    if copy {
        args.push("--copy-command".to_string());
        args.push(WL_COPY.to_string());
    }
    let mut on_enter = Vec::new();
    if copy {
        on_enter.push("save-to-clipboard");
    }
    if output.is_some() {
        on_enter.push("save-to-file");
    }
    if !on_enter.is_empty() {
        args.push("--actions-on-enter".to_string());
        args.push(on_enter.join(","));
    }
    args
}

pub fn ocr(request: &Request) -> Result<Text> {
    let settings = settings();
    let copy = request.copy.unwrap_or(settings.copy);
    let save = request.save.unwrap_or(false);
    let notify = request.notify.unwrap_or(settings.notify);
    let language = match &request.language {
        Some(language) => language.clone(),
        None => settings.ocr_language.clone(),
    };
    validate_language(&language)?;

    if which(TESSERACT).is_none() {
        bail!("{TESSERACT} is not installed, so there is nothing to read text with");
    }

    let Some(frame) = capture(request, save)? else {
        return Ok(Text::cancelled(request.mode, &language));
    };

    let text = read_text(&frame.path, &language)?;
    let copied = if copy && !text.is_empty() {
        copy_text(&text).context("copying the text to the clipboard")?;
        true
    } else {
        false
    };

    Ok(Text {
        cancelled: false,
        mode: request.mode.as_str().to_string(),
        characters: text.chars().count(),
        lines: if text.is_empty() {
            0
        } else {
            text.lines().count()
        },
        notified: notify && announce_text(&frame, &text, copied).is_ok(),
        copied,
        language,
        geometry: frame.geometry,
        path: if save {
            Some(frame.path.display().to_string())
        } else {
            None
        },
        saved: save,
        text,
    })
}

struct Recording {
    child: std::process::Child,
    path: PathBuf,
    mode: Mode,
    geometry: Option<String>,
    output: Option<String>,
    started: Instant,
    log: Arc<Mutex<String>>,
}

static RECORDING: Mutex<Option<Recording>> = Mutex::new(None);

const STOP_GRACE: Duration = Duration::from_secs(10);

fn recordings() -> Result<std::sync::MutexGuard<'static, Option<Recording>>> {
    RECORDING
        .lock()
        .map_err(|_| anyhow!("the recording lock is poisoned"))
}

pub fn record(request: &Request) -> Result<Session> {
    require(WF_RECORDER)?;
    let settings = settings();

    {
        let slot = recordings()?;
        if let Some(running) = slot.as_ref() {
            bail!(
                "already recording {} for {} -- stop that one first",
                running.mode.as_str(),
                human_duration(running.started.elapsed().as_secs())
            );
        }
    }

    let Some(target) = choose_target(request)? else {
        return Ok(Session {
            cancelled: true,
            mode: request.mode.as_str().to_string(),
            ..Session::default()
        });
    };

    if !request.delay.is_zero() {
        std::thread::sleep(request.delay);
    }

    let directory = request
        .directory
        .clone()
        .unwrap_or_else(|| settings.recording_directory.clone());
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("creating {}", directory.display()))?;
    let destination = unique(&directory, &filename(&settings.recording_filename, "mp4"));

    let mut command = Command::new(WF_RECORDER);
    command.args(["-f", &destination.display().to_string()]);
    // A constant rate is what makes the file playable: left alone, wf-recorder declares 90000fps,
    // x264 picks level 6.2, and players show black.
    if settings.recording_framerate > 0 {
        command.args(["-r", &settings.recording_framerate.to_string()]);
    }
    if let Some(geometry) = &target.geometry {
        command.args(["-g", geometry]);
    }
    if let Some(output) = &target.output {
        command.args(["-o", output]);
    }
    // The recorder outlives the request, so it must not hold the caller's stdout; its output is
    // drained into a buffer instead.
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running {WF_RECORDER}"))?;

    let log = Arc::new(Mutex::new(String::new()));
    if let Some(stderr) = child.stderr.take() {
        let sink = Arc::clone(&log);
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buffer = String::new();
            let mut stderr = stderr;
            let _ = stderr.read_to_string(&mut buffer);
            if let Ok(mut sink) = sink.lock() {
                sink.push_str(&buffer);
            }
        });
    }

    std::thread::sleep(Duration::from_millis(400));
    if let Some(status) = child.try_wait()? {
        let detail = log.lock().ok().map(|log| log.trim().to_string());
        let detail = detail.filter(|detail| !detail.is_empty());
        let _ = std::fs::remove_file(&destination);
        match detail {
            Some(detail) => bail!("{WF_RECORDER} stopped immediately: {}", last_line(&detail)),
            None => bail!("{WF_RECORDER} stopped immediately ({status})"),
        }
    }

    let session = Session {
        recording: true,
        cancelled: false,
        mode: request.mode.as_str().to_string(),
        path: Some(destination.display().to_string()),
        geometry: target.geometry.clone(),
        output: target.output.clone(),
        seconds: 0,
        bytes: 0,
        notified: false,
    };
    *recordings()? = Some(Recording {
        child,
        path: destination,
        mode: request.mode,
        geometry: target.geometry,
        output: target.output,
        started: Instant::now(),
        log,
    });
    Ok(session)
}

pub fn stop_recording(notify: Option<bool>) -> Result<Session> {
    let notify = notify.unwrap_or_else(|| settings().recording_notify);
    let Some(mut running) = recordings()?.take() else {
        return Ok(Session::default());
    };

    let seconds = running.started.elapsed().as_secs();
    // SIGINT tells wf-recorder to finalize the file rather than abandon it.
    let interrupted = signal(&running.child, "INT");
    let finished = wait_for(&mut running.child, STOP_GRACE);
    if !finished {
        signal(&running.child, "KILL");
        let _ = running.child.wait();
    }

    let bytes = std::fs::metadata(&running.path)
        .map(|meta| meta.len())
        .unwrap_or(0);
    let path = running.path.display().to_string();
    let failed = bytes == 0;
    if failed {
        let _ = std::fs::remove_file(&running.path);
    }

    let notified = notify && {
        let summary = if failed {
            "Recording failed"
        } else {
            "Recording saved"
        };
        let body = if failed {
            let log = running
                .log
                .lock()
                .ok()
                .map(|log| last_line(log.trim()))
                .unwrap_or_default();
            if log.is_empty() {
                if interrupted {
                    "Nothing was written".to_string()
                } else {
                    "The recorder could not be stopped cleanly".to_string()
                }
            } else {
                log
            }
        } else {
            format!(
                "{} · {} · {}",
                file_name(&running.path),
                human_duration(seconds),
                human_bytes(bytes)
            )
        };
        announce(summary, &body, None).is_ok()
    };

    Ok(Session {
        recording: false,
        cancelled: false,
        mode: running.mode.as_str().to_string(),
        path: if failed { None } else { Some(path) },
        geometry: running.geometry,
        output: running.output,
        seconds,
        bytes,
        notified,
    })
}

pub fn recording() -> Result<Session> {
    let mut slot = recordings()?;
    let Some(running) = slot.as_mut() else {
        return Ok(Session::default());
    };
    if running.child.try_wait()?.is_some() {
        let running = slot.take().expect("checked just above");
        let bytes = std::fs::metadata(&running.path)
            .map(|meta| meta.len())
            .unwrap_or(0);
        return Ok(Session {
            recording: false,
            mode: running.mode.as_str().to_string(),
            path: Some(running.path.display().to_string()),
            seconds: running.started.elapsed().as_secs(),
            bytes,
            ..Session::default()
        });
    }
    Ok(Session {
        recording: true,
        cancelled: false,
        mode: running.mode.as_str().to_string(),
        path: Some(running.path.display().to_string()),
        geometry: running.geometry.clone(),
        output: running.output.clone(),
        seconds: running.started.elapsed().as_secs(),
        bytes: 0,
        notified: false,
    })
}

fn signal(child: &std::process::Child, name: &str) -> bool {
    Command::new("kill")
        .arg(format!("-{name}"))
        .arg(child.id().to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn wait_for(child: &mut std::process::Child, grace: Duration) -> bool {
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return false,
        }
    }
    false
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default()
}

fn last_line(text: &str) -> String {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_string()
}

fn human_duration(seconds: u64) -> String {
    let (hours, minutes, seconds) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn human_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    match bytes {
        0..KB => format!("{bytes} B"),
        KB..MB => format!("{:.0} KB", bytes as f64 / KB as f64),
        MB..GB => format!("{:.1} MB", bytes as f64 / MB as f64),
        _ => format!("{:.2} GB", bytes as f64 / GB as f64),
    }
}

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
    match image_format(destination) {
        Some(format) => {
            command.args(["-t", format]);
        }
        None => bail!(
            "{GRIM} cannot write {} -- screenshot_filename should end in .png, .jpg, or .ppm",
            destination.display()
        ),
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

fn select_region() -> Result<Option<String>> {
    require(SLURP)?;
    slurp(&[], None)
}

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
                // surface.
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
        .or_else(|| parse_geometry(&chosen).ok()))
}

/// slurp exits non-zero on Escape; a cancel is told apart from an error by empty stdout.
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

fn copy_image(path: &Path) -> Result<()> {
    require(WL_COPY)?;
    let data = std::fs::read(path)?;
    let mime = match image_format(path) {
        Some("jpeg") => "image/jpeg",
        Some("ppm") => "image/x-portable-pixmap",
        _ => "image/png",
    };
    // wl-copy forks a process that holds the selection and inherits our stdout; left inherited it
    // holds the pipe open (`epochctl capture ocr | wc -l` would hang).
    let mut child = Command::new(WL_COPY)
        .args(["--type", mime])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("running {WL_COPY}"))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| anyhow!("clipboard stdin unavailable"))?
        .write_all(&data)?;
    drop(child.stdin.take());
    let status = child.wait()?;
    if !status.success() {
        bail!("{WL_COPY} exited with {status}");
    }
    Ok(())
}

/// tesseract warns on stderr and still exits 0, so only a non-zero exit is a failure.
fn read_text(path: &Path, language: &str) -> Result<String> {
    let out = Command::new(TESSERACT)
        .arg(path)
        .arg("stdout")
        .args(["-l", language])
        .output()
        .with_context(|| format!("running {TESSERACT}"))?;
    if !out.status.success() {
        let detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let detail = if detail.is_empty() {
            format!("exited with {}", out.status)
        } else {
            detail
        };
        bail!("{TESSERACT} failed: {detail}");
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn validate_language(language: &str) -> Result<()> {
    let valid = !language.is_empty()
        && language
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '_' || c == '-');
    if valid {
        Ok(())
    } else {
        bail!("\"{language}\" is not a tesseract language (try eng, or eng+deu)")
    }
}

fn copy_text(text: &str) -> Result<()> {
    require(WL_COPY)?;
    // See copy_image: the selection owner must not keep this process's stdout open.
    let mut child = Command::new(WL_COPY)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("running {WL_COPY}"))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| anyhow!("clipboard stdin unavailable"))?
        .write_all(text.as_bytes())?;
    drop(child.stdin.take());
    let status = child.wait()?;
    if !status.success() {
        bail!("{WL_COPY} exited with {status}");
    }
    Ok(())
}

fn announce_shot(frame: &Frame, saved: bool, copied: bool) -> Result<()> {
    let summary = match (saved, copied) {
        (true, true) => "Screenshot saved and copied",
        (true, false) => "Screenshot saved",
        (false, true) => "Screenshot copied",
        (false, false) => "Screenshot taken",
    };
    let mut body = if saved {
        frame
            .path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default()
    } else {
        String::new()
    };
    if frame.width > 0 && frame.height > 0 {
        if !body.is_empty() {
            body.push_str(" · ");
        }
        body.push_str(&format!("{}×{}", frame.width, frame.height));
    }
    announce(summary, &body, Some(&frame.path))
}

fn announce_text(frame: &Frame, text: &str, copied: bool) -> Result<()> {
    if text.is_empty() {
        return announce(
            "No text found",
            "Nothing legible in that region",
            Some(&frame.path),
        );
    }
    let summary = if copied {
        "Text copied"
    } else {
        "Text captured"
    };
    announce(summary, &preview(text, 3, 160), Some(&frame.path))
}

fn preview(text: &str, lines: usize, characters: usize) -> String {
    let mut out = String::new();
    let mut taken = 0;
    for line in text.lines().take(lines) {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
        taken += 1;
    }
    let more = text.lines().count() > taken;
    if out.chars().count() > characters {
        out = out.chars().take(characters).collect();
        return format!("{}…", out.trim_end());
    }
    if more {
        out.push('…');
    }
    out
}

fn announce(summary: &str, body: &str, image: Option<&Path>) -> Result<()> {
    crate::notify::send(summary, body, "camera-photo", image, "epoch-screenshot")
}

fn scratch_dir() -> Result<PathBuf> {
    let directory = dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("epochoxide/screenshots");
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("creating {}", directory.display()))?;
    Ok(directory)
}

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

fn filename(template: &str, fallback: &str) -> String {
    let expanded = Command::new("date")
        .arg(format!("+{template}"))
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default();
    let name = if expanded.is_empty() || expanded.contains('/') {
        format!("screenshot-{}", epoch_seconds())
    } else {
        expanded
    };
    if Path::new(&name).extension().is_some() {
        name
    } else {
        format!("{name}.{fallback}")
    }
}

fn epoch_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

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

fn image_format(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_string_lossy().to_lowercase().as_str() {
        "png" => Some("png"),
        "jpg" | "jpeg" => Some("jpeg"),
        "ppm" => Some("ppm"),
        _ => None,
    }
}

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
        assert_eq!(filename("shot.png", "png"), "shot.png");
        assert_eq!(filename("shot.jpg", "png"), "shot.jpg");
        assert_eq!(filename("shot", "png"), "shot.png");
        assert_eq!(filename("clip.mkv", "mp4"), "clip.mkv");
        assert_eq!(filename("clip", "mp4"), "clip.mp4");
    }

    #[test]
    fn a_template_that_would_escape_the_directory_falls_back() {
        let name = filename("../../%Y/shot.png", "png");
        assert!(!name.contains('/'), "{name} still has a path separator");
        assert!(name.ends_with(".png"));
    }

    #[test]
    fn a_date_template_is_expanded() {
        let name = filename("screenshot-%Y.png", "png");
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
    fn a_language_is_a_tesseract_language_or_a_typo() {
        assert!(validate_language("eng").is_ok());
        assert!(validate_language("eng+deu").is_ok());
        assert!(validate_language("chi_sim").is_ok());
        assert!(validate_language("").is_err());
        assert!(validate_language("eng deu").is_err());
        assert!(validate_language("../eng").is_err());
    }

    #[test]
    fn a_notification_body_shows_the_first_lines_and_says_there_are_more() {
        assert_eq!(preview("one\ntwo", 3, 160), "one\ntwo");
        assert_eq!(preview("one\ntwo\nthree\nfour", 3, 160), "one\ntwo\nthree…");
        assert_eq!(preview("", 3, 160), "");
    }

    #[test]
    fn a_long_first_line_is_cut_rather_than_filling_the_toast() {
        let long = "x".repeat(400);
        let shown = preview(&long, 3, 20);
        assert_eq!(shown.chars().count(), 21, "20 characters and an ellipsis");
        assert!(shown.ends_with('…'));
    }

    #[test]
    fn durations_read_the_way_people_say_them() {
        assert_eq!(human_duration(0), "0:00");
        assert_eq!(human_duration(42), "0:42");
        assert_eq!(human_duration(187), "3:07");
        assert_eq!(human_duration(3733), "1:02:13");
    }

    #[test]
    fn sizes_read_the_way_people_say_them() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(788_390), "770 KB");
        assert_eq!(human_bytes(41_943_040), "40.0 MB");
        assert_eq!(human_bytes(3_221_225_472), "3.00 GB");
    }

    #[test]
    fn a_failure_is_reported_from_the_end_of_the_log() {
        let log = "selected region\nSetting codec option: crf=20\n\nfailed to open output\n";
        assert_eq!(last_line(log), "failed to open output");
        assert_eq!(last_line(""), "");
    }

    #[test]
    fn nothing_is_recording_until_something_starts() {
        let idle = Session::default();
        assert!(!idle.recording);
        assert!(!idle.cancelled);
        assert_eq!(idle.path, None);
        assert_eq!(idle.seconds, 0);
    }

    #[test]
    fn the_image_format_follows_the_extension() {
        assert_eq!(image_format(Path::new("a.png")), Some("png"));
        assert_eq!(image_format(Path::new("a.JPEG")), Some("jpeg"));
        assert_eq!(image_format(Path::new("a.txt")), None);
    }

    #[test]
    fn satty_is_told_where_the_shot_goes() {
        let args = satty_args(
            Path::new("/tmp/in.png"),
            Some(Path::new("/s/out.png")),
            true,
        );
        let joined = args.join(" ");
        assert!(joined.contains("--filename /tmp/in.png"));
        assert!(joined.contains("--output-filename /s/out.png"));
        assert!(joined.contains("--copy-command wl-copy"));
        assert!(joined.contains("--actions-on-enter save-to-clipboard,save-to-file"));
        assert!(joined.contains("--early-exit"));
    }

    #[test]
    fn satty_saves_nothing_when_saving_is_off() {
        let args = satty_args(Path::new("/tmp/in.png"), None, true);
        assert!(!args.iter().any(|arg| arg == "--output-filename"));
        assert!(args
            .join(" ")
            .contains("--actions-on-enter save-to-clipboard"));
        assert!(!args.join(" ").contains("save-to-file"));
    }
}
