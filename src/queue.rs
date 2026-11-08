use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::tmux::PaneInfo;

const DB_FILENAME: &str = "queue.db";
const MAX_HISTORY_ENTRIES: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    NeedsInput,
    Error,
    Visited,
    Running,
}

impl StopReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            StopReason::NeedsInput => "needs_input",
            StopReason::Error => "error",
            StopReason::Visited => "visited",
            StopReason::Running => "running",
        }
    }
}

impl fmt::Display for StopReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for StopReason {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "needs_input" => Ok(StopReason::NeedsInput),
            "error" => Ok(StopReason::Error),
            "visited" => Ok(StopReason::Visited),
            "running" => Ok(StopReason::Running),
            other => Err(anyhow!("invalid stop_reason: {other}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueEntry {
    pub pane_id: String,
    pub session_name: String,
    pub window_index: u32,
    pub window_name: String,
    pub stop_reason: StopReason,
    pub summary: String,
    pub timestamp: DateTime<Utc>,
    pub cwd: String,
}

impl QueueEntry {
    pub fn tmux_target(&self) -> String {
        format!(
            "{}:{}:{}",
            self.session_name, self.window_index, self.window_name
        )
    }
}

/// Validates that a tmux pane ID has the form `^%\d+$`.
pub fn is_valid_pane_id(pane_id: &str) -> bool {
    let Some(rest) = pane_id.strip_prefix('%') else {
        return false;
    };
    !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())
}

/// Resolves the queue directory:
/// 1. `$AGENT_QUEUE_DIR` (if set and non-empty)
/// 2. `dirs::data_local_dir().join("agent-queue")` (`~/.local/share/agent-queue` on Linux)
/// 3. `dirs::cache_dir().join("agent-queue")`
pub fn queue_dir() -> PathBuf {
    if let Ok(val) = env::var("AGENT_QUEUE_DIR") {
        let trimmed = val.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    if let Some(dir) = dirs::data_local_dir() {
        return dir.join("agent-queue");
    }
    if let Some(dir) = dirs::cache_dir() {
        return dir.join("agent-queue");
    }
    PathBuf::from("/tmp/agent-queue")
}

pub fn open_db(dir: &Path) -> Result<Connection> {
    fs::create_dir_all(dir)
        .with_context(|| format!("failed to create queue dir {}", dir.display()))?;
    let db_path = dir.join(DB_FILENAME);
    let conn = Connection::open(&db_path)
        .with_context(|| format!("failed to open SQLite database {}", db_path.display()))?;

    conn.busy_timeout(Duration::from_millis(5000))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS entries (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            pane_id       TEXT NOT NULL,
            state         TEXT NOT NULL,
            session_name  TEXT NOT NULL,
            window_index  INTEGER NOT NULL,
            window_name   TEXT NOT NULL,
            stop_reason   TEXT NOT NULL CHECK (stop_reason IN ('needs_input', 'error', 'visited', 'running')),
            summary       TEXT NOT NULL,
            timestamp     TEXT NOT NULL,
            cwd           TEXT NOT NULL,
            UNIQUE(state, pane_id)
        );
        CREATE INDEX IF NOT EXISTS idx_entries_state_pane ON entries(state, pane_id);",
    )?;

    Ok(conn)
}

fn prune_history_tx(conn: &Connection) -> Result<()> {
    conn.execute(
        "DELETE FROM entries
         WHERE state = 'history'
           AND id NOT IN (
               SELECT id FROM entries
               WHERE state = 'history'
               ORDER BY CASE WHEN stop_reason = 'running' THEN 0 ELSE 1 END ASC, id DESC
               LIMIT ?1
           )",
        params![MAX_HISTORY_ENTRIES as i64],
    )?;
    Ok(())
}

fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<QueueEntry> {
    let pane_id: String = row.get(0)?;
    let session_name: String = row.get(1)?;
    let window_index: u32 = row.get(2)?;
    let window_name: String = row.get(3)?;
    let stop_reason_str: String = row.get(4)?;
    let summary: String = row.get(5)?;
    let timestamp_str: String = row.get(6)?;
    let cwd: String = row.get(7)?;

    let stop_reason = StopReason::from_str(&stop_reason_str).unwrap_or(StopReason::NeedsInput);
    let timestamp = DateTime::parse_from_rfc3339(&timestamp_str)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now());

