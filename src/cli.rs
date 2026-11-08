use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use std::str::FromStr;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use chrono::Utc;
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::queue::{
    clear_entry_in, is_valid_pane_id, mark_running_in, mark_visited_in, queue_dir, read_history,
    read_queue_dir, upsert_entry_in, QueueEntry, StopReason,
};
use crate::tmux::{
    get_pane_info, jump_to_pane, pane_exists, prune_stale_panes_in, prune_stale_running_panes_in,
};

const MAX_SUMMARY_CHARS: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Tui,
    Push {
        pane: Option<String>,
        stop_reason: StopReason,
        summary: Option<String>,
        cwd: Option<String>,
    },
    Clear {
        pane: Option<String>,
        mode: ClearMode,
        summary: Option<String>,
        cwd: Option<String>,
    },
    Visit {
        pane: Option<String>,
    },
    Jump {
        pane: Option<String>,
        after: JumpAfter,
    },
    List {
        state: ListState,
        json: bool,
    },
    Watch,
    Hook {
        event: HookEvent,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClearMode {
    ToHistory,
    NoHistory,
    Running,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JumpAfter {
    None,
    Visit,
    Dismiss,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListState {
    Waiting,
    History,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    Stop,
    Notification,
    PromptSubmit,
}

impl FromStr for HookEvent {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "stop" => Ok(HookEvent::Stop),
            "notification" => Ok(HookEvent::Notification),
            "prompt-submit" => Ok(HookEvent::PromptSubmit),
            other => Err(anyhow!(
                "unknown hook event '{other}' (expected stop, notification, or prompt-submit)"
            )),
        }
    }
}

pub fn parse_args<I, S>(args: I) -> Result<Command>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut iter = args.into_iter().map(|s| s.as_ref().to_string());
    let Some(sub) = iter.next() else {
        return Ok(Command::Tui);
    };

    match sub.as_str() {
        "push" => {
            let mut pane = None;
            let mut stop_reason = StopReason::NeedsInput;
            let mut summary = None;
            let mut cwd = None;

            while let Some(arg) = iter.next() {
                match arg.as_str() {
                    "--pane" => {
                        pane = Some(
                            iter.next()
                                .ok_or_else(|| anyhow!("--pane requires a value"))?,
                        );
                    }
                    "--stop-reason" => {
                        let val = iter
                            .next()
                            .ok_or_else(|| anyhow!("--stop-reason requires a value"))?;
                        stop_reason = StopReason::from_str(&val)?;
                    }
                    "--summary" => {
                        summary = Some(
                            iter.next()
                                .ok_or_else(|| anyhow!("--summary requires a value"))?,
                        );
                    }
                    "--cwd" => {
                        cwd = Some(
                            iter.next()
                                .ok_or_else(|| anyhow!("--cwd requires a value"))?,
                        );
                    }
                    other => bail!("unknown argument for push: {other}"),
                }
            }
            Ok(Command::Push {
                pane,
                stop_reason,
                summary,
                cwd,
            })
        }
        "clear" => {
            let mut pane = None;
            let mut no_history = false;
            let mut running = false;
            let mut summary = None;
            let mut cwd = None;

            while let Some(arg) = iter.next() {
                match arg.as_str() {
                    "--pane" => {
                        pane = Some(
                            iter.next()
                                .ok_or_else(|| anyhow!("--pane requires a value"))?,
                        );
                    }
                    "--no-history" => no_history = true,
                    "--running" => running = true,
                    "--summary" => {
                        summary = Some(
                            iter.next()
                                .ok_or_else(|| anyhow!("--summary requires a value"))?,
                        );
                    }
                    "--cwd" => {
                        cwd = Some(
                            iter.next()
                                .ok_or_else(|| anyhow!("--cwd requires a value"))?,
                        );
                    }
                    other => bail!("unknown argument for clear: {other}"),
                }
            }
            if no_history && running {
                bail!("--no-history and --running are mutually exclusive");
            }
            let mode = if no_history {
                ClearMode::NoHistory
            } else if running {
                ClearMode::Running
            } else {
                ClearMode::ToHistory
            };
            Ok(Command::Clear {
                pane,
                mode,
                summary,
                cwd,
            })
        }
        "visit" => {
            let mut pane = None;
            while let Some(arg) = iter.next() {
                match arg.as_str() {
                    "--pane" => {
                        pane = Some(
                            iter.next()
                                .ok_or_else(|| anyhow!("--pane requires a value"))?,
                        );
                    }
                    other => bail!("unknown argument for visit: {other}"),
                }
            }
            Ok(Command::Visit { pane })
        }
        "jump" => {
            let mut pane = None;
            let mut visit = false;
            let mut dismiss = false;
            while let Some(arg) = iter.next() {
                match arg.as_str() {
                    "--pane" => {
                        pane = Some(
                            iter.next()
                                .ok_or_else(|| anyhow!("--pane requires a value"))?,
                        );
                    }
                    "--visit" => visit = true,
                    "--dismiss" => dismiss = true,
                    other => bail!("unknown argument for jump: {other}"),
                }
            }
            if visit && dismiss {
                bail!("--visit and --dismiss are mutually exclusive");
            }
            let after = if visit {
                JumpAfter::Visit
            } else if dismiss {
                JumpAfter::Dismiss
            } else {
                JumpAfter::None
            };
            Ok(Command::Jump { pane, after })
        }
        "list" => {
            let mut state = ListState::Waiting;
            let mut json = false;
            while let Some(arg) = iter.next() {
                match arg.as_str() {
                    "--state" => {
                        let val = iter
                            .next()
                            .ok_or_else(|| anyhow!("--state requires a value"))?;
                        state = match val.as_str() {
                            "waiting" => ListState::Waiting,
                            "history" => ListState::History,
                            other => bail!("invalid --state '{other}' (expected waiting or history)"),
                        };
                    }
                    "--json" => json = true,
                    other => bail!("unknown argument for list: {other}"),
                }
            }
            Ok(Command::List { state, json })
        }
        "watch" => {
            if let Some(extra) = iter.next() {
                bail!("unknown argument for watch: {extra}");
            }
            Ok(Command::Watch)
        }
        "hook" => {
            let event_str = iter
                .next()
                .ok_or_else(|| anyhow!("hook requires an event: stop, notification, or prompt-submit"))?;
            let event = HookEvent::from_str(&event_str)?;
            if let Some(extra) = iter.next() {
                bail!("unknown argument for hook: {extra}");
            }
            Ok(Command::Hook { event })
        }
        other => bail!("unknown subcommand: {other}"),
    }
}

