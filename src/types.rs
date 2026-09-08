use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ItemType {
    Regular,
    File,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FuzzyInfo {
    pub start: usize,
    pub field: String,
    pub positions: Vec<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Item {
    pub identifier: String,
    pub text: String,
    pub subtext: String,
    pub icon: String,
    pub provider: String,
    pub score: i32,
    pub fuzzyinfo: Option<FuzzyInfo>,
    pub item_type: ItemType,
    pub mimetype: String,
    pub preview: String,
    pub preview_type: String,
    pub state: Vec<String>,
    pub actions: Vec<String>,
    pub icon_path: Option<String>,
    pub thumbnail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActionCapability {
    pub label: String,
    pub needs_args: bool,
    pub destructive: bool,
    pub async_action: bool,
    pub terminal: bool,
    pub confirmation: bool,
}

impl ActionCapability {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            needs_args: false,
            destructive: false,
            async_action: false,
            terminal: false,
            confirmation: false,
        }
    }

    pub fn destructive(mut self) -> Self {
        self.destructive = true;
        self.confirmation = true;
        self
    }
    pub fn needs_args(mut self) -> Self {
        self.needs_args = true;
        self
    }
    pub fn async_action(mut self) -> Self {
        self.async_action = true;
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderCapability {
    pub name: String,
    pub name_pretty: String,
    pub description: String,
    pub icon: String,
    pub prefixes: Vec<String>,
    pub actions: HashMap<String, ActionCapability>,
    pub supports_query: bool,
    pub supports_activate: bool,
    pub supports_streaming: bool,
    pub supports_subscriptions: bool,
    pub emits_events: bool,
}

impl Item {
    pub fn new(provider: &str, identifier: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            identifier: identifier.into(),
            text: text.into(),
            subtext: String::new(),
            icon: String::new(),
            provider: provider.to_string(),
            score: 0,
            fuzzyinfo: None,
            item_type: ItemType::Regular,
            mimetype: String::new(),
            preview: String::new(),
            preview_type: String::new(),
            state: Vec::new(),
            actions: Vec::new(),
            icon_path: None,
            thumbnail: None,
        }
    }
}

pub fn action_map(actions: &[(&str, ActionCapability)]) -> HashMap<String, ActionCapability> {
    actions
        .iter()
        .map(|(name, cap)| ((*name).to_string(), cap.clone()))
        .collect()
}
