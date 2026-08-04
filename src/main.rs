mod app;
mod clipboard;
mod config;
#[cfg(debug_assertions)]
mod dev;
mod github;
mod model;
mod playbook;
mod review_agent;
mod ui;
mod updater;

#[cfg(debug_assertions)]
use std::time::Instant;
use std::{
    io::{self, stdout},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use app::{Action, App, DataSource};
use clap::Parser;
use config::{Config, ConfigStore, parse_refresh_seconds};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
#[cfg(debug_assertions)]
use dev::DevScenario;
use ratatui::{Terminal, backend::CrosstermBackend};

#[derive(Debug, Parser)]
#[command(
    name = "kritikon",
    version,
    about = "A focused GitHub pull-request command center for your terminal"
)]
struct Cli {
    /// Override the configured refresh interval for this run (whole seconds, minimum 5).
    #[arg(long, value_parser = parse_refresh_seconds)]
    refresh_seconds: Option<u64>,

    /// Delete the saved configuration and restore defaults, then exit.
    #[arg(long, conflicts_with_all = ["check", "refresh_seconds"])]
    reset_config: bool,

    /// Fetch data, print configuration/counts, and exit without starting the TUI.
    #[arg(long)]
    check: bool,

    /// Use generated data for UI development without calling GitHub.
    #[cfg(debug_assertions)]
    #[arg(long, value_enum)]
    dev_scenario: Option<DevScenario>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let store = config_store(&cli)?;

    if cli.reset_config {
        let deleted = store.reset()?;
        println!(
            "{} {}",
            if deleted {
                "Deleted"
            } else {
                "No config found at"
            },
            store.path().display()
        );
        println!(
            "Defaults: {} seconds; all direct and team review requests are always included",
            Config::default().refresh_seconds
        );
        return Ok(());
    }

    let (mut settings, config_error) = match store.load_or_default() {
        Ok(config) => (config, None),
        Err(error) => (Config::default(), Some(format!("{error:#}"))),
    };
    if let Some(seconds) = cli.refresh_seconds {
        settings.refresh_seconds = seconds;
    }
    settings.validate()?;

    let source = data_source(&cli);
    if cli.check {
        if let Some(error) = config_error {
            bail!(
                "{error}\nRun `kritikon --reset-config` or launch the TUI and press t to repair it."
            );
        }
        let data = fetch_once(source)?;
        println!("Authenticated as @{}", data.viewer);
        println!("To review:        {} open PR(s)", data.review_queue.len());
        println!("Involved:         {} open PR(s)", data.involved.len());
        println!("My PRs:           {} open PR(s)", data.owned.len());
        println!("Refresh:          {} seconds", settings.refresh_seconds);
        println!("Teams monitored:  {}", data.teams.len());
        for team in &data.teams {
            println!("  - {team}");
        }
        println!("Config:           {}", store.path().display());
        for warning in data.warnings {
            println!("Warning:          {warning}");
        }
        return Ok(());
    }

    if updater::check_and_prompt() == updater::StartupAction::RestartRequired {
        return Ok(());
    }

    run_tui(settings, store, source, config_error)
}

fn config_store(cli: &Cli) -> Result<ConfigStore> {
    #[cfg(debug_assertions)]
    if cli.dev_scenario.is_some() {
        return Ok(ConfigStore::at(
            std::env::current_dir()
                .context("could not determine current directory for development config")?
                .join("target/kritikon-dev/config.toml"),
        ));
    }
    let _ = cli;
    ConfigStore::system()
}

fn data_source(cli: &Cli) -> DataSource {
    #[cfg(debug_assertions)]
    if let Some(scenario) = cli.dev_scenario {
        return DataSource::Dev(scenario);
    }
    let _ = cli;
    DataSource::Github
}

fn fetch_once(source: DataSource) -> Result<model::DashboardData> {
    match source {
        DataSource::Github => github::fetch_complete_dashboard(),
        #[cfg(debug_assertions)]
        DataSource::Dev(scenario) => dev::fetch_dashboard(scenario, 0),
    }
}