fn resolve_pane_id(explicit: Option<&str>) -> Option<String> {
    let candidate = explicit
        .map(ToOwned::to_owned)
        .or_else(|| env::var("TMUX_PANE").ok())?;
    let trimmed = candidate.trim().to_string();
    if is_valid_pane_id(&trimmed) {
        Some(trimmed)
    } else {
        None
    }
}

/// Truncates `text` to its first non-empty line, capped at `MAX_SUMMARY_CHARS` (200) Unicode chars.
pub fn sanitize_summary(text: &str) -> Option<String> {
    let first_line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())?;
    let truncated: String = first_line.chars().take(MAX_SUMMARY_CHARS).collect();
    Some(truncated)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchCounts {
    pub waiting: usize,
    pub error: usize,
    pub visited: usize,
    pub running: usize,
    pub history: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchSnapshot {
    pub waiting: Vec<QueueEntry>,
    pub visited: Vec<QueueEntry>,
    pub running: Vec<QueueEntry>,
    pub history: Vec<QueueEntry>,
    pub counts: WatchCounts,
}

pub fn build_watch_snapshot(dir: &Path) -> Result<WatchSnapshot> {
    let waiting_all = read_queue_dir(dir)?;
    let history_all = read_history(dir)?;

    let mut waiting = Vec::new();
    let mut visited = Vec::new();
    let mut error_count = 0;

    for entry in waiting_all {
        if entry.stop_reason == StopReason::Visited {
            visited.push(entry);
        } else {
            if entry.stop_reason == StopReason::Error {
                error_count += 1;
            }
            waiting.push(entry);
        }
    }

    let mut running = Vec::new();
    let mut history = Vec::new();
    for entry in history_all {
        if entry.stop_reason == StopReason::Running {
            running.push(entry);
        } else {
            history.push(entry);
        }
    }

    let counts = WatchCounts {
        waiting: waiting.len(),
        error: error_count,
        visited: visited.len(),
        running: running.len(),
        history: history.len(),
    };

    Ok(WatchSnapshot {
        waiting,
        visited,
        running,
        history,
        counts,
    })
}

pub fn format_list_output(entries: &[QueueEntry], json: bool) -> Result<String> {
    if json {
        Ok(serde_json::to_string_pretty(entries)?)
    } else {
        let mut out = String::new();
        for e in entries {
            out.push_str(&format!(
                "{}\t{}\t{}\t{}\n",
                e.pane_id,
                e.tmux_target(),
                e.stop_reason.as_str(),
                e.summary
            ));
        }
        Ok(out)
    }
}

pub fn run_command(cmd: Command) -> Result<()> {
    let dir = queue_dir();
    match cmd {
        Command::Tui => Ok(()),
        Command::Push {
            pane,
            stop_reason,
            summary,
            cwd,
        } => {
            let Some(pane_id) = resolve_pane_id(pane.as_deref()) else {
                return Ok(());
            };
            let Ok(pane_info) = get_pane_info(&pane_id) else {
                return Ok(());
            };
            let resolved_summary = summary
                .as_deref()
                .and_then(sanitize_summary)
                .unwrap_or_default();
            let resolved_cwd = cwd.unwrap_or_else(|| {
                env::current_dir()
                    .ok()
                    .and_then(|p| p.to_str().map(ToOwned::to_owned))
                    .unwrap_or_default()
            });
            let entry = QueueEntry {
                pane_id,
                session_name: pane_info.session_name,
                window_index: pane_info.window_index,
                window_name: pane_info.window_name,
                stop_reason,
                summary: resolved_summary,
                timestamp: Utc::now(),
                cwd: resolved_cwd,
            };
            upsert_entry_in(&dir, &entry)?;
            Ok(())
        }
        Command::Clear {
            pane,
            mode,
            summary,
            cwd,
        } => {
            let Some(pane_id) = resolve_pane_id(pane.as_deref()) else {
                return Ok(());
            };
            match mode {
                ClearMode::ToHistory => {
                    clear_entry_in(&dir, &pane_id, true)?;
                }
                ClearMode::NoHistory => {
                    clear_entry_in(&dir, &pane_id, false)?;
                }
                ClearMode::Running => {
                    let pane_info = get_pane_info(&pane_id).ok();
                    let clean_summary = summary.as_deref().and_then(sanitize_summary);
                    mark_running_in(
                        &dir,
                        &pane_id,
                        pane_info.as_ref(),
                        clean_summary.as_deref(),
                        cwd.as_deref(),
                    )?;
                }
            }
            Ok(())
        }
        Command::Visit { pane } => {
            let Some(pane_id) = resolve_pane_id(pane.as_deref()) else {
                return Ok(());
            };
            mark_visited_in(&dir, &pane_id)?;
            Ok(())
        }
        Command::Jump { pane, after } => {
            let waiting = read_queue_dir(&dir)?;
            let history = read_history(&dir)?;

            let target_pane = match pane.as_deref() {
                Some(p) if is_valid_pane_id(p) => p.to_string(),
                Some(p) => bail!("invalid pane_id: {p}"),
                None => waiting
                    .first()
                    .map(|e| e.pane_id.clone())
                    .or_else(|| resolve_pane_id(None))
                    .ok_or_else(|| anyhow!("no pane specified and waiting queue is empty"))?,
            };

            let entry = waiting
                .into_iter()
                .chain(history)
                .find(|e| e.pane_id == target_pane)
                .or_else(|| {
                    get_pane_info(&target_pane).ok().map(|info| QueueEntry {
                        pane_id: target_pane.clone(),
                        session_name: info.session_name,
                        window_index: info.window_index,
                        window_name: info.window_name,
                        stop_reason: StopReason::NeedsInput,
                        summary: String::new(),
                        timestamp: Utc::now(),
                        cwd: String::new(),
                    })
                })
                .ok_or_else(|| anyhow!("pane {target_pane} not found"))?;

            let jump_res = jump_to_pane(&entry)?;

            match after {
                JumpAfter::None => {}
                JumpAfter::Visit => {
                    mark_visited_in(&dir, &entry.pane_id)?;
                }
                JumpAfter::Dismiss => {
                    clear_entry_in(&dir, &entry.pane_id, true)?;
                }
            }

            let out = serde_json::json!({
                "ok": true,
                "pane_id": entry.pane_id,
                "client_pid": jump_res.client_pid,
                "terminal_pid": jump_res.terminal_pid,
            });
            println!("{}", serde_json::to_string(&out)?);
            Ok(())
        }
        Command::List { state, json } => {
            let entries = match state {
                ListState::Waiting => read_queue_dir(&dir)?,
                ListState::History => read_history(&dir)?,
            };
            let output = format_list_output(&entries, json)?;
            print!("{output}");
            Ok(())
        }
        Command::Watch => run_watch(&dir),
        Command::Hook { event } => {
            let mut input = String::new();
            let _ = io::stdin().read_to_string(&mut input);
            // Hooks must never fail or block Claude Code
            let _ = handle_hook_event(&dir, event, &input);
            Ok(())
        }
    }
}

fn run_watch(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir)?;

    let (tx, rx) = mpsc::channel();
    let mut watcher = RecommendedWatcher::new(
        move |res| {
            let _ = tx.send(res);
        },
        Config::default(),
    )?;
    watcher.watch(dir, RecursiveMode::NonRecursive)?;

    let mut last_json: Option<String> = None;

    loop {
        let _ = prune_stale_panes_in(dir);
        let _ = prune_stale_running_panes_in(dir);

        if let Ok(snapshot) = build_watch_snapshot(dir) {
            if let Ok(json) = serde_json::to_string(&snapshot) {
                if last_json.as_deref() != Some(json.as_str()) {
                    println!("{json}");
                    let _ = io::stdout().flush();
                    last_json = Some(json);
                }
            }
        }

        match rx.recv_timeout(Duration::from_secs(3)) {
            Ok(_) => {
                // Drain any queued notifications that arrived together
                while rx.try_recv().is_ok() {}
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    Ok(())
}

// --- Claude Code Hook Handling ---

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookAction {
    Ignore,
    Push {
        stop_reason: StopReason,
        summary: String,
        cwd: Option<String>,
    },
    ClearRunning {
        summary: Option<String>,
        cwd: Option<String>,
    },
}

fn is_stop_hook_summary_line(val: &Value) -> bool {
    val.get("type").and_then(Value::as_str) == Some("stop_hook_summary")
        || val.get("subtype").and_then(Value::as_str) == Some("stop_hook_summary")
}

fn is_api_error_line(val: &Value) -> bool {
    val.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true)
        || val.get("subtype").and_then(Value::as_str) == Some("api_error")
}

fn is_429_rate_limit_line(val: &Value) -> bool {
    let is_api_err_msg = val.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true);
    if is_api_err_msg {
        let serialized = val.to_string();
        if serialized.contains("429") {
            return true;
        }
    }
    if val.get("subtype").and_then(Value::as_str) == Some("api_error") {
        let status = val
            .get("error")
            .and_then(|e| e.get("status"))
            .and_then(Value::as_u64)
            .or_else(|| val.get("status").and_then(Value::as_u64));
        if status == Some(429) {
            return true;
        }
    }
    false
}

fn extract_text_from_content(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => sanitize_summary(s),
        Value::Array(items) => {
            for item in items {
                if let Some(s) = item.as_str() {
                    if let Some(clean) = sanitize_summary(s) {
                        return Some(clean);
                    }
                }
                let item_type = item.get("type").and_then(Value::as_str);
                if item_type == Some("text") || item_type.is_none() {
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        if let Some(clean) = sanitize_summary(text) {
                            return Some(clean);
                        }
                    }
                }
            }
            None
        }
        _ => None,
    }
}