    Ok(QueueEntry {
        pane_id,
        session_name,
        window_index,
        window_name,
        stop_reason,
        summary,
        timestamp,
        cwd,
    })
}

pub fn upsert_entry(entry: &QueueEntry) -> Result<()> {
    upsert_entry_in(&queue_dir(), entry)
}

pub fn upsert_entry_in(dir: &Path, entry: &QueueEntry) -> Result<()> {
    if !is_valid_pane_id(&entry.pane_id) {
        return Ok(());
    }
    let mut conn = open_db(dir)?;
    let tx = conn.transaction()?;
    let ts = entry.timestamp.to_rfc3339();

    if entry.stop_reason == StopReason::Running {
        tx.execute(
            "INSERT OR REPLACE INTO entries (
                pane_id, state, session_name, window_index, window_name,
                stop_reason, summary, timestamp, cwd
            ) VALUES (?1, 'history', ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                entry.pane_id,
                entry.session_name,
                entry.window_index,
                entry.window_name,
                entry.stop_reason.as_str(),
                entry.summary,
                ts,
                entry.cwd,
            ],
        )?;
        tx.execute(
            "DELETE FROM entries WHERE state = 'waiting' AND pane_id = ?1",
            params![entry.pane_id],
        )?;
        prune_history_tx(&tx)?;
    } else {
        tx.execute(
            "INSERT INTO entries (
                pane_id, state, session_name, window_index, window_name,
                stop_reason, summary, timestamp, cwd
            ) VALUES (?1, 'waiting', ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            ON CONFLICT(state, pane_id) DO UPDATE SET
                session_name = excluded.session_name,
                window_index = excluded.window_index,
                window_name  = excluded.window_name,
                stop_reason  = excluded.stop_reason,
                summary      = excluded.summary,
                timestamp    = excluded.timestamp,
                cwd          = excluded.cwd",
            params![
                entry.pane_id,
                entry.session_name,
                entry.window_index,
                entry.window_name,
                entry.stop_reason.as_str(),
                entry.summary,
                ts,
                entry.cwd,
            ],
        )?;
        tx.execute(
            "DELETE FROM entries WHERE state = 'history' AND pane_id = ?1",
            params![entry.pane_id],
        )?;
    }

    tx.commit()?;
    Ok(())
}

pub fn mark_visited(pane_id: &str) -> Result<bool> {
    mark_visited_in(&queue_dir(), pane_id)
}

pub fn mark_visited_in(dir: &Path, pane_id: &str) -> Result<bool> {
    if !is_valid_pane_id(pane_id) {
        return Ok(false);
    }
    let conn = open_db(dir)?;
    let updated = conn.execute(
        "UPDATE entries SET stop_reason = 'visited' WHERE state = 'waiting' AND pane_id = ?1",
        params![pane_id],
    )?;
    Ok(updated > 0)
}

pub fn mark_running(
    pane_id: &str,
    pane_info: Option<&PaneInfo>,
    summary: Option<&str>,
    cwd: Option<&str>,
) -> Result<()> {
    mark_running_in(&queue_dir(), pane_id, pane_info, summary, cwd)
}