fn run_tui(
    settings: Config,
    store: ConfigStore,
    source: DataSource,
    config_error: Option<String>,
) -> Result<()> {
    let playbook_store = playbook::PlaybookStore::beside_config(store.path())?;
    let (custom_playbooks, playbook_warning) = match playbook_store.load_or_default() {
        Ok(playbooks) => (playbooks, None),
        Err(error) => (
            Vec::new(),
            Some(format!(
                "Saved playbooks could not be loaded; built-ins remain available. {error:#}"
            )),
        ),
    };
    let mut terminal = setup_terminal()?;
    let _guard = TerminalGuard;
    let mut app = App::new(settings, store.path().to_path_buf(), source);
    app.set_custom_playbooks(custom_playbooks, playbook_warning);
    let mut review_coordinator = review_agent::ReviewCoordinator::system()?;
    #[cfg(debug_assertions)]
    let mut development_review_events = Vec::<(Instant, review_agent::ReviewEvent)>::new();
    if let Some(error) = config_error {
        app.open_config(Some(format!(
            "Saved configuration is invalid; safe defaults are active. {error}"
        )));
    }
    #[cfg(debug_assertions)]
    if source == DataSource::Dev(DevScenario::ConfigError) {
        app.open_config(Some(
            "Simulated invalid configuration. Save valid values or reset it.".into(),
        ));
    }
    app.begin_refresh();

    loop {
        for event in review_coordinator.drain_events() {
            apply_review_event(&mut app, event);
        }
        #[cfg(debug_assertions)]
        {
            let now = Instant::now();
            let mut index = 0;
            while index < development_review_events.len() {
                if development_review_events[index].0 <= now {
                    let (_, event) = development_review_events.remove(index);
                    apply_review_event(&mut app, event);
                } else {
                    index += 1;
                }
            }
        }
        app.tick();
        terminal
            .draw(|frame| ui::render(frame, &mut app))
            .context("could not draw the terminal interface")?;

        if event::poll(Duration::from_millis(50)).context("could not poll terminal input")? {
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
                Action::CopyBranch(branch) => match clipboard::copy(&branch) {
                    Ok(()) => app.set_notice(format!("Copied branch: {branch}")),
                    Err(error) => {
                        app.set_notice(format!("Could not copy branch: {error:#}"));
                    }
                },
                Action::CopyUrl(url) => match clipboard::copy(&url) {
                    Ok(()) => app.set_notice("Copied selected PR URL"),
                    Err(error) => {
                        app.set_notice(format!("Could not copy PR URL: {error:#}"));
                    }
                },
                Action::OpenReview(target) => {
                    if app.show_review_for_target(&target.url) {
                        continue;
                    }
                    #[cfg(debug_assertions)]
                    let result = if source != DataSource::Github {
                        Ok(review_agent::development_snapshot(target.clone()))
                    } else {
                        review_agent::inspect(target.clone()).map_err(|error| format!("{error:#}"))
                    };
                    #[cfg(not(debug_assertions))]
                    let result =
                        review_agent::inspect(target.clone()).map_err(|error| format!("{error:#}"));

                    match result {
                        Ok(snapshot) => app.show_review_snapshot(snapshot),
                        Err(error) => app.show_review_error(target, error),
                    }
                }
                Action::LaunchReview {
                    target,
                    mode,
                    focus,
                } => {
                    #[cfg(debug_assertions)]
                    if source != DataSource::Github {
                        match mode {
                            review_agent::LaunchMode::Review(kind) => {
                                let mut preparing =
                                    review_agent::development_snapshot(target.clone());
                                if kind != review_agent::ReviewRunKind::FollowUp {
                                    preparing.draft = None;
                                }
                                if kind == review_agent::ReviewRunKind::NewSession {
                                    preparing.session_id = None;
                                }
                                app.review_started(preparing);

                                let mut running =
                                    review_agent::development_snapshot(target.clone());
                                if kind != review_agent::ReviewRunKind::FollowUp {
                                    running.draft = None;
                                }
                                let mut completed = review_agent::development_snapshot(target);
                                if let Some(focus) = focus {
                                    completed.warning = Some(format!(
                                        "DEV simulation used custom focus: {}",
                                        focus.trim()
                                    ));
                                }
                                let now = Instant::now();
                                development_review_events.push((
                                    now + Duration::from_millis(600),
                                    review_agent::ReviewEvent::SessionReady(running),
                                ));
                                development_review_events.push((
                                    now + Duration::from_secs(2),
                                    review_agent::ReviewEvent::Completed(completed),
                                ));
                            }
                            review_agent::LaunchMode::Chat => {
                                app.set_notice(
                                    "DEV: simulated OpenCode attach/detach; background review continues",
                                );
                            }
                        }
                        continue;
                    }

                    match mode {
                        review_agent::LaunchMode::Review(kind) => {
                            match review_coordinator.start_review(target.clone(), kind, focus) {
                                Ok(snapshot) => app.review_started(snapshot),
                                Err(error) => {
                                    app.show_review_error(target, format!("{error:#}"));
                                }
                            }
                        }
                        review_agent::LaunchMode::Chat => {
                            let snapshot = match review_agent::inspect(target.clone()) {
                                Ok(snapshot) => snapshot,
                                Err(error) => {
                                    app.show_review_error(target, format!("{error:#}"));
                                    continue;
                                }
                            };
                            suspend_terminal(&mut terminal)?;
                            let result = review_coordinator
                                .open_chat(&snapshot)
                                .map_err(|error| format!("{error:#}"));
                            resume_terminal(&mut terminal)?;
                            match result {
                                Ok(snapshot) => app.review_chat_closed(snapshot),
                                Err(error) => app.show_review_error(target, error),
                            }
                        }
                    }
                }
                Action::PostReview(snapshot, kind) => {
                    #[cfg(debug_assertions)]
                    if source != DataSource::Github {
                        app.review_posted(kind);
                        continue;
                    }

                    match review_agent::post_review(&snapshot, kind) {
                        Ok(()) => app.review_posted(kind),
                        Err(error) => app.review_failed(format!("{error:#}")),
                    }
                }
                Action::SavePlaybooks {
                    playbooks,
                    selected_name,
                    notice,
                } => match playbook_store.save(&playbooks) {
                    Ok(()) => app.playbooks_saved(playbooks, selected_name, notice),
                    Err(error) => app.playbook_write_failed(format!(
                        "Could not save review playbooks: {error:#}"
                    )),
                },
                Action::SaveConfig(config) => match store.save(&config) {
                    Ok(()) => app.apply_config(config, false),
                    Err(error) => app.config_write_failed(format!("Could not save: {error:#}")),
                },
                Action::ResetConfig => match store.reset() {
                    Ok(_) => app.apply_config(Config::default(), true),
                    Err(error) => app.config_write_failed(format!("Could not reset: {error:#}")),
                },
            }
        }
    }

    Ok(())
}