fn extract_first_user_prompt(lines: &[Value]) -> Option<String> {
    for val in lines {
        let is_user = val.get("type").and_then(Value::as_str) == Some("user")
            || val
                .get("message")
                .and_then(|m| m.get("role"))
                .and_then(Value::as_str)
                == Some("user")
            || val.get("role").and_then(Value::as_str) == Some("user");
        if !is_user {
            continue;
        }
        if val.get("isMeta").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        if let Some(content) = val.get("message").and_then(|m| m.get("content")) {
            if let Some(text) = extract_text_from_content(content) {
                return Some(text);
            }
        }
        if let Some(content) = val.get("content") {
            if let Some(text) = extract_text_from_content(content) {
                return Some(text);
            }
        }
    }
    None
}

fn parse_transcript_lines(path: &Path) -> Result<Vec<Value>> {
    let raw = fs::read_to_string(path)?;
    let mut parsed = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
            if !is_stop_hook_summary_line(&val) {
                parsed.push(val);
            }
        }
    }
    Ok(parsed)
}

/// Evaluates a Claude Code hook event and JSON payload into a deterministic `HookAction`.
pub fn evaluate_hook(event: HookEvent, stdin_json: &str) -> HookAction {
    let Ok(payload) = serde_json::from_str::<Value>(stdin_json) else {
        return HookAction::Ignore;
    };

    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);

    match event {
        HookEvent::Stop => {
            let Some(transcript_path_str) =
                payload.get("transcript_path").and_then(Value::as_str)
            else {
                return HookAction::Ignore;
            };
            if transcript_path_str.trim().is_empty() {
                return HookAction::Ignore;
            }
            let Ok(lines) = parse_transcript_lines(Path::new(transcript_path_str)) else {
                return HookAction::Ignore;
            };

            if let Some(last) = lines.last() {
                if is_429_rate_limit_line(last) {
                    return HookAction::Ignore;
                }
            }

            let stop_reason = match lines.last() {
                Some(last) if is_api_error_line(last) => StopReason::Error,
                _ => StopReason::NeedsInput,
            };

            let summary = payload
                .get("last_assistant_message")
                .and_then(Value::as_str)
                .and_then(sanitize_summary)
                .or_else(|| extract_first_user_prompt(&lines))
                .unwrap_or_else(|| "Session complete".to_string());

            HookAction::Push {
                stop_reason,
                summary,
                cwd,
            }
        }
        HookEvent::Notification => {
            let Some(notif_type) = payload.get("notification_type").and_then(Value::as_str) else {
                return HookAction::Ignore;
            };
            if !matches!(
                notif_type,
                "idle_prompt" | "permission_prompt" | "elicitation_dialog"
            ) {
                return HookAction::Ignore;
            }

            let summary = payload
                .get("message")
                .and_then(Value::as_str)
                .and_then(sanitize_summary)
                .or_else(|| {
                    let tp = payload.get("transcript_path").and_then(Value::as_str)?;
                    let lines = parse_transcript_lines(Path::new(tp)).ok()?;
                    extract_first_user_prompt(&lines)
                })
                .unwrap_or_else(|| "Waiting for input".to_string());

            HookAction::Push {
                stop_reason: StopReason::NeedsInput,
                summary,
                cwd,
            }
        }
        HookEvent::PromptSubmit => {
            let summary = payload
                .get("prompt")
                .and_then(Value::as_str)
                .and_then(sanitize_summary);
            HookAction::ClearRunning { summary, cwd }
        }
    }
}

