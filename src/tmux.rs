use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

use crate::queue::{
    clear_entry_in, is_valid_pane_id, queue_dir, read_history, read_queue_dir, QueueEntry,
    StopReason,
};

const TERMINAL_EMULATORS: &[&str] = &["alacritty", "kitty", "foot", "wezterm-gui", "ghostty"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneInfo {
    pub session_name: String,
    pub window_index: u32,
    pub window_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxClient {
    pub activity: u64,
    pub tty: String,
    pub pid: u32,
    pub session_name: String,
    pub pane_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JumpResult {
    pub client_pid: Option<u32>,
    pub terminal_pid: Option<u32>,
}

/// Parses output of `tmux display-message -p -t <pane_id> "#{session_name}\t#{window_index}\t#{window_name}"`.
pub fn parse_pane_info(output: &str) -> Result<PaneInfo> {
    let line = output.lines().next().unwrap_or("").trim_end_matches('\r');
    let mut parts = line.splitn(3, '\t');
    let session_name = parts
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("missing session_name in tmux pane info"))?
        .to_string();
    let window_index_str = parts
        .next()
        .ok_or_else(|| anyhow!("missing window_index in tmux pane info"))?;
    let window_index: u32 = window_index_str
        .trim()
        .parse()
        .with_context(|| format!("invalid window_index: {window_index_str}"))?;
    let window_name = parts.next().unwrap_or("").to_string();

    Ok(PaneInfo {
        session_name,
        window_index,
        window_name,
    })
}

/// Queries tmux for `<pane_id>` metadata: `session_name`, `window_index`, `window_name`.
pub fn get_pane_info(pane_id: &str) -> Result<PaneInfo> {
    if !is_valid_pane_id(pane_id) {
        return Err(anyhow!("invalid pane_id: {pane_id}"));
    }
    let output = Command::new("tmux")
        .args([
            "display-message",
            "-p",
            "-t",
            pane_id,
            "#{session_name}\t#{window_index}\t#{window_name}",
        ])
        .output()
        .context("failed to execute tmux display-message")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "tmux display-message failed for {pane_id}: {}",
            stderr.trim()
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_pane_info(&stdout)
}

/// Checks if `<pane_id>` exists in tmux via `tmux display-message -t <pane_id> -p ""`.
pub fn pane_exists(pane_id: &str) -> bool {
    if !is_valid_pane_id(pane_id) {
        return false;
    }
    Command::new("tmux")
        .args(["display-message", "-t", pane_id, "-p", ""])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Removes dead panes from the waiting queue (`clear_entry(..., false)`).
pub fn prune_stale_panes() -> Result<()> {
    prune_stale_panes_in(&queue_dir())
}

pub fn prune_stale_panes_in(dir: &Path) -> Result<()> {
    prune_stale_panes_with(dir, pane_exists)
}

pub fn prune_stale_panes_with<F>(dir: &Path, mut exists_fn: F) -> Result<()>
where
    F: FnMut(&str) -> bool,
{
    let entries = read_queue_dir(dir)?;
    for entry in entries {
        if !exists_fn(&entry.pane_id) {
            let _ = clear_entry_in(dir, &entry.pane_id, false);
        }
    }
    Ok(())
}

/// Removes dead panes from history only when `stop_reason == StopReason::Running`.
/// Completed/idle history entries persist even after panes close.
pub fn prune_stale_running_panes() -> Result<()> {
    prune_stale_running_panes_in(&queue_dir())
}

pub fn prune_stale_running_panes_in(dir: &Path) -> Result<()> {
    prune_stale_running_panes_with(dir, pane_exists)
}

pub fn prune_stale_running_panes_with<F>(dir: &Path, mut exists_fn: F) -> Result<()>
where
    F: FnMut(&str) -> bool,
{
    let history = read_history(dir)?;
    for entry in history {
        if entry.stop_reason == StopReason::Running && !exists_fn(&entry.pane_id) {
            let _ = clear_entry_in(dir, &entry.pane_id, false);
        }
    }
    Ok(())
}

/// Parses output of `tmux list-clients -F "#{client_activity}\t#{client_tty}\t#{client_pid}\t#{session_name}\t#{pane_id}"`.
pub fn parse_clients(output: &str) -> Vec<TmuxClient> {
    let mut clients = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 5 {
            continue;
        }
        let activity = parts[0].parse::<u64>().unwrap_or(0);
        let tty = parts[1].to_string();
        let pid = match parts[2].parse::<u32>() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let session_name = parts[3].to_string();
        let pane_id = parts[4].to_string();
        clients.push(TmuxClient {
            activity,
            tty,
            pid,
            session_name,
            pane_id,
        });
    }
    clients
}

/// Scores and selects the best tmux client:
/// - Client whose active pane matches `current_tmux_pane` (`$TMUX_PANE`): `u64::MAX`
/// - Client whose active pane matches `target_pane`: `u64::MAX - 1`
/// - Otherwise: `client.activity`
pub fn select_best_client<'a>(
    clients: &'a [TmuxClient],
    current_tmux_pane: Option<&str>,
    target_pane: &str,
) -> Option<&'a TmuxClient> {
    clients.iter().max_by_key(|c| {
        if current_tmux_pane.is_some_and(|cur| !cur.is_empty() && c.pane_id == cur) {
            u64::MAX
        } else if c.pane_id == target_pane {
            u64::MAX - 1
        } else {
            c.activity
        }
    })
}