fn apply_review_event(app: &mut App, event: review_agent::ReviewEvent) {
    match event {
        review_agent::ReviewEvent::SessionReady(snapshot) => {
            app.review_session_ready(snapshot);
        }
        review_agent::ReviewEvent::Completed(snapshot) => {
            app.review_completed(snapshot);
        }
        review_agent::ReviewEvent::Failed { snapshot, error } => {
            app.review_background_failed(snapshot, error);
        }
    }
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

fn suspend_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    terminal
        .show_cursor()
        .context("could not show terminal cursor")?;
    disable_raw_mode().context("could not suspend terminal raw mode")?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )
    .context("could not suspend Kritikon terminal screen")
}

fn resume_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    enable_raw_mode().context("could not restore terminal raw mode")?;
    if let Err(error) = execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        EnableMouseCapture
    ) {
        let _ = disable_raw_mode();
        return Err(error).context("could not restore Kritikon terminal screen");
    }
    terminal.clear().context("could not redraw Kritikon")?;
    terminal
        .hide_cursor()
        .context("could not hide terminal cursor")
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), LeaveAlternateScreen, DisableMouseCapture);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_refresh_values_are_always_validated_as_seconds() {
        assert!(Cli::try_parse_from(["kritikon", "--refresh-seconds", "4"]).is_err());
        assert!(Cli::try_parse_from(["kritikon", "--refresh-seconds", "5s"]).is_err());
        assert!(Cli::try_parse_from(["kritikon", "--refresh-seconds", "5"]).is_ok());
        assert!(
            Cli::try_parse_from(["kritikon", "--refresh-seconds", &u64::MAX.to_string(),]).is_ok()
        );
    }

    #[test]
    fn removed_scope_flag_is_rejected() {
        assert!(Cli::try_parse_from(["kritikon", "--review-scope", "teams"]).is_err());
    }
}
