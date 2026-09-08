use super::Provider;
use crate::compositor;
use crate::{
    fuzzy,
    types::{action_map, ActionCapability, Item, ProviderCapability},
};
use anyhow::Result;
use std::collections::HashMap;

pub struct WindowsProvider {
    wm_class_icons: HashMap<String, String>,
}

impl WindowsProvider {
    pub fn new(wm_class_icons: HashMap<String, String>) -> Self {
        Self { wm_class_icons }
    }

    fn icon_for(&self, app: &str) -> String {
        icon_candidates(app)
            .into_iter()
            .find_map(|key| self.wm_class_icons.get(&key).cloned())
            .unwrap_or_else(|| "preferences-system-windows".into())
    }
}

const BROWSER_PREFIXES: [&str; 6] = [
    "google-chrome-",
    "microsoft-edge-",
    "chromium-",
    "chrome-",
    "brave-",
    "vivaldi-",
];

/// Desktop-entry keys to try for a window class, best first.
///
/// An exact match covers well-behaved apps. Chromium-family web apps do not report the
/// `StartupWMClass` their .desktop file declares — they report `brave-gmail.com__-Default` or
/// `brave-mail.proton.me__u_0_inbox-Default` — so the host is peeled out of the class and tried
/// as a name, then by domain, and finally the browser itself so a web app at least gets the
/// browser's icon instead of a generic window.
fn icon_candidates(app: &str) -> Vec<String> {
    let key = app.to_lowercase();
    let mut out = vec![key.clone()];

    let base = key
        .split("__")
        .next()
        .unwrap_or(&key)
        .trim_end_matches("-default")
        .to_string();
    out.push(base.clone());

    let browser = BROWSER_PREFIXES
        .iter()
        .find(|prefix| base.starts_with(**prefix));
    let host = browser
        .and_then(|prefix| base.strip_prefix(*prefix))
        .unwrap_or(&base)
        .to_string();
    out.push(host.clone());

    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() > 1 {
        out.push(labels[0].to_string());
        for start in 1..labels.len() - 1 {
            out.push(labels[start..].join("."));
        }
        out.push(labels[labels.len() - 1].to_string());
    }

    if let Some(prefix) = browser {
        let name = prefix.trim_end_matches('-');
        out.push(format!("{name}-browser"));
        out.push(name.to_string());
    }

    let mut seen = std::collections::HashSet::new();
    out.into_iter()
        .filter(|key| !key.is_empty() && seen.insert(key.clone()))
        .collect()
}

impl Provider for WindowsProvider {
    fn name(&self) -> &str {
        "windows"
    }
    fn pretty_name(&self) -> &str {
        "Windows"
    }

    fn query(&mut self, query: &str, limit: usize, exact: bool) -> Vec<Item> {
        let mut out = Vec::new();
        for window in compositor::windows() {
            let haystack = format!("{} {} {}", window.title, window.app_id, window.workspace);
            if let Some((score, info)) = fuzzy::score(query, &haystack, exact, "text") {
                // Window::id is already backend-qualified, so it round-trips straight back to
                // the compositor on activate.
                let mut item = Item::new(self.name(), window.id, window.title);
                item.subtext = format!("{} {}", window.app_id, window.workspace)
                    .trim()
                    .to_string();
                item.icon = self.icon_for(&window.app_id);
                item.actions = vec!["focus".into(), "close".into()];
                item.score = score + 5_000;
                item.fuzzyinfo = Some(info);
                out.push(item);
            }
        }
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
        match action {
            "focus" => compositor::focus_window(identifier),
            "close" => compositor::close_window(identifier),
            _ => anyhow::bail!("unsupported windows action: {action}"),
        }
    }

    fn capability(&self) -> ProviderCapability {
        ProviderCapability {
            name: self.name().into(),
            name_pretty: self.pretty_name().into(),
            description: "Search, focus, and close open windows".into(),
            icon: String::new(),
            prefixes: Vec::new(),
            actions: action_map(&[
                ("focus", ActionCapability::new("Focus")),
                ("close", ActionCapability::new("Close").destructive()),
            ]),
            supports_query: true,
            supports_activate: true,
            supports_streaming: true,
            supports_subscriptions: false,
            emits_events: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{icon_candidates, WindowsProvider};
    use std::collections::HashMap;

    fn provider(entries: &[(&str, &str)]) -> WindowsProvider {
        WindowsProvider::new(
            entries
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<HashMap<_, _>>(),
        )
    }

    #[test]
    fn matches_window_class_exactly() {
        let provider = provider(&[("com.mitchellh.ghostty", "ghostty-icon")]);
        assert_eq!(provider.icon_for("com.mitchellh.ghostty"), "ghostty-icon");
    }

    #[test]
    fn matches_chromium_webapp_class_by_host() {
        // Brave reports these classes; the .desktop files declare StartupWMClass=gmail / proton.me.
        let provider = provider(&[("gmail", "gmail-icon"), ("proton.me", "proton-icon")]);
        assert_eq!(provider.icon_for("brave-gmail.com__-Default"), "gmail-icon");
        assert_eq!(
            provider.icon_for("brave-mail.proton.me__u_0_inbox-Default"),
            "proton-icon"
        );
    }

    #[test]
    fn falls_back_to_the_browser_then_a_generic_window() {
        let provider = provider(&[("brave-browser", "brave-icon")]);
        assert_eq!(
            provider.icon_for("brave-unknown.example__-Default"),
            "brave-icon"
        );
        assert_eq!(
            provider.icon_for("some-unknown-app"),
            "preferences-system-windows"
        );
    }

    #[test]
    fn prefers_earlier_candidates() {
        let candidates = icon_candidates("brave-gmail.com__-Default");
        let gmail = candidates.iter().position(|c| c == "gmail").unwrap();
        let brave = candidates
            .iter()
            .position(|c| c == "brave-browser")
            .unwrap();
        assert!(gmail < brave);
    }
}
