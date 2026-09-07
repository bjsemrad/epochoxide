use serde::{Deserialize, Serialize};

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
        }
    }
}
