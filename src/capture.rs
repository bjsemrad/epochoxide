//! Screen capture.
//!
//! Screenshots are taken with `grim`, regions are chosen with `slurp`, text is read out of them
//! with `tesseract`, the result is put on the clipboard with `wl-copy`, and the user is told about
//! it with `notify-send` -- which the shell itself answers, since EpochShell is the session's
//! notification server.
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
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

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
    /// Tesseract language for `ocr`, overriding `ocr_language`.
    pub language: Option<String>,
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

/// What an OCR pass read. Empty text is a result, not a failure: a region with nothing legible in
/// it is a thing that happens, and it is worth saying so rather than raising an error.
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
    /// The image the text was read from, kept only when the request asked to save it.
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

/// A screen recording, running or finished.
///
/// The same shape answers "start", "stop", and "what is happening", so a caller polling for the
/// bar indicator and a caller that just pressed stop read the same fields.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Session {
    pub recording: bool,
    /// True when the user cancelled the selection instead of starting anything.
    pub cancelled: bool,
    pub mode: String,
    pub path: Option<String>,
    pub geometry: Option<String>,
    pub output: Option<String>,
    /// How long it has been running, or ran for.
    pub seconds: u64,
    /// Size of the finished file. Zero while it is still being written.
    pub bytes: u64,
    pub notified: bool,
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
    /// False when tesseract is not installed, which is what `capture.ocr` needs.
    pub ocr: bool,
    pub ocr_language: String,
    /// False when wf-recorder is not installed, which is what `capture.record` needs.
    pub record: bool,
    pub recording_directory: String,
    pub compositor: Option<String>,
}

const GRIM: &str = "grim";
const SLURP: &str = "slurp";
const WL_COPY: &str = "wl-copy";
const NOTIFY_SEND: &str = "notify-send";
const TESSERACT: &str = "tesseract";
const WF_RECORDER: &str = "wf-recorder";

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
            tool(TESSERACT, "reading text out of a capture", false),
            tool(WF_RECORDER, "screen recording", false),
        ],
        window_capture: regions.is_some(),
        ocr: which(TESSERACT).is_some(),
        ocr_language: settings.ocr_language,
        record: which(WF_RECORDER).is_some(),
        recording_directory: settings.recording_directory.display().to_string(),
        compositor: compositor::active().map(|backend| backend.name().to_string()),
    }
}

/// A captured frame, sitting in a file, before anything has been decided about what to do with it.
struct Frame {
    path: PathBuf,
    geometry: Option<String>,
    output: Option<String>,
    window: Option<String>,
    width: u32,
    height: u32,
    bytes: u64,
}

/// What the user pointed at: a rectangle, a whole output, or the entire layout.
#[derive(Debug, Clone, Default)]
struct Target {
    geometry: Option<String>,
    output: Option<String>,
    window: Option<String>,
}

/// Work out what to point the camera at, asking the user when the mode calls for it.
///
/// `Ok(None)` is a cancelled selection rather than a failure. This is shared by every capture --
/// a still, an OCR pass, a recording -- so "what does region mean" is answered in one place.
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
            // Falling back to the whole layout is better than refusing: a single-monitor session
            // whose compositor does not report a focused output still gets its capture.
            target.output = request.output.clone().or_else(compositor::focused_monitor);
        }
        Mode::All => {}
    }
    Ok(Some(target))
}

/// Ask what to capture, capture it, and leave it in a file.
///
/// `Ok(None)` is a cancelled selection rather than a failure. `save` decides where the file lands:
/// a kept capture goes to the screenshot directory under its configured name, and everything else
/// to the pruned scratch directory, because even a capture nobody wants to keep has to exist as a
/// file for the clipboard, for tesseract, and for the notification's thumbnail.
fn capture(request: &Request, save: bool) -> Result<Option<Frame>> {
    let settings = settings();
    require(GRIM)?;

    // Selection happens before the delay so the delay is a chance to arrange what is on screen,
    // not dead time before the user is even asked what to capture.
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

/// Take a screenshot, then copy it, save it, and announce it as the request asks.
pub fn screenshot(request: &Request) -> Result<Shot> {
    let settings = settings();
    let copy = request.copy.unwrap_or(settings.copy);
    let save = request.save.unwrap_or(settings.save);
    let notify = request.notify.unwrap_or(settings.notify);

    let Some(frame) = capture(request, save)? else {
        return Ok(Shot::cancelled(request.mode));
    };

    // A copy that fails is worth reporting rather than swallowing: the user asked for the shot to
    // be on the clipboard, and a silent "it worked" would send them pasting into nothing.
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
        // A missing notify-send is not a failed screenshot, so the notification is best-effort and
        // the result says whether it actually went out.
        notified: notify && announce_shot(&frame, save, copied).is_ok(),
        geometry: frame.geometry,
        output: frame.output,
        window: frame.window,
        width: frame.width,
        height: frame.height,
        bytes: frame.bytes,
    };
    Ok(shot)
}

