use std::path::PathBuf;

use anyhow::Result;

use crate::input::Action;
use crate::queue::{
    clear_entry_in, mark_visited_in, read_history, read_queue_dir, remove_history_entry_in,
    QueueEntry, StopReason,
};
use crate::tmux::jump_to_pane;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    Waiting,
    History,
}

#[derive(Debug)]
pub struct App {
    pub entries: Vec<QueueEntry>,
    pub history: Vec<QueueEntry>,
    pub selected: usize,
    pub filter: Filter,
    pub search: Option<String>,
    pub search_active: bool,
    pub running: bool,
    pub queue_dir: PathBuf,
}

impl App {
    pub fn new(queue_dir: PathBuf) -> Result<Self> {
        let mut app = Self {
            entries: Vec::new(),
            history: Vec::new(),
            selected: 0,
            filter: Filter::Waiting,
            search: None,
            search_active: false,
            running: true,
            queue_dir,
        };
        app.reload()?;
        Ok(app)
    }

    pub fn reload(&mut self) -> Result<()> {
        self.entries = read_queue_dir(&self.queue_dir)?;
        self.history = read_history(&self.queue_dir)?;
        self.clamp_selected();
        Ok(())
    }

    pub fn active_source(&self) -> &[QueueEntry] {
        match self.filter {
            Filter::Waiting => &self.entries,
            Filter::History => &self.history,
        }
    }

    pub fn matches_search(entry: &QueueEntry, query: &str) -> bool {
        if query.is_empty() {
            return true;
        }
        let q = query.to_lowercase();
        entry.session_name.to_lowercase().contains(&q)
            || entry.window_name.to_lowercase().contains(&q)
            || entry.tmux_target().to_lowercase().contains(&q)
            || entry.summary.to_lowercase().contains(&q)
            || entry.cwd.to_lowercase().contains(&q)
    }

    pub fn filtered_entries(&self) -> Vec<&QueueEntry> {
        let query = self.search.as_deref().unwrap_or("");
        self.active_source()
            .iter()
            .filter(|e| Self::matches_search(e, query))
            .collect()
    }

    pub fn selected_entry(&self) -> Option<QueueEntry> {
        let filtered = self.filtered_entries();
        filtered.get(self.selected).map(|e| (*e).clone())
    }

    pub fn clamp_selected(&mut self) {
        let len = self.filtered_entries().len();
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }

    pub fn waiting_counts(&self) -> (usize, usize) {
        let visited = self
            .entries
            .iter()
            .filter(|e| e.stop_reason == StopReason::Visited)
            .count();
        let unvisited = self.entries.len().saturating_sub(visited);
        (unvisited, visited)
    }

    pub fn history_counts(&self) -> (usize, usize) {
        let total = self.history.len();
        let running = self
            .history
            .iter()
            .filter(|e| e.stop_reason == StopReason::Running)
            .count();
        (total, running)
    }

