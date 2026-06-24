mod app;
mod db;
mod fuzzy;
mod import;
mod ui;

use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::Terminal;
use ratatui::prelude::CrosstermBackend;

use app::{App, AppResult};

/// Search and resume OpenCode sessions across folders.
#[derive(Parser)]
#[command(name = env!("CARGO_PKG_NAME"), version, about, long_about = None, disable_version_flag = true)]
struct Cli {
    /// Always import a copy of the selected session into the current directory,
    /// even if it belongs to the current folder's project (instead of resuming
    /// it in place).
    #[arg(long)]
    cwd: bool,

    /// Use the SQLite database at <path> instead of the default.
    #[arg(long, value_name = "path")]
    db: Option<PathBuf>,

    /// Print version information and exit.
    #[arg(short = 'v', long, action = clap::ArgAction::Version)]
    version: (),
}

fn main() {
    let cli = Cli::parse();

    let db_override = cli.db;

    // When set, always import/export the session into the current working
    // directory instead of resuming it in place — even if the session already
    // belongs to the current folder's project.
    let force_cwd = cli.cwd;

    // Set up channel and spawn background loader
    let (tx, rx) = mpsc::channel();
    let loader_db_override = db_override.clone();
    std::thread::spawn(move || {
        db::stream_sessions(loader_db_override, tx);
    });

    let mut app = App::new(rx);

    // Set up terminal
    enable_raw_mode().expect("failed to enable raw mode");
    crossterm::execute!(io::stdout(), EnterAlternateScreen).expect("failed to enter alt screen");
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).expect("failed to create terminal");

    // Main loop — poll with a short timeout so we can receive new session batches
    loop {
        // Drain any pending sessions from the background thread
        app.poll_sessions();

        terminal
            .draw(|f| ui::draw(f, &mut app))
            .expect("draw failed");

        if app.should_exit() {
            break;
        }

        // Poll for keyboard events with a short timeout to stay responsive
        // to incoming session data
        if event::poll(Duration::from_millis(50)).unwrap_or(false) {
            if let Ok(Event::Key(key)) = event::read() {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Esc => app.quit(),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.quit()
                    }
                    KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.cursor = 0;
                    }
                    KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.cursor = app.query.len();
                    }
                    KeyCode::Enter => app.confirm(),
                    KeyCode::Backspace => app.backspace(),
                    KeyCode::Left => app.move_cursor_left(),
                    KeyCode::Right => app.move_cursor_right(),
                    KeyCode::Up => app.move_up(),
                    KeyCode::Down => app.move_down(),
                    KeyCode::F(2) => app.toggle_sort(),
                    KeyCode::Char(c) => app.type_char(c),
                    _ => {}
                }
            }
        }
    }

    // Restore terminal
    disable_raw_mode().expect("failed to disable raw mode");
    crossterm::execute!(io::stdout(), LeaveAlternateScreen).expect("failed to leave alt screen");

    // Act on result. If the selected session belongs to the project of the
    // current directory, resume it directly. Otherwise opencode would resume
    // it in its original directory, so copy it here (export-then-import)
    // and open the copy instead.
    if let Some(AppResult::Selected(session)) = app.result {
        if !force_cwd && same_project(db_override.as_deref(), &session.id) {
            let err = Command::new("opencode").arg("-s").arg(&session.id).exec();
            eprintln!("Failed to exec opencode: {err}");
            std::process::exit(1);
        }
        let cwd = std::env::current_dir()
            .map(|d| d.display().to_string())
            .unwrap_or_else(|_| ".".to_string());
        eprintln!("Importing session {} into {cwd} ...", session.id);
        match import::import_session(&session.id) {
            Ok(new_id) => {
                let err = Command::new("opencode").arg("-s").arg(&new_id).exec();
                eprintln!("Failed to exec opencode: {err}");
                std::process::exit(1);
            }
            Err(err) => {
                eprintln!("Failed to import session: {err}");
                std::process::exit(1);
            }
        }
    }
}

/// Whether the session belongs to the same opencode project as the current
/// working directory: the cwd's git toplevel matches one of the project's
/// known checkout directories, or both are outside any git repo (opencode's
/// "global" project).
fn same_project(db_override: Option<&Path>, session_id: &str) -> bool {
    let toplevel = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string());
    let Ok(project) = db::session_project(db_override, session_id) else {
        return false;
    };
    match toplevel {
        Some(toplevel) => {
            let toplevel = canonical(&toplevel);
            project.directories.iter().any(|d| canonical(d) == toplevel)
        }
        None => project.id == "global",
    }
}

fn canonical(path: &str) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path))
}