/// Capture a region and read the text out of it.
///
/// The image is a means to an end here, so it is not kept unless the request asks: what the user
/// wanted is on the clipboard, and a screenshots folder filling up with pictures of text nobody
/// will look at again is not a feature.
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

// --- Recording ---------------------------------------------------------------

/// The recording in flight, if there is one.
///
/// A recording is the one stateful thing in this module: it outlives the request that started it,
/// so the daemon holds the process rather than the connection. Only one runs at a time -- two
/// recorders would fight over the same file name and produce two files nobody asked for.
struct Recording {
    child: std::process::Child,
    path: PathBuf,
    mode: Mode,
    geometry: Option<String>,
    output: Option<String>,
    started: Instant,
    /// Whatever the recorder said, drained by a thread so a full pipe can never stall it.
    log: Arc<Mutex<String>>,
}

static RECORDING: Mutex<Option<Recording>> = Mutex::new(None);

/// How long to let the recorder finish writing the file after being asked to stop, before it is
/// killed outright. Finalizing an MP4 is quick; this is the bound on a recorder that has wedged.
const STOP_GRACE: Duration = Duration::from_secs(10);

fn recordings() -> Result<std::sync::MutexGuard<'static, Option<Recording>>> {
    RECORDING
        .lock()
        .map_err(|_| anyhow!("the recording lock is poisoned"))
}

/// Start recording. Fails rather than silently replacing a recording already in progress.
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
    // Recording at a constant rate is not a quality setting, it is what makes the file playable.
    // Left to time itself, wf-recorder writes a stream declaring 90000fps, from which x264 derives
    // level 6.2 -- above what players will decode, so the video opens and shows black while the
    // frames inside it are perfectly good.
    if settings.recording_framerate > 0 {
        command.args(["-r", &settings.recording_framerate.to_string()]);
    }
    if let Some(geometry) = &target.geometry {
        command.args(["-g", geometry]);
    }
    if let Some(output) = &target.output {
        command.args(["-o", output]);
    }
    // The recorder outlives this request, so it must not hold the caller's stdout; its own
    // reporting is drained into a buffer instead, where a failure can still be quoted back.
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

    // A recorder that cannot start -- no output by that name, no permission to write -- dies
    // immediately, and reporting that now is far better than a bar indicator counting up against
    // a process that is already gone.
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