    pub fn handle_action(&mut self, action: Action) -> Result<()> {
        match action {
            Action::None => {}
            Action::Down => {
                let len = self.filtered_entries().len();
                if len > 0 {
                    self.selected = (self.selected + 1).min(len - 1);
                }
            }
            Action::Up => {
                self.selected = self.selected.saturating_sub(1);
            }
            Action::ToggleFilter => {
                self.filter = match self.filter {
                    Filter::Waiting => Filter::History,
                    Filter::History => Filter::Waiting,
                };
                self.selected = 0;
                self.clamp_selected();
            }
            Action::JumpAndVisit => {
                if let Some(entry) = self.selected_entry() {
                    let _ = jump_to_pane(&entry);
                    if self.filter == Filter::Waiting {
                        let _ = mark_visited_in(&self.queue_dir, &entry.pane_id);
                    }
                    self.running = false;
                }
            }
            Action::Open => {
                if let Some(entry) = self.selected_entry() {
                    let _ = jump_to_pane(&entry);
                    self.running = false;
                }
            }
            Action::Dismiss => {
                if let Some(entry) = self.selected_entry() {
                    match self.filter {
                        Filter::Waiting => {
                            clear_entry_in(&self.queue_dir, &entry.pane_id, true)?;
                        }
                        Filter::History => {
                            remove_history_entry_in(&self.queue_dir, &entry.pane_id)?;
                        }
                    }
                    self.reload()?;
                }
            }
            Action::StartSearch => {
                self.search_active = true;
                if self.search.is_none() {
                    self.search = Some(String::new());
                }
            }
            Action::SearchInput(c) => {
                self.search.get_or_insert_with(String::new).push(c);
                self.clamp_selected();
            }
            Action::SearchBackspace => {
                if let Some(ref mut q) = self.search {
                    q.pop();
                    if q.is_empty() {
                        self.search = None;
                        self.search_active = false;
                    }
                } else {
                    self.search_active = false;
                }
                self.clamp_selected();
            }
            Action::LockSearch => {
                self.search_active = false;
                if self.search.as_deref().is_some_and(str::is_empty) {
                    self.search = None;
                }
                self.clamp_selected();
            }
            Action::ClearSearch => {
                self.search = None;
                self.search_active = false;
                self.clamp_selected();
            }
            Action::Quit => {
                self.running = false;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::upsert_entry_in;
    use chrono::{TimeZone, Utc};
    use tempfile::TempDir;

    fn make_entry(pane_id: &str, session: &str, summary: &str, reason: StopReason) -> QueueEntry {
        QueueEntry {
            pane_id: pane_id.to_string(),
            session_name: session.to_string(),
            window_index: 1,
            window_name: "dev".to_string(),
            stop_reason: reason,
            summary: summary.to_string(),
            timestamp: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            cwd: "/workspace/repo".to_string(),
        }
    }

    #[test]
    fn test_app_search_and_navigation() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().to_path_buf();

        upsert_entry_in(&dir, &make_entry("%1", "alpha", "Fix parser bug", StopReason::NeedsInput)).unwrap();
        upsert_entry_in(&dir, &make_entry("%2", "beta", "Add SQLite tests", StopReason::Error)).unwrap();

        let mut app = App::new(dir).unwrap();
        assert_eq!(app.filtered_entries().len(), 2);
        assert_eq!(app.selected, 0);

        app.handle_action(Action::Down).unwrap();
        assert_eq!(app.selected, 1);

        // Start search and filter to "sqlite"
        app.handle_action(Action::StartSearch).unwrap();
        assert!(app.search_active);
        for c in "sqlite".chars() {
            app.handle_action(Action::SearchInput(c)).unwrap();
        }
        assert_eq!(app.filtered_entries().len(), 1);
        assert_eq!(app.selected, 0);
        assert_eq!(app.selected_entry().unwrap().pane_id, "%2");

        // Lock search
        app.handle_action(Action::LockSearch).unwrap();
        assert!(!app.search_active);
        assert_eq!(app.search.as_deref(), Some("sqlite"));

        // Clear search
        app.handle_action(Action::ClearSearch).unwrap();
        assert!(app.search.is_none());
        assert_eq!(app.filtered_entries().len(), 2);
    }

    #[test]
    fn test_app_dismiss_waiting_to_history_and_delete_from_history() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().to_path_buf();

        upsert_entry_in(&dir, &make_entry("%1", "alpha", "Task 1", StopReason::NeedsInput)).unwrap();
        let mut app = App::new(dir).unwrap();
        assert_eq!(app.entries.len(), 1);
        assert_eq!(app.history.len(), 0);

        // Dismiss from Waiting -> moves to History
        app.handle_action(Action::Dismiss).unwrap();
        assert_eq!(app.entries.len(), 0);
        assert_eq!(app.history.len(), 1);

        // Toggle to History tab and dismiss -> deletes from History
        app.handle_action(Action::ToggleFilter).unwrap();
        assert_eq!(app.filter, Filter::History);
        app.handle_action(Action::Dismiss).unwrap();
        assert_eq!(app.history.len(), 0);
    }
}
