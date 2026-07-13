use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

const KEEP_ON_LOAD: usize = 1000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub at: DateTime<Local>,
    pub tunnel: String,
    pub event: String,
    pub detail: String,
}

/// Append-only event log, persisted as JSON lines so past sessions survive.
pub struct History {
    path: PathBuf,
    entries: Vec<HistoryEntry>,
}

impl History {
    pub fn load() -> Self {
        let path = crate::config::history_path();
        let mut entries: Vec<HistoryEntry> = Vec::new();
        if let Ok(text) = fs::read_to_string(&path) {
            for line in text.lines() {
                if let Ok(entry) = serde_json::from_str(line) {
                    entries.push(entry);
                }
            }
        }
        let excess = entries.len().saturating_sub(KEEP_ON_LOAD);
        entries.drain(..excess);
        Self { path, entries }
    }

    pub fn record(&mut self, tunnel: &str, event: &str, detail: impl Into<String>) {
        let entry = HistoryEntry {
            at: Local::now(),
            tunnel: tunnel.to_string(),
            event: event.to_string(),
            detail: detail.into(),
        };
        if let Ok(line) = serde_json::to_string(&entry) {
            if let Some(parent) = self.path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&self.path) {
                let _ = writeln!(file, "{line}");
            }
        }
        self.entries.push(entry);
        if self.entries.len() > KEEP_ON_LOAD * 2 {
            self.entries.drain(..KEEP_ON_LOAD);
        }
    }

    pub fn entries(&self) -> &[HistoryEntry] {
        &self.entries
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}
