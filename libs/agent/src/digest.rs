//! The list of what the agent shell ran, for the person at the PC: a file every user may read and only the lane may write,
//! which the tray's "Managed by Rig: commands run here" opens. The host writes it, with its own line ends and rights.

use std::sync::Arc;

use crate::{events::EventKind, journal::Journal, session::Shared};

/// How many commands the list shows.
pub const LINES: usize = 200;

/// The list, newest first. `highest` names the most a command may run as there: SYSTEM or root.
pub fn text(journal: &Journal, highest: &str, newline: &str) -> String {
    let mut text = format!(
        "This PC is managed by Rig. Agents may run commands on it as {highest} or as the signed-in user, without a window, and every{newline}command is listed here, newest first. Only an administrator can turn this off, by running the install again.{newline}{newline}"
    );
    let entries: Vec<_> = journal.tail(4 * LINES).into_iter().filter(|e| e.what == "shell").collect();
    if entries.is_empty() {
        text.push_str(&format!("No commands have run yet.{newline}"));
    }
    for entry in entries.iter().rev().take(LINES) {
        let detail = &entry.detail;
        let as_whom = detail.get("as").and_then(|v| v.as_str()).unwrap_or("-");
        let as_whom = if as_whom == "system" { highest } else { as_whom };
        let graph = detail.get("graph").and_then(|v| v.as_str()).unwrap_or("");
        let command: String = detail.get("command").and_then(|v| v.as_str()).unwrap_or("").chars().take(300).collect::<String>().replace(['\r', '\n'], " ");
        let took = detail.get("took_ms").and_then(|v| v.as_u64()).map_or(String::new(), |ms| format!(", {:.1} s", ms as f64 / 1000.0));
        text.push_str(&format!("{}  {}  [{graph}] as {as_whom}: {}{took}{newline}    {command}{newline}", utc(entry.at_ms), entry.who, entry.outcome));
    }
    text
}

/// Calls `write` after every command the lane journals, for as long as the process runs.
pub async fn keep(shared: Arc<Shared>, write: impl Fn() + Clone + Send + Sync + 'static) {
    use tokio::sync::broadcast::error::RecvError;
    let mut events = shared.events.subscribe();
    loop {
        match events.recv().await {
            Ok(event) if matches!(event.kind, EventKind::Shell { .. }) => {
                let _ = tokio::task::spawn_blocking(write.clone()).await;
            }
            Ok(_) | Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => break,
        }
    }
}

fn utc(at_ms: u64) -> String {
    let secs = at_ms / 1000;
    let (days, day) = (secs / 86_400, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02} UTC", day / 3600, day / 60 % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::Entry;
    use serde_json::json;

    #[test]
    fn the_list_names_root_or_system_and_shows_the_newest_first() {
        let dir = std::env::temp_dir().join(format!("rigdesk-digest-{:08x}", rand::random::<u32>()));
        let journal = Journal::open(dir.join("a.jsonl"), 1 << 20);
        assert!(text(&journal, "root", "\n").contains("No commands have run yet."));
        journal.record(&Entry::new("Ops (a1)", "shell", "exit 0", json!({"as": "system", "graph": "fleet", "command": "apt-get update", "took_ms": 1500})));
        journal.record(&Entry::new("Ops (a1)", "shell", "exit 1", json!({"as": "user", "graph": "fleet", "command": "ls\n-la"})));
        let list = text(&journal, "root", "\n");
        assert!(list.contains("as root or as the signed-in user"));
        let (newest, oldest) = (list.find("as user: exit 1").unwrap(), list.find("as root: exit 0, 1.5 s").unwrap());
        assert!(newest < oldest, "{list}");
        assert!(list.contains("    ls -la\n"));
        assert_eq!(utc(0), "1970-01-01 00:00 UTC");
        let _ = std::fs::remove_dir_all(dir);
    }
}