pub fn mark_running_in(
    dir: &Path,
    pane_id: &str,
    pane_info: Option<&PaneInfo>,
    summary: Option<&str>,
    cwd: Option<&str>,
) -> Result<()> {
    if !is_valid_pane_id(pane_id) {
        return Ok(());
    }
    let mut conn = open_db(dir)?;
    let tx = conn.transaction()?;

    // Look for an existing row for this pane_id (preferring waiting over history)
    let existing: Option<QueueEntry> = tx
        .query_row(
            "SELECT pane_id, session_name, window_index, window_name, stop_reason, summary, timestamp, cwd
             FROM entries
             WHERE pane_id = ?1
             ORDER BY CASE WHEN state = 'waiting' THEN 0 ELSE 1 END ASC
             LIMIT 1",
            params![pane_id],
            row_to_entry,
        )
        .optional()?;

    let (session_name, window_index, window_name) = match (pane_info, &existing) {
        (Some(info), _) => (
            info.session_name.clone(),
            info.window_index,
            info.window_name.clone(),
        ),
        (None, Some(ex)) => (
            ex.session_name.clone(),
            ex.window_index,
            ex.window_name.clone(),
        ),
        (None, None) => {
            tx.execute(
                "DELETE FROM entries WHERE state = 'waiting' AND pane_id = ?1",
                params![pane_id],
            )?;
            tx.commit()?;
            return Ok(());
        }
    };

    let resolved_summary = summary
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            existing
                .as_ref()
                .map(|e| e.summary.trim().to_owned())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "Running".to_string());

    let resolved_cwd = cwd
        .filter(|c| !c.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| existing.as_ref().map(|e| e.cwd.clone()))
        .unwrap_or_default();

    let now = Utc::now().to_rfc3339();

    tx.execute(
        "INSERT OR REPLACE INTO entries (
            pane_id, state, session_name, window_index, window_name,
            stop_reason, summary, timestamp, cwd
        ) VALUES (?1, 'history', ?2, ?3, ?4, 'running', ?5, ?6, ?7)",
        params![
            pane_id,
            session_name,
            window_index,
            window_name,
            resolved_summary,
            now,
            resolved_cwd,
        ],
    )?;

    tx.execute(
        "DELETE FROM entries WHERE state = 'waiting' AND pane_id = ?1",
        params![pane_id],
    )?;

    prune_history_tx(&tx)?;
    tx.commit()?;
    Ok(())
}

pub fn clear_entry(pane_id: &str, to_history: bool) -> Result<bool> {
    clear_entry_in(&queue_dir(), pane_id, to_history)
}

pub fn clear_entry_in(dir: &Path, pane_id: &str, to_history: bool) -> Result<bool> {
    if !is_valid_pane_id(pane_id) {
        return Ok(false);
    }
    let mut conn = open_db(dir)?;
    let tx = conn.transaction()?;

    if to_history {
        let waiting: Option<QueueEntry> = tx
            .query_row(
                "SELECT pane_id, session_name, window_index, window_name, stop_reason, summary, timestamp, cwd
                 FROM entries
                 WHERE state = 'waiting' AND pane_id = ?1",
                params![pane_id],
                row_to_entry,
            )
            .optional()?;

        if let Some(entry) = waiting {
            let normalized_reason = match entry.stop_reason {
                StopReason::Visited => StopReason::NeedsInput,
                other => other,
            };
            tx.execute(
                "INSERT OR REPLACE INTO entries (
                    pane_id, state, session_name, window_index, window_name,
                    stop_reason, summary, timestamp, cwd
                ) VALUES (?1, 'history', ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    entry.pane_id,
                    entry.session_name,
                    entry.window_index,
                    entry.window_name,
                    normalized_reason.as_str(),
                    entry.summary,
                    entry.timestamp.to_rfc3339(),
                    entry.cwd,
                ],
            )?;
            tx.execute(
                "DELETE FROM entries WHERE state = 'waiting' AND pane_id = ?1",
                params![pane_id],
            )?;
            prune_history_tx(&tx)?;
            tx.commit()?;
            return Ok(true);
        }
        tx.commit()?;
        Ok(false)
    } else {
        let deleted = tx.execute(
            "DELETE FROM entries
             WHERE pane_id = ?1
               AND (state = 'waiting' OR (state = 'history' AND stop_reason = 'running'))",
            params![pane_id],
        )?;
        let total_deleted = if deleted == 0 {
            tx.execute(
                "DELETE FROM entries WHERE state = 'history' AND pane_id = ?1",
                params![pane_id],
            )?
        } else {
            deleted
        };
        tx.commit()?;
        Ok(total_deleted > 0)
    }
}

