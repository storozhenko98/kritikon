mod app;
mod github;
mod model;
mod ui;

use std::{
    io::{self, stdout},
    time::Duration,
};

use anyhow::{Context, Result};
use app::{Action, App};
use clap::Parser;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

#[derive(Debug, Parser)]
#[command(
    name = "review-monitor",
    version,
    about = "A calm, complete terminal dashboard for GitHub pull-request reviews"
)]
struct Cli {
    /// Automatic refresh interval in seconds; use 0 to disable.
    #[arg(long, default_value_t = 300)]
    refresh_seconds: u64,

    /// Show only review requests made directly to you, not your teams.
    #[arg(long)]
    no_team_requests: bool,

    /// Fetch data, print scope/counts, and exit without starting the TUI.
    #[arg(long)]
    check: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let include_team_requests = !cli.no_team_requests;

    if cli.check {
        let data = github::fetch_dashboard(include_team_requests)?;
        println!("Authenticated as @{}", data.viewer);
        println!("Review requested: {} open PR(s)", data.requested.len());
        println!("My PRs:           {} open PR(s)", data.owned.len());
        if include_team_requests {
            println!("Team scope:       {} team(s) checked", data.team_count);
        } else {
            println!("Team scope:       disabled");
        }
        for warning in data.warnings {
            println!("Warning:          {warning}");
        }
        return Ok(());
    }

    run_tui(include_team_requests, cli.refresh_seconds)
}

fn run_tui(include_team_requests: bool, refresh_seconds: u64) -> Result<()> {
    let mut terminal = setup_terminal()?;
    let _guard = TerminalGuard;
    let mut app = App::new(include_team_requests, refresh_seconds);
    app.begin_refresh();

    loop {
        app.tick();
        terminal
            .draw(|frame| ui::render(frame, &mut app))
            .context("could not draw the terminal interface")?;

        if event::poll(Duration::from_millis(100)).context("could not poll terminal input")? {
            let action = match event::read().context("could not read terminal input")? {
                Event::Key(key) if key.kind == KeyEventKind::Press => app.handle_key(key),
                Event::Mouse(mouse) => app.handle_mouse(mouse),
                _ => Action::None,
            };

            match action {
                Action::None => {}
                Action::Quit => break,
                Action::Refresh => app.begin_refresh(),
                Action::Open(url) => match open::that_detached(&url) {
                    Ok(()) => app.set_notice("Opened selected PR in your browser"),
                    Err(error) => app.set_notice(format!("Could not open browser: {error}")),
                },
            }
        }
    }

    Ok(())
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode().context("could not enable terminal raw mode")?;
    let mut output = stdout();
    if let Err(error) = execute!(output, EnterAlternateScreen, EnableMouseCapture) {
        let _ = disable_raw_mode();
        return Err(error).context("could not initialize terminal screen");
    }
    match Terminal::new(CrosstermBackend::new(output)) {
        Ok(terminal) => Ok(terminal),
        Err(error) => {
            let _ = disable_raw_mode();
            let _ = execute!(stdout(), LeaveAlternateScreen, DisableMouseCapture);
            Err(error).context("could not create terminal backend")
        }
    }
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), LeaveAlternateScreen, DisableMouseCapture);
    }
}