pub fn handle_hook_event(dir: &Path, event: HookEvent, stdin_json: &str) -> Result<()> {
    let action = evaluate_hook(event, stdin_json);
    let Some(pane_id) = resolve_pane_id(None) else {
        return Ok(());
    };

    match action {
        HookAction::Ignore => Ok(()),
        HookAction::Push {
            stop_reason,
            summary,
            cwd,
        } => {
            if !pane_exists(&pane_id) {
                return Ok(());
            }
            let Ok(pane_info) = get_pane_info(&pane_id) else {
                return Ok(());
            };
            let entry = QueueEntry {
                pane_id,
                session_name: pane_info.session_name,
                window_index: pane_info.window_index,
                window_name: pane_info.window_name,
                stop_reason,
                summary,
                timestamp: Utc::now(),
                cwd: cwd.unwrap_or_default(),
            };
            upsert_entry_in(dir, &entry)?;
            Ok(())
        }
        HookAction::ClearRunning { summary, cwd } => {
            let pane_info = get_pane_info(&pane_id).ok();
            mark_running_in(
                dir,
                &pane_id,
                pane_info.as_ref(),
                summary.as_deref(),
                cwd.as_deref(),
            )?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_parse_cli_args() {
        assert_eq!(parse_args(Vec::<&str>::new()).unwrap(), Command::Tui);

        assert_eq!(
            parse_args(["push", "--pane", "%3", "--stop-reason", "error", "--summary", "boom", "--cwd", "/tmp"]).unwrap(),
            Command::Push {
                pane: Some("%3".to_string()),
                stop_reason: StopReason::Error,
                summary: Some("boom".to_string()),
                cwd: Some("/tmp".to_string()),
            }
        );

        assert_eq!(
            parse_args(["clear", "--pane", "%1", "--running", "--summary", "coding"]).unwrap(),
            Command::Clear {
                pane: Some("%1".to_string()),
                mode: ClearMode::Running,
                summary: Some("coding".to_string()),
                cwd: None,
            }
        );

        assert_eq!(
            parse_args(["clear", "--no-history"]).unwrap(),
            Command::Clear {
                pane: None,
                mode: ClearMode::NoHistory,
                summary: None,
                cwd: None,
            }
        );

        assert_eq!(
            parse_args(["jump", "--pane", "%2", "--visit"]).unwrap(),
            Command::Jump {
                pane: Some("%2".to_string()),
                after: JumpAfter::Visit,
            }
        );

        assert_eq!(
            parse_args(["list", "--state", "history", "--json"]).unwrap(),
            Command::List {
                state: ListState::History,
                json: true,
            }
        );

        assert_eq!(
            parse_args(["hook", "prompt-submit"]).unwrap(),
            Command::Hook {
                event: HookEvent::PromptSubmit,
            }
        );
    }

    #[test]
    fn test_hook_stop_normal_and_fallbacks() {
        let tmp = TempDir::new().unwrap();
        let transcript = tmp.path().join("transcript.jsonl");
        fs::write(
            &transcript,
            concat!(
                r#"{"type":"user","message":{"role":"user","content":"Fix the login bug"}}"#,
                "\n",
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"All done"}]}}"#,
                "\n",
                r#"{"type":"stop_hook_summary","summary":"ignored"}"#,
                "\n"
            ),
        )
        .unwrap();

        // With last_assistant_message present
        let payload = serde_json::json!({
            "transcript_path": transcript.to_str().unwrap(),
            "last_assistant_message": "\n  Fixed the login bug in auth.rs\nSecond line",
            "cwd": "/work/app"
        });
        assert_eq!(
            evaluate_hook(HookEvent::Stop, &payload.to_string()),
            HookAction::Push {
                stop_reason: StopReason::NeedsInput,
                summary: "Fixed the login bug in auth.rs".to_string(),
                cwd: Some("/work/app".to_string()),
            }
        );

        // Without last_assistant_message -> falls back to first user prompt in transcript
        let payload_no_asst = serde_json::json!({
            "transcript_path": transcript.to_str().unwrap(),
            "cwd": "/work/app"
        });
        assert_eq!(
            evaluate_hook(HookEvent::Stop, &payload_no_asst.to_string()),
            HookAction::Push {
                stop_reason: StopReason::NeedsInput,
                summary: "Fix the login bug".to_string(),
                cwd: Some("/work/app".to_string()),
            }
        );
    }

    #[test]
    fn test_hook_stop_429_ignored_and_api_error_flagged() {
        let tmp = TempDir::new().unwrap();
        let t_429_a = tmp.path().join("429_a.jsonl");
        fs::write(
            &t_429_a,
            r#"{"type":"assistant","isApiErrorMessage":true,"message":{"content":"API Error: 429 rate limit"}}"#,
        )
        .unwrap();
        let payload_429_a = serde_json::json!({
            "transcript_path": t_429_a.to_str().unwrap()
        });
        assert_eq!(
            evaluate_hook(HookEvent::Stop, &payload_429_a.to_string()),
            HookAction::Ignore
        );

        let t_429_b = tmp.path().join("429_b.jsonl");
        fs::write(
            &t_429_b,
            r#"{"type":"system","subtype":"api_error","error":{"status":429}}"#,
        )
        .unwrap();
        let payload_429_b = serde_json::json!({
            "transcript_path": t_429_b.to_str().unwrap()
        });
        assert_eq!(
            evaluate_hook(HookEvent::Stop, &payload_429_b.to_string()),
            HookAction::Ignore
        );

        // Non-429 API error -> StopReason::Error
        let t_500 = tmp.path().join("500.jsonl");
        fs::write(
            &t_500,
            r#"{"type":"system","subtype":"api_error","error":{"status":500}}"#,
        )
        .unwrap();
        let payload_500 = serde_json::json!({
            "transcript_path": t_500.to_str().unwrap(),
            "last_assistant_message": "Internal server error"
        });
        assert_eq!(
            evaluate_hook(HookEvent::Stop, &payload_500.to_string()),
            HookAction::Push {
                stop_reason: StopReason::Error,
                summary: "Internal server error".to_string(),
                cwd: None,
            }
        );
    }

    #[test]
    fn test_hook_notification_and_prompt_submit() {
        // Ignored notification_type
        let ignored = serde_json::json!({
            "notification_type": "other_event",
            "message": "hello"
        });
        assert_eq!(
            evaluate_hook(HookEvent::Notification, &ignored.to_string()),
            HookAction::Ignore
        );

        // Allowed notification_type
        let perm = serde_json::json!({
            "notification_type": "permission_prompt",
            "message": "Allow bash command?\nDetails...",
            "cwd": "/repo"
        });
        assert_eq!(
            evaluate_hook(HookEvent::Notification, &perm.to_string()),
            HookAction::Push {
                stop_reason: StopReason::NeedsInput,
                summary: "Allow bash command?".to_string(),
                cwd: Some("/repo".to_string()),
            }
        );

        // Prompt-submit
        let submit = serde_json::json!({
            "prompt": "Implement phase 3\nwith tests",
            "cwd": "/repo"
        });
        assert_eq!(
            evaluate_hook(HookEvent::PromptSubmit, &submit.to_string()),
            HookAction::ClearRunning {
                summary: Some("Implement phase 3".to_string()),
                cwd: Some("/repo".to_string()),
            }
        );
    }
}
