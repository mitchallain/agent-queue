pub mod app;
pub mod cli;
pub mod input;
pub mod queue;
pub mod tmux;
pub mod ui;

use std::env;
use std::fs;
use std::io;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use notify::RecursiveMode;
use notify_debouncer_mini::new_debouncer;
use ratatui::{backend::CrosstermBackend, Terminal};

use crate::app::App;
use crate::cli::{parse_args, run_command, Command};
use crate::input::map_key;
use crate::queue::queue_dir;
use crate::tmux::{prune_stale_panes_in, prune_stale_running_panes_in};

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    let cmd = parse_args(&args)?;

    match cmd {
        Command::Tui => run_tui(),
        other => run_command(other),
    }
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
}

fn run_tui() -> Result<()> {
    let dir = queue_dir();
    fs::create_dir_all(&dir)?;

    let _ = prune_stale_panes_in(&dir);
    let _ = prune_stale_running_panes_in(&dir);

    let mut app = App::new(dir.clone())?;

    // Set up 50ms debounced filesystem watcher on queue_dir
    let (tx, rx) = mpsc::channel();
    let mut debouncer = new_debouncer(Duration::from_millis(50), move |res| {
        let _ = tx.send(res);
    })?;
    debouncer
        .watcher()
        .watch(&dir, RecursiveMode::NonRecursive)?;

    // Install panic hook to ensure terminal is restored on panic
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let loop_result = run_event_loop(&mut terminal, &mut app, &rx);

    restore_terminal();
    let _ = terminal.show_cursor();

    loop_result
}

fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    fs_rx: &mpsc::Receiver<notify_debouncer_mini::DebounceEventResult>,
) -> Result<()> {
    let mut last_prune = Instant::now();

    while app.running {
        terminal.draw(|frame| ui::draw(frame, app))?;

        let mut needs_reload = false;
        while fs_rx.try_recv().is_ok() {
            needs_reload = true;
        }

        if last_prune.elapsed() >= Duration::from_secs(3) {
            let _ = prune_stale_panes_in(&app.queue_dir);
            let _ = prune_stale_running_panes_in(&app.queue_dir);
            needs_reload = true;
            last_prune = Instant::now();
        }

        if needs_reload {
            app.reload()?;
        }

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    let action = map_key(key.code, key.modifiers, app.search_active);
                    app.handle_action(action)?;
                }
            }
        }
    }

    Ok(())
}