fn get_session_group(session_name: &str) -> Option<String> {
    let output = Command::new("tmux")
        .args(["display-message", "-t", session_name, "-p", "#{session_group}"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let group = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if group.is_empty() {
        None
    } else {
        Some(group)
    }
}

pub fn sessions_in_same_group(
    session_a: &str,
    group_a: Option<&str>,
    session_b: &str,
    group_b: Option<&str>,
) -> bool {
    if session_a == session_b {
        return true;
    }
    match (group_a, group_b) {
        (Some(ga), Some(gb)) => !ga.is_empty() && ga == gb,
        _ => false,
    }
}

/// Parses `/proc/<pid>/stat` content into `(comm, ppid)`.
pub fn parse_proc_stat(stat_content: &str) -> Option<(String, u32)> {
    let open = stat_content.find('(')?;
    let close = stat_content.rfind(')')?;
    if close <= open {
        return None;
    }
    let comm = stat_content[open + 1..close].to_string();
    let rest = stat_content.get(close + 1..)?.trim_start();
    // Fields after `(comm)`: state (0), ppid (1), ...
    let mut fields = rest.split_whitespace();
    let _state = fields.next()?;
    let ppid_str = fields.next()?;
    let ppid = ppid_str.parse::<u32>().ok()?;
    Some((comm, ppid))
}

/// Strips leading `.` and trailing `-wrapped` from a process name.
pub fn normalize_proc_name(name: &str) -> &str {
    let s = name.strip_prefix('.').unwrap_or(name);
    s.strip_suffix("-wrapped").unwrap_or(s)
}

pub fn is_terminal_emulator(name: &str) -> bool {
    let norm = normalize_proc_name(name);
    TERMINAL_EMULATORS.contains(&norm)
}

/// Walks `/proc/<pid>/stat` parent PIDs up to depth 8 starting from `client_pid`
/// to find a known terminal emulator PID.
pub fn resolve_terminal_pid(client_pid: u32) -> Option<u32> {
    resolve_terminal_pid_in(Path::new("/proc"), client_pid)
}

pub fn resolve_terminal_pid_in(proc_root: &Path, client_pid: u32) -> Option<u32> {
    let mut curr_pid = client_pid;
    for _ in 0..8 {
        if curr_pid <= 1 {
            break;
        }
        let stat_path = proc_root.join(curr_pid.to_string()).join("stat");
        let Ok(content) = fs::read_to_string(&stat_path) else {
            break;
        };
        let Some((comm, ppid)) = parse_proc_stat(&content) else {
            break;
        };
        if is_terminal_emulator(&comm) {
            return Some(curr_pid);
        }
        // Also check `/proc/<pid>/exe` symlink basename in case comm was truncated at 15 chars
        let exe_path = proc_root.join(curr_pid.to_string()).join("exe");
        if let Ok(target) = fs::read_link(&exe_path) {
            if let Some(exe_name) = target.file_name().and_then(|n| n.to_str()) {
                if is_terminal_emulator(exe_name) {
                    return Some(curr_pid);
                }
            }
        }
        if ppid <= 1 || ppid == curr_pid {
            break;
        }
        curr_pid = ppid;
    }
    None
}

/// Selects the best tmux client, switches session if needed (respecting session groups),
/// selects the target window and pane, and resolves the client & terminal PIDs.
pub fn jump_to_pane(entry: &QueueEntry) -> Result<JumpResult> {
    let list_out = Command::new("tmux")
        .args([
            "list-clients",
            "-F",
            "#{client_activity}\t#{client_tty}\t#{client_pid}\t#{session_name}\t#{pane_id}",
        ])
        .output()
        .context("failed to run tmux list-clients")?;

    let clients = if list_out.status.success() {
        parse_clients(&String::from_utf8_lossy(&list_out.stdout))
    } else {
        Vec::new()
    };

    let current_tmux_pane = env::var("TMUX_PANE").ok();
    let selected_client =
        select_best_client(&clients, current_tmux_pane.as_deref(), &entry.pane_id);

    let mut target_session = entry.session_name.clone();
    let mut client_pid = None;
    let mut terminal_pid = None;

    if let Some(client) = selected_client {
        client_pid = Some(client.pid);
        terminal_pid = resolve_terminal_pid(client.pid);

        let active_group = get_session_group(&client.session_name);
        let entry_group = get_session_group(&entry.session_name);

        if sessions_in_same_group(
            &client.session_name,
            active_group.as_deref(),
            &entry.session_name,
            entry_group.as_deref(),
        ) {
            target_session = client.session_name.clone();
        } else {
            let status = Command::new("tmux")
                .args([
                    "switch-client",
                    "-c",
                    &client.tty,
                    "-t",
                    &entry.session_name,
                ])
                .status()
                .context("failed to run tmux switch-client")?;
            if !status.success() {
                return Err(anyhow!(
                    "tmux switch-client failed for tty {} -> session {}",
                    client.tty,
                    entry.session_name
                ));
            }
        }
    }

    let window_target = format!("{}:{}", target_session, entry.window_index);
    let win_status = Command::new("tmux")
        .args(["select-window", "-t", &window_target])
        .status()
        .context("failed to run tmux select-window")?;
    if !win_status.success() {
        return Err(anyhow!("tmux select-window failed for {window_target}"));
    }

    let pane_status = Command::new("tmux")
        .args(["select-pane", "-t", &entry.pane_id])
        .status()
        .context("failed to run tmux select-pane")?;
    if !pane_status.success() {
        return Err(anyhow!("tmux select-pane failed for {}", entry.pane_id));
    }

    Ok(JumpResult {
        client_pid,
        terminal_pid,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::upsert_entry_in;
    use chrono::{TimeZone, Utc};
    use tempfile::TempDir;

    #[test]
    fn test_parse_pane_info() {
        let info = parse_pane_info("dev\t3\teditor\n").unwrap();
        assert_eq!(
            info,
            PaneInfo {
                session_name: "dev".to_string(),
                window_index: 3,
                window_name: "editor".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_clients_and_selection_scoring() {
        let raw = "\
1700000100\t/dev/pts/1\t1001\tmain\t%1
1700000500\t/dev/pts/2\t1002\twork\t%2
1700000200\t/dev/pts/3\t1003\tchat\t%3
";
        let clients = parse_clients(raw);
        assert_eq!(clients.len(), 3);

        // 1. $TMUX_PANE match wins (score u64::MAX) even with lower activity
        let picked = select_best_client(&clients, Some("%1"), "%3").unwrap();
        assert_eq!(picked.tty, "/dev/pts/1");

        // 2. target_pane match wins (score u64::MAX - 1) when $TMUX_PANE doesn't match
        let picked = select_best_client(&clients, Some("%99"), "%3").unwrap();
        assert_eq!(picked.tty, "/dev/pts/3");

        // 3. Highest client_activity wins when neither matches
        let picked = select_best_client(&clients, None, "%99").unwrap();
        assert_eq!(picked.tty, "/dev/pts/2");
    }

    #[test]
    fn test_sessions_in_same_group() {
        assert!(sessions_in_same_group("main", None, "main", None));
        assert!(sessions_in_same_group(
            "main-1",
            Some("grp0"),
            "main-2",
            Some("grp0")
        ));
        assert!(!sessions_in_same_group(
            "main-1",
            Some("grp0"),
            "other",
            Some("grp1")
        ));
        assert!(!sessions_in_same_group("main-1", None, "other", None));
    }

    #[test]
    fn test_parse_proc_stat_and_normalize_name() {
        // Standard comm
        let stat = "1234 (alacritty) S 1000 1234 1234 0 -1 4194304 0 0 0 0";
        assert_eq!(
            parse_proc_stat(stat),
            Some(("alacritty".to_string(), 1000))
        );

        // Comm with spaces and parens
        let stat_weird = "2345 (tmux: client (1)) S 2000 2345 2345 0 -1";
        assert_eq!(
            parse_proc_stat(stat_weird),
            Some(("tmux: client (1)".to_string(), 2000))
        );

        assert!(is_terminal_emulator("alacritty"));
        assert!(is_terminal_emulator(".alacritty-wrapped"));
        assert!(is_terminal_emulator(".kitty-wrapped"));
        assert!(is_terminal_emulator("foot"));
        assert!(is_terminal_emulator("wezterm-gui"));
        assert!(is_terminal_emulator(".ghostty-wrapped"));
        assert!(!is_terminal_emulator("bash"));
    }

    #[test]
    fn test_resolve_terminal_pid_walk() {
        let tmp = TempDir::new().unwrap();
        let proc_root = tmp.path();

        // PID 500 (tmux client) -> PPID 400 (zsh) -> PPID 300 (.ghostty-wrapped) -> PPID 1
        fs::create_dir_all(proc_root.join("500")).unwrap();
        fs::write(
            proc_root.join("500/stat"),
            "500 (tmux: client) S 400 500 500 0 -1",
        )
        .unwrap();

        fs::create_dir_all(proc_root.join("400")).unwrap();
        fs::write(proc_root.join("400/stat"), "400 (zsh) S 300 400 400 0 -1").unwrap();

        fs::create_dir_all(proc_root.join("300")).unwrap();
        fs::write(
            proc_root.join("300/stat"),
            "300 (.ghostty-wrapped) S 1 300 300 0 -1",
        )
        .unwrap();

        assert_eq!(resolve_terminal_pid_in(proc_root, 500), Some(300));
        assert_eq!(resolve_terminal_pid_in(proc_root, 999), None);
    }

    #[test]
    fn test_prune_stale_panes_and_running_panes() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        let mk = |pane: &str, reason: StopReason| QueueEntry {
            pane_id: pane.to_string(),
            session_name: "s".to_string(),
            window_index: 0,
            window_name: "w".to_string(),
            stop_reason: reason,
            summary: "sum".to_string(),
            timestamp: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            cwd: "/tmp".to_string(),
        };

        // Waiting: %1 (alive), %2 (dead)
        upsert_entry_in(dir, &mk("%1", StopReason::NeedsInput)).unwrap();
        upsert_entry_in(dir, &mk("%2", StopReason::Error)).unwrap();

        // History: %3 (running, dead), %4 (idle/completed in history, dead)
        upsert_entry_in(dir, &mk("%3", StopReason::Running)).unwrap();
        upsert_entry_in(dir, &mk("%4", StopReason::NeedsInput)).unwrap();
        clear_entry_in(dir, "%4", true).unwrap();

        // Prune waiting panes where only %1 exists
        prune_stale_panes_with(dir, |p| p == "%1").unwrap();
        let waiting = read_queue_dir(dir).unwrap();
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].pane_id, "%1");

        // Prune stale running panes where neither %3 nor %4 exists:
        // %3 (Running) is removed, %4 (NeedsInput in history) is preserved!
        prune_stale_running_panes_with(dir, |_| false).unwrap();
        let history = read_history(dir).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].pane_id, "%4");
        assert_eq!(history[0].stop_reason, StopReason::NeedsInput);
    }
}