pub fn remove_history_entry(pane_id: &str) -> Result<bool> {
    remove_history_entry_in(&queue_dir(), pane_id)
}

pub fn remove_history_entry_in(dir: &Path, pane_id: &str) -> Result<bool> {
    let conn = open_db(dir)?;
    let deleted = conn.execute(
        "DELETE FROM entries WHERE state = 'history' AND pane_id = ?1",
        params![pane_id],
    )?;
    Ok(deleted > 0)
}

pub fn read_queue_dir(dir: &Path) -> Result<Vec<QueueEntry>> {
    if !dir.join(DB_FILENAME).exists() {
        return Ok(Vec::new());
    }
    let conn = open_db(dir)?;
    let mut stmt = conn.prepare(
        "SELECT pane_id, session_name, window_index, window_name, stop_reason, summary, timestamp, cwd
         FROM entries
         WHERE state = 'waiting'
         ORDER BY CASE WHEN stop_reason = 'visited' THEN 1 ELSE 0 END ASC, timestamp ASC, id ASC",
    )?;
    let rows = stmt.query_map([], row_to_entry)?;
    let mut entries = Vec::new();
    for row in rows {
        entries.push(row?);
    }
    Ok(entries)
}

pub fn read_history(dir: &Path) -> Result<Vec<QueueEntry>> {
    if !dir.join(DB_FILENAME).exists() {
        return Ok(Vec::new());
    }
    let conn = open_db(dir)?;
    let mut stmt = conn.prepare(
        "SELECT pane_id, session_name, window_index, window_name, stop_reason, summary, timestamp, cwd
         FROM entries
         WHERE state = 'history'
         ORDER BY CASE WHEN stop_reason = 'running' THEN 0 ELSE 1 END ASC, id DESC",
    )?;
    let rows = stmt.query_map([], row_to_entry)?;
    let mut entries = Vec::new();
    for row in rows {
        entries.push(row?);
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use tempfile::TempDir;

    fn sample_entry(pane_id: &str, stop_reason: StopReason, summary: &str, secs: i64) -> QueueEntry {
        QueueEntry {
            pane_id: pane_id.to_string(),
            session_name: "work".to_string(),
            window_index: 1,
            window_name: "code".to_string(),
            stop_reason,
            summary: summary.to_string(),
            timestamp: Utc.timestamp_opt(1_700_000_000 + secs, 0).unwrap(),
            cwd: "/home/user/project".to_string(),
        }
    }

    #[test]
    fn test_pane_id_validation() {
        assert!(is_valid_pane_id("%0"));
        assert!(is_valid_pane_id("%42"));
        assert!(!is_valid_pane_id(""));
        assert!(!is_valid_pane_id("%"));
        assert!(!is_valid_pane_id("42"));
        assert!(!is_valid_pane_id("%42a"));
    }

    #[test]
    fn test_tmux_target_formatting() {
        let entry = sample_entry("%1", StopReason::NeedsInput, "hello", 0);
        assert_eq!(entry.tmux_target(), "work:1:code");
    }

    #[test]
    fn test_single_state_invariant_waiting_and_history() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        // Insert into waiting
        let e1 = sample_entry("%1", StopReason::NeedsInput, "waiting 1", 10);
        upsert_entry_in(dir, &e1).unwrap();
        assert_eq!(read_queue_dir(dir).unwrap().len(), 1);
        assert_eq!(read_history(dir).unwrap().len(), 0);

        // Move to running in history via mark_running
        mark_running_in(dir, "%1", None, Some("working..."), None).unwrap();
        assert_eq!(read_queue_dir(dir).unwrap().len(), 0);
        let hist = read_history(dir).unwrap();
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].stop_reason, StopReason::Running);
        assert_eq!(hist[0].summary, "working...");

        // Upsert waiting again removes history row
        let e2 = sample_entry("%1", StopReason::Error, "failed", 20);
        upsert_entry_in(dir, &e2).unwrap();
        let waiting = read_queue_dir(dir).unwrap();
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].stop_reason, StopReason::Error);
        assert_eq!(read_history(dir).unwrap().len(), 0);

        // Upsert with Running moves directly to history and removes waiting
        let e3 = sample_entry("%1", StopReason::Running, "restarted", 30);
        upsert_entry_in(dir, &e3).unwrap();
        assert_eq!(read_queue_dir(dir).unwrap().len(), 0);
        let hist = read_history(dir).unwrap();
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].stop_reason, StopReason::Running);
    }

    #[test]
    fn test_waiting_ordering_unvisited_first_fifo() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        upsert_entry_in(dir, &sample_entry("%1", StopReason::NeedsInput, "first", 10)).unwrap();
        upsert_entry_in(dir, &sample_entry("%2", StopReason::Error, "second", 20)).unwrap();
        upsert_entry_in(dir, &sample_entry("%3", StopReason::NeedsInput, "third", 30)).unwrap();

        // Mark %1 as visited; it should move after unvisited (%2, %3)
        assert!(mark_visited_in(dir, "%1").unwrap());

        let waiting = read_queue_dir(dir).unwrap();
        let ids: Vec<&str> = waiting.iter().map(|e| e.pane_id.as_str()).collect();
        assert_eq!(ids, vec!["%2", "%3", "%1"]);
        assert_eq!(waiting[2].stop_reason, StopReason::Visited);
    }

    #[test]
    fn test_clear_entry_to_history_normalizes_visited() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        upsert_entry_in(dir, &sample_entry("%5", StopReason::NeedsInput, "done", 10)).unwrap();
        mark_visited_in(dir, "%5").unwrap();

        assert!(clear_entry_in(dir, "%5", true).unwrap());
        assert!(read_queue_dir(dir).unwrap().is_empty());

        let history = read_history(dir).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].pane_id, "%5");
        assert_eq!(history[0].stop_reason, StopReason::NeedsInput);
    }

    #[test]
    fn test_clear_entry_no_history_deletes_rows() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        upsert_entry_in(dir, &sample_entry("%7", StopReason::NeedsInput, "msg", 10)).unwrap();
        assert!(clear_entry_in(dir, "%7", false).unwrap());
        assert!(read_queue_dir(dir).unwrap().is_empty());
        assert!(read_history(dir).unwrap().is_empty());

        // Also deletes history entry when called with to_history = false
        upsert_entry_in(dir, &sample_entry("%8", StopReason::NeedsInput, "msg2", 20)).unwrap();
        clear_entry_in(dir, "%8", true).unwrap();
        assert_eq!(read_history(dir).unwrap().len(), 1);
        assert!(clear_entry_in(dir, "%8", false).unwrap());
        assert!(read_history(dir).unwrap().is_empty());
    }

    #[test]
    fn test_history_ordering_and_pruning_to_100() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        // Add 105 completed history items
        for i in 1..=105 {
            let pane = format!("%{i}");
            upsert_entry_in(dir, &sample_entry(&pane, StopReason::NeedsInput, "done", i)).unwrap();
            clear_entry_in(dir, &pane, true).unwrap();
        }

        // Add 1 running item
        let info = PaneInfo {
            session_name: "main".to_string(),
            window_index: 0,
            window_name: "agent".to_string(),
        };
        mark_running_in(dir, "%999", Some(&info), Some("active task"), Some("/tmp")).unwrap();

        let history = read_history(dir).unwrap();
        assert_eq!(history.len(), 100);
        // Running entry is first, followed by newest completed (%105, %104, ...)
        assert_eq!(history[0].pane_id, "%999");
        assert_eq!(history[0].stop_reason, StopReason::Running);
        assert_eq!(history[1].pane_id, "%105");
        assert_eq!(history[2].pane_id, "%104");
    }
}
