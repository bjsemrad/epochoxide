use crate::types::Item;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, path::PathBuf, time::{SystemTime, UNIX_EPOCH}};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct UsageEntry {
    count: u32,
    last_used_epoch: u64,
}

#[derive(Debug)]
pub struct UsageHistory {
    entries: HashMap<String, UsageEntry>,
    path: PathBuf,
}

impl UsageHistory {
    pub fn load() -> Self {
        let path = dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("epochoxide/history.json");
        let entries = fs::read_to_string(&path).ok().and_then(|raw| serde_json::from_str(&raw).ok()).unwrap_or_default();
        Self { entries, path }
    }

    pub fn apply(&self, item: &mut Item, query: &str) {
        let key = key(&item.provider, &item.identifier);
        let Some(entry) = self.entries.get(&key) else { return; };
        let count_bonus = (entry.count.min(50) as i32) * 250;
        let recency_bonus = recency_bonus(entry.last_used_epoch);
        let query_bonus = if !query.is_empty() { 500 } else { 0 };
        item.score += count_bonus + recency_bonus + query_bonus;
        item.state.push("history".to_string());
    }

    pub fn record(&mut self, provider: &str, identifier: &str) -> Result<()> {
        let entry = self.entries.entry(key(provider, identifier)).or_default();
        entry.count = entry.count.saturating_add(1);
        entry.last_used_epoch = now_epoch();
        self.save()
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() { fs::create_dir_all(parent)?; }
        fs::write(&self.path, serde_json::to_vec(&self.entries)?)?;
        Ok(())
    }
}

fn key(provider: &str, identifier: &str) -> String { format!("{provider}:{identifier}") }

fn now_epoch() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default()
}

fn recency_bonus(last_used_epoch: u64) -> i32 {
    let age = now_epoch().saturating_sub(last_used_epoch);
    match age {
        0..=3600 => 10_000,
        3601..=86_400 => 5_000,
        86_401..=604_800 => 2_000,
        604_801..=2_592_000 => 750,
        _ => 100,
    }
}

#[cfg(test)]
mod tests {
    use super::recency_bonus;

    #[test]
    fn recency_bonus_prefers_recent_items() {
        let now = super::now_epoch();
        assert!(recency_bonus(now) > recency_bonus(now.saturating_sub(3_000_000)));
    }
}
