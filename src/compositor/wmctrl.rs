//! X11 via wmctrl. The last resort: windows only, with no workspace, monitor, or focus data.

use super::ipc::{command_output, run};
use super::{Compositor, Window};
use anyhow::Result;

pub struct Wmctrl;

impl Compositor for Wmctrl {
    fn name(&self) -> &'static str {
        "wmctrl"
    }

    fn responds(&self) -> bool {
        command_output("wmctrl", &["-m"]).is_some()
    }

    fn windows(&self) -> Option<Vec<Window>> {
        Some(
            command_output("wmctrl", &["-lx"])?
                .lines()
                .filter_map(parse)
                .collect(),
        )
    }

    fn focus_window(&self, handle: &str) -> Result<()> {
        run(&format!("wmctrl -ia {handle}"))
    }

    fn close_window(&self, handle: &str) -> Result<()> {
        run(&format!("wmctrl -ic {handle}"))
    }
}

/// `wmctrl -lx` is columnar: id, desktop, class, host, then the title, which may contain spaces.
fn parse(line: &str) -> Option<Window> {
    let mut parts = line.split_whitespace();
    let id = parts.next()?.to_string();
    let _desktop = parts.next()?;
    let app = parts.next().unwrap_or_default().to_string();
    let _host = parts.next();
    let title = parts.collect::<Vec<_>>().join(" ");
    Some(Window {
        id: format!("wmctrl:{id}"),
        app_id: app,
        title,
        workspace: String::new(),
        monitor: String::new(),
        focused: false,
        floating: false,
        // `wmctrl -lx` carries no geometry, so ordering falls back to the listing order.
        x: 0,
        y: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_title_containing_spaces_is_kept_whole() {
        let window =
            parse("0x03000007  0 Navigator.firefox  thor  Mozilla Firefox — Sharp").unwrap();
        assert_eq!(window.id, "wmctrl:0x03000007");
        assert_eq!(window.app_id, "Navigator.firefox");
        assert_eq!(window.title, "Mozilla Firefox — Sharp");
    }

    #[test]
    fn a_truncated_line_is_skipped_rather_than_panicking() {
        assert!(parse("0x03000007").is_none());
        assert!(parse("").is_none());
    }
}
