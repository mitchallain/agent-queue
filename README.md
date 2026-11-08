# agent-queue

`agent-queue` is a fast Rust TUI and CLI that tracks `tmux`-hosted AI coding agent sessions (**Claude Code** and **Pi**) across two views—**Waiting** (needs human attention) and **History** (actively running or recently dismissed)—and lets you jump directly to the target `tmux` pane.

## Features

- **Interactive TUI (`ratatui`)**:
  - **Waiting** tab: orders unvisited entries first (FIFO), then visited entries.
  - **History** tab: shows actively `running` sessions at the top, followed by recently completed/dismissed sessions.
  - **Instant search (`/`)**: case-insensitive live filtering with match highlighting across session/window target, summary, and working directory.
  - **Live reload**: watches the SQLite WAL database via `inotify` (50ms debounce) and automatically prunes closed `tmux` panes.
- **Session-Group-Aware `tmux` Jumping**:
  - Selects the best `tmux` client (`$TMUX_PANE` match, target pane match, or most recently active client), switches sessions only when outside the current `#{session_group}`, and focuses the target window and pane.
  - Resolves the parent terminal emulator PID (`alacritty`, `kitty`, `foot`, `wezterm-gui`, `ghostty`) via `/proc/<pid>/stat`.
- **Agent Integrations**:
  - **Pi Coding Agent (`pi/extension.ts`)**: tracks prompt execution (`running`), turn completion (`needs_input` / `error`), mid-turn confirmation/input dialogs, and session shutdown.
  - **Claude Code (`plugin/hooks/`)**: hooks for `Stop`, `Notification`, and `UserPromptSubmit`.

## Installation

### Cargo

```bash
cargo install --path .
```

### Nix Flake

```bash
nix build
# or run directly:
nix run
```

### Home Manager

Add the flake input and import `homeManagerModules.default`:

```nix
{
  programs.agent-queue = {
    enable = true;
  };
}
```

## TUI Keybindings

Run `agent-queue` with no arguments to open the interactive TUI:

| Key | Action |
| --- | --- |
| `j` / `Down` / `Ctrl+n` | Move selection down |
| `k` / `Up` / `Ctrl+p` | Move selection up |
| `h` / `l` / `Left` / `Right` / `f` | Toggle between **Waiting** and **History** tabs |
| `Enter` | Jump to selected `tmux` pane and mark as `visited` (when in Waiting) |
| `o` | Peek: jump to selected `tmux` pane without changing queue state |
| `d` | Dismiss selected entry (Waiting → History, History → delete) |
| `/` | Enter search mode (`Enter` locks filter for navigation, `Esc` clears) |
| `Esc` / `Ctrl+[` / `Ctrl+c` | Clear active or locked search filter |
| `q` | Quit |

## CLI Usage

```bash
# Push or update a pane in the queue (defaults to $TMUX_PANE)
agent-queue push [--pane %1] [--stop-reason needs_input|error|visited|running] [--summary "Text"] [--cwd /path]

# Clear a waiting entry (moves to history by default)
agent-queue clear [--pane %1] [--no-history | --running] [--summary "Text"] [--cwd /path]

# Mark a waiting entry as visited
agent-queue visit [--pane %1]

# Jump to a pane in tmux (prints JSON with pane_id, client_pid, terminal_pid)
agent-queue jump [--pane %1] [--visit | --dismiss]

# List entries (TSV by default, or JSON array with --json)
agent-queue list [--state waiting|history] [--json]

# Stream deduplicated newline-delimited JSON state changes
agent-queue watch
```

## Integrations

### Pi Coding Agent

Load `pi/extension.ts` in Pi (or symlink it into `~/.pi/agent/extensions/`):

```bash
pi -e /path/to/agent-queue/pi/extension.ts
```

The extension resolves the `agent-queue` binary via `$AGENT_QUEUE_BIN`, `$PATH`, local `target/{release,debug}/agent-queue`, or `~/.cargo/bin/agent-queue`.

### Claude Code Hooks

Point Claude Code hooks at `plugin/hooks/hooks.json` or configure the hook subcommands directly in `~/.claude/settings.json`:

- `UserPromptSubmit`: `agent-queue hook prompt-submit`
- `Notification`: `agent-queue hook notification`
- `Stop`: `agent-queue hook stop`

## Configuration

| Environment Variable | Description |
| --- | --- |
| `AGENT_QUEUE_DIR` | Override directory for `queue.db` (defaults to `$XDG_DATA_HOME/agent-queue` / `~/.local/share/agent-queue`) |
| `AGENT_QUEUE_BIN` | Override `agent-queue` binary path for `pi/extension.ts` |
