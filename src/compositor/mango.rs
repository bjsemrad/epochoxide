//! mango, over its IPC socket (the protocol `mmsg` speaks).
//!
//! mango is dwl-derived, so it has tags rather than workspaces: every monitor carries its own
//! numbered set, and a window can sit on several at once. Each (monitor, tag) pair is reported as
//! one workspace named by its number, the way niri reports per-output workspaces by index.

use super::ipc::{mango_request, mango_watch};
use super::{Compositor, Monitor, Window, WindowRegion, Workspace};
use anyhow::Result;
use serde_json::Value;

pub struct Mango;

impl Compositor for Mango {
    fn name(&self) -> &'static str {
        "mango"
    }

    fn responds(&self) -> bool {
        json("get version").is_some()
    }

    fn windows(&self) -> Option<Vec<Window>> {
        let clients = clients()?;
        let active = active_tags();
        Some(
            clients
                .iter()
                .map(|client| {
                    let monitor = string(client, "monitor").unwrap_or_default();
                    Window {
                        id: format!("mango:{}", int(client, "id")),
                        app_id: string(client, "appid").unwrap_or_default(),
                        title: string(client, "title").unwrap_or_default(),
                        workspace: client_tag(client, &monitor, &active)
                            .map(|tag| tag.to_string())
                            .unwrap_or_default(),
                        monitor,
                        focused: flag(client, "is_focused"),
                        floating: flag(client, "is_floating"),
                        x: int(client, "x"),
                        y: int(client, "y"),
                    }
                })
                .collect(),
        )
    }

    fn workspaces(&self) -> Option<Vec<Workspace>> {
        let monitors = monitors_json()?;
        Some(
            monitors
                .iter()
                .flat_map(|monitor| {
                    let name = string(monitor, "name").unwrap_or_default();
                    monitor
                        .get("tags")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .map(move |tag| {
                            let index = int(tag, "index");
                            Workspace {
                                id: format!("mango:{name}:{index}"),
                                name: index.to_string(),
                                monitor: name.clone(),
                                active: flag(tag, "is_active"),
                                urgent: flag(tag, "is_urgent"),
                                windows: int(tag, "client_count").max(0) as u32,
                            }
                        })
                })
                .collect(),
        )
    }

    fn monitors(&self) -> Option<Vec<Monitor>> {
        Some(
            monitors_json()?
                .iter()
                .map(|monitor| {
                    let name = string(monitor, "name").unwrap_or_default();
                    Monitor {
                        id: format!("mango:{name}"),
                        name,
                        width: int(monitor, "width"),
                        height: int(monitor, "height"),
                        scale: monitor.get("scale").and_then(Value::as_f64).unwrap_or(1.0),
                        focused: flag(monitor, "active"),
                    }
                })
                .collect(),
        )
    }

    /// mango reports client geometry in the global output layout, and says outright which
    /// clients are on a tag someone is looking at.
    fn window_regions(&self) -> Option<Vec<WindowRegion>> {
        Some(
            clients()?
                .iter()
                .filter(|client| flag(client, "is_visible") && !flag(client, "is_minimized"))
                .map(|client| WindowRegion {
                    id: format!("mango:{}", int(client, "id")),
                    app_id: string(client, "appid").unwrap_or_default(),
                    title: string(client, "title").unwrap_or_default(),
                    monitor: string(client, "monitor").unwrap_or_default(),
                    focused: flag(client, "is_focused"),
                    x: int(client, "x"),
                    y: int(client, "y"),
                    width: int(client, "width"),
                    height: int(client, "height"),
                })
                .filter(|region| region.width > 0 && region.height > 0)
                .collect(),
        )
    }

    /// `focusid` switches to the client's tag and monitor as well, so a window on a hidden tag
    /// can be focused from the bar.
    fn focus_window(&self, handle: &str) -> Result<()> {
        dispatch(&format!("focusid client,{}", client_id(handle)?))
    }

    fn close_window(&self, handle: &str) -> Result<()> {
        dispatch(&format!("killclient client,{}", client_id(handle)?))
    }

    /// `monitor:tag` from a workspace id, or a bare tag number from a keybinding, which acts on
    /// the focused monitor the way mango's own `view` binding does.
    fn focus_workspace(&self, handle: &str) -> Result<()> {
        dispatch(&workspace_dispatch(handle)?)
    }

    fn watch(&self, on_event: &mut dyn FnMut() -> Result<()>) -> Result<()> {
        mango_watch(on_event)
    }
}

fn workspace_dispatch(handle: &str) -> Result<String> {
    let (monitor, tag) = match handle.rsplit_once(':') {
        Some((monitor, tag)) => (Some(monitor), tag),
        None => (None, handle),
    };
    let tag: u32 = tag
        .parse()
        .ok()
        .filter(|tag| *tag > 0)
        .ok_or_else(|| anyhow::anyhow!("mango tags are numbered from 1, not \"{tag}\""))?;
    Ok(match monitor {
        // The monitor is matched as a regex, so anchor it: "DP-1" would otherwise also match
        // "eDP-1".
        Some(monitor) => format!("viewcrossmon,{tag},^{}$", regex_escape(monitor)),
        None => format!("view,{tag},0"),
    })
}