/// Stop the recording and wait for the file to be finished.
///
/// Stopping when nothing is recording is not an error: a keybinding bound to "stop" pressed twice
/// should say so quietly rather than fail.
pub fn stop_recording(notify: Option<bool>) -> Result<Session> {
    let notify = notify.unwrap_or_else(|| settings().recording_notify);
    let Some(mut running) = recordings()?.take() else {
        return Ok(Session::default());
    };

    let seconds = running.started.elapsed().as_secs();
    // SIGINT is what tells wf-recorder to finalize the file rather than abandon it. Sent through
    // kill(1) because nothing else here needs libc, and a signal is not worth a dependency.
    let interrupted = signal(&running.child, "INT");
    let finished = wait_for(&mut running.child, STOP_GRACE);
    if !finished {
        // A recorder that will not stop leaves a file that may still be usable, so it is killed
        // rather than left holding the screen capture open forever.
        signal(&running.child, "KILL");
        let _ = running.child.wait();
    }

    let bytes = std::fs::metadata(&running.path)
        .map(|meta| meta.len())
        .unwrap_or(0);
    let path = running.path.display().to_string();
    // An empty file means the recorder never wrote anything worth keeping; saying "saved" over
    // that would be a lie, and the file itself is not worth leaving behind.
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

/// What is being recorded right now, if anything.
///
/// A recorder that has died on its own -- the disk filled, the output was unplugged -- is noticed
/// here and cleared, so a bar indicator polling this stops counting up against a dead process.
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

/// Send a signal by name. Returns whether the signal was delivered.
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

/// Wait for a child for at most `grace`. Returns whether it actually exited.
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

/// The last non-empty line of a tool's output: recorders log a lot, and the failure is at the end.
fn last_line(text: &str) -> String {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_string()
}

/// A duration as a person reads it back: `0:42`, `3:07`, `1:02:13`.
fn human_duration(seconds: u64) -> String {
    let (hours, minutes, seconds) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// A file size in the units a person reads, not bytes.
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
    // A name grim cannot write is a mistake in screenshot_filename, and writing PNG bytes into it
    // under another extension would hide that rather than fix it.
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
    // wl-copy forks a background process to hold the selection until something replaces it, and
    // that process inherits whatever stdout it was given. Left inherited, it holds a pipe open
    // long after this command has finished -- `epochctl capture ocr | wc -l` would sit there
    // until the next copy. Handing it nothing costs nothing: it has nothing to say.
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
    // wl-copy forks a background process to hold the selection and exits, so waiting here costs
    // nothing and turns "the clipboard tool is broken" into a reported failure.
    let status = child.wait()?;
    if !status.success() {
        bail!("{WL_COPY} exited with {status}");
    }
    Ok(())
}

/// Read the text out of an image.
///
/// tesseract writes its own progress and warnings to stderr and still exits 0, so only a non-zero
/// exit is a failure; an empty answer means the region had nothing legible in it.
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

/// tesseract takes `eng`, or several joined with `+`. Anything else is a typo worth naming now
/// rather than a confusing tesseract error after the user has already chosen a region.
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

/// Tell the session a shot was taken. The image path goes along as a hint so the shell can show
/// the shot itself rather than a generic camera icon.
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

/// Tell the session what was read. The text itself is the body, trimmed to a few lines: the point
/// is to confirm the right thing was captured, and the whole of it is on the clipboard anyway.
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

/// The first few lines of `text`, ellipsised, for a notification body.
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

/// Send one notification, best-effort. `image` becomes the thumbnail the shell draws.
fn announce(summary: &str, body: &str, image: Option<&Path>) -> Result<()> {
    require(NOTIFY_SEND)?;
    let mut command = Command::new(NOTIFY_SEND);
    command
        .arg("--app-name=EpochShell")
        .arg("--icon=camera-photo");
    if let Some(image) = image {
        command.arg(format!("--hint=string:image-path:{}", image.display()));
    }
    // Successive captures replace each other in the toast stack instead of stacking up.
    let status = command
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
///
/// `fallback` is the extension to use when the template names no file type at all. It is passed in
/// rather than assumed, because what a capture should be called depends on what it is: a still is
/// a `.png`, a recording is an `.mp4`, and a helper that knows only about images turns
/// `recording-%H%M%S.mp4` into `recording-102636.mp4.png` -- which ffmpeg then tries to write as a
/// single image and gives up on.
fn filename(template: &str, fallback: &str) -> String {
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
        assert_eq!(filename("shot.png", "png"), "shot.png");
        assert_eq!(filename("shot.jpg", "png"), "shot.jpg");
        // No extension, so one is added rather than leaving grim to guess.
        assert_eq!(filename("shot", "png"), "shot.png");
        // A recording keeps the container it names, and gets mp4 when it names none.
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
        // Recorders log a wall of setup before the line that matters.
        let log = "selected region\nSetting codec option: crf=20\n\nfailed to open output\n";
        assert_eq!(last_line(log), "failed to open output");
        assert_eq!(last_line(""), "");
    }

    #[test]
    fn nothing_is_recording_until_something_starts() {
        // The empty session is what a caller polling for the bar indicator sees, so it must be
        // honest about having no file and no elapsed time rather than defaulting to true.
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
}
