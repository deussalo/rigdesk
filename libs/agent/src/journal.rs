use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::now_ms;

/// The journal holds what the agent typed and ran, so it is readable by its owner alone.
fn open_private(path: &std::path::Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let file = options.open(path)?;
    // A journal made before this rule keeps its old mode unless it is narrowed here.
    #[cfg(unix)]
    let _ = file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600));
    Ok(file)
}

/// One line of the action log: who did what, when, and how it ended.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub at_ms: u64,
    pub who: String,
    pub what: String,
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub detail: Value,
}

impl Entry {
    pub fn new(who: impl Into<String>, what: impl Into<String>, outcome: impl Into<String>, detail: Value) -> Self {
        Entry { at_ms: now_ms(), who: who.into(), what: what.into(), outcome: outcome.into(), detail }
    }
}

/// Append-only JSONL on disk, rotated to `<name>.1` past `max_bytes`; one previous file is kept.
pub struct Journal {
    path: Option<PathBuf>,
    max_bytes: u64,
    file: Mutex<Option<File>>,
}

impl Journal {
    pub fn open(path: PathBuf, max_bytes: u64) -> Self {
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let file = open_private(&path);
        if let Err(e) = &file {
            log::error!("agent journal {} cannot be opened: {e}", path.display());
        }
        Journal { path: Some(path), max_bytes, file: Mutex::new(file.ok()) }
    }

    /// No file: entries go to the log only. For tests and hosts without a data dir.
    pub fn memory() -> Self {
        Journal { path: None, max_bytes: 0, file: Mutex::new(None) }
    }

    fn rotated(&self) -> Option<PathBuf> {
        self.path.as_ref().map(|p| {
            let mut name = p.file_name().unwrap_or_default().to_os_string();
            name.push(".1");
            p.with_file_name(name)
        })
    }

    pub fn record(&self, entry: &Entry) {
        log::info!("agent journal: {} {} {}", entry.who, entry.what, entry.outcome);
        let Some(path) = &self.path else { return };
        let Ok(mut line) = serde_json::to_string(entry) else { return };
        line.push('\n');
        let mut file = self.file.lock().unwrap();
        let full = file.as_ref().and_then(|f| f.metadata().ok()).map_or(false, |m| m.len() + line.len() as u64 > self.max_bytes);
        if full {
            *file = None;
            if let Some(old) = self.rotated() {
                let _ = fs::rename(path, old);
            }
            *file = open_private(path).ok();
        }
        match file.as_mut() {
            Some(f) => {
                if let Err(e) = f.write_all(line.as_bytes()) {
                    log::error!("agent journal write failed: {e}");
                }
            }
            None => log::error!("agent journal has no file; entry kept in the log only"),
        }
    }

    /// The newest `n` entries, oldest first, across the current file and the rotated one.
    pub fn tail(&self, n: usize) -> Vec<Entry> {
        let Some(path) = &self.path else { return Vec::new() };
        let mut entries = Vec::new();
        for p in [self.rotated(), Some(path.clone())].into_iter().flatten() {
            if let Ok(f) = File::open(&p) {
                entries.extend(BufReader::new(f).lines().map_while(Result::ok).filter_map(|l| serde_json::from_str::<Entry>(&l).ok()));
            }
        }
        let skip = entries.len().saturating_sub(n);
        entries.split_off(skip)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_and_tails_across_files() {
        let dir = std::env::temp_dir().join(format!("rigdesk-journal-{}", rand::random::<u32>()));
        let journal = Journal::open(dir.join("agent.jsonl"), 300);
        for i in 0..10 {
            journal.record(&Entry::new("agent a1", format!("click {i}"), "ok", Value::Null));
        }
        assert!(dir.join("agent.jsonl.1").exists());
        let tail = journal.tail(3);
        assert_eq!(tail.iter().map(|e| e.what.as_str()).collect::<Vec<_>>(), vec!["click 7", "click 8", "click 9"]);
        let _ = fs::remove_dir_all(dir);
    }
}