fn regex_escape(value: &str) -> String {
    value
        .chars()
        .flat_map(|c| {
            let special = "\\.+*?()|[]{}^$".contains(c);
            special.then_some('\\').into_iter().chain([c])
        })
        .collect()
}

fn client_id(handle: &str) -> Result<u32> {
    handle
        .parse()
        .map_err(|_| anyhow::anyhow!("\"{handle}\" is not a mango client id"))
}

/// Run a dispatch. mango answers `{"success":true}` when it found the function (and client), and
/// an `error` otherwise.
fn dispatch(command: &str) -> Result<()> {
    let reply = mango_request(&format!("dispatch {command}"))
        .ok_or_else(|| anyhow::anyhow!("mango is not answering"))?;
    let value: Value = serde_json::from_str(&reply).unwrap_or(Value::Null);
    if flag(&value, "success") {
        Ok(())
    } else {
        let error = string(&value, "error").unwrap_or(reply);
        anyhow::bail!("mango rejected \"{command}\": {error}")
    }
}

/// The tag a window is shown under. A window on several tags is listed under the one being
/// viewed on its monitor if there is one, and otherwise its lowest, so it appears once.
fn client_tag(client: &Value, monitor: &str, active: &[(String, Vec<i64>)]) -> Option<i64> {
    let tags: Vec<i64> = client
        .get("tags")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(Value::as_i64)
        .collect();
    let viewed = active
        .iter()
        .find(|(name, _)| name == monitor)
        .map(|(_, tags)| tags.as_slice())
        .unwrap_or_default();
    tags.iter()
        .find(|tag| viewed.contains(tag))
        .or_else(|| tags.iter().min())
        .copied()
}

fn active_tags() -> Vec<(String, Vec<i64>)> {
    monitors_json()
        .unwrap_or_default()
        .iter()
        .map(|monitor| {
            let tags = monitor
                .get("active_tags")
                .and_then(Value::as_array)
                .map(|tags| tags.iter().filter_map(Value::as_i64).collect())
                .unwrap_or_default();
            (string(monitor, "name").unwrap_or_default(), tags)
        })
        .collect()
}

fn clients() -> Option<Vec<Value>> {
    list("get all-clients", "clients")
}

fn monitors_json() -> Option<Vec<Value>> {
    list("get all-monitors", "monitors")
}

fn list(command: &str, key: &str) -> Option<Vec<Value>> {
    json(command)?.get(key)?.as_array().cloned()
}

fn json(command: &str) -> Option<Value> {
    serde_json::from_str(&mango_request(command)?).ok()
}

fn string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

fn int(value: &Value, key: &str) -> i64 {
    value.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn flag(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn viewing(monitor: &str, tags: &[i64]) -> Vec<(String, Vec<i64>)> {
        vec![(monitor.to_string(), tags.to_vec())]
    }

    #[test]
    fn a_window_is_listed_under_its_tag() {
        let client = json!({ "tags": [4] });
        assert_eq!(
            client_tag(&client, "eDP-1", &viewing("eDP-1", &[1])),
            Some(4)
        );
    }

    #[test]
    fn a_window_on_several_tags_is_listed_under_the_one_being_viewed() {
        let client = json!({ "tags": [2, 5] });
        assert_eq!(
            client_tag(&client, "eDP-1", &viewing("eDP-1", &[5])),
            Some(5)
        );
        assert_eq!(
            client_tag(&client, "eDP-1", &viewing("eDP-1", &[1])),
            Some(2)
        );
    }

    #[test]
    fn the_view_on_another_monitor_does_not_count() {
        let client = json!({ "tags": [2, 5] });
        assert_eq!(
            client_tag(&client, "eDP-1", &viewing("DP-2", &[5])),
            Some(2)
        );
    }

    #[test]
    fn a_workspace_id_switches_on_its_own_monitor() {
        assert_eq!(
            workspace_dispatch("eDP-1:3").unwrap(),
            "viewcrossmon,3,^eDP-1$"
        );
    }

    #[test]
    fn a_bare_tag_switches_on_the_focused_monitor() {
        assert_eq!(workspace_dispatch("3").unwrap(), "view,3,0");
    }

    #[test]
    fn a_tag_that_is_not_a_positive_number_is_refused() {
        assert!(workspace_dispatch("web").is_err());
        assert!(workspace_dispatch("eDP-1:0").is_err());
    }

    #[test]
    fn regex_characters_in_a_monitor_name_are_escaped() {
        assert_eq!(regex_escape("HDMI-A.1"), "HDMI-A\\.1");
    }
}
