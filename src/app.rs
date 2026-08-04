use std::{
    collections::HashMap,
    path::PathBuf,
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::{Duration, Instant},
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::{
    config::{Config, parse_refresh_seconds},
    github,
    model::{DashboardData, InvolvementReason, PullRequest},
    playbook::{self, ReviewPlaybook},
    review_agent::{LaunchMode, ReviewKind, ReviewRunKind, ReviewSnapshot, ReviewTarget},
};
#[cfg(debug_assertions)]
use crate::{dev, dev::DevScenario};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    ReviewQueue,
    Involved,
    Owned,
}

impl Tab {
    pub fn index(self) -> usize {
        match self {
            Self::ReviewQueue => 0,
            Self::Involved => 1,
            Self::Owned => 2,
        }
    }

    fn cycle(self, delta: isize) -> Self {
        const TABS: [Tab; 3] = [Tab::ReviewQueue, Tab::Involved, Tab::Owned];
        let index = self.index();
        TABS[(index as isize + delta).rem_euclid(TABS.len() as isize) as usize]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataSource {
    Github,
    #[cfg(debug_assertions)]
    Dev(DevScenario),
}

impl DataSource {
    pub fn label(self) -> Option<&'static str> {
        match self {
            Self::Github => None,
            #[cfg(debug_assertions)]
            Self::Dev(scenario) => Some(scenario.label()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    Refresh,
    Open(String),
    CopyBranch(String),
    CopyUrl(String),
    OpenReview(ReviewTarget),
    LaunchReview {
        target: ReviewTarget,
        mode: LaunchMode,
        focus: Option<String>,
    },
    PostReview(ReviewSnapshot, ReviewKind),
    SavePlaybooks {
        playbooks: Vec<ReviewPlaybook>,
        selected_name: Option<String>,
        notice: String,
    },
    SaveConfig(Config),
    ResetConfig,
}

#[derive(Debug, Clone)]
pub struct RowHitbox {
    pub rect: Rect,
    pub index: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFocus {
    Refresh,
    Save,
    Reset,
    Cancel,
}

impl ConfigFocus {
    fn cycle(self, delta: isize) -> Self {
        const FIELDS: [ConfigFocus; 4] = [
            ConfigFocus::Refresh,
            ConfigFocus::Save,
            ConfigFocus::Reset,
            ConfigFocus::Cancel,
        ];
        let index = FIELDS.iter().position(|field| *field == self).unwrap_or(0);
        FIELDS[(index as isize + delta).rem_euclid(FIELDS.len() as isize) as usize]
    }
}

#[derive(Debug, Clone)]
pub struct ConfigEditor {
    pub refresh_input: String,
    pub refresh_pristine: bool,
    pub focus: ConfigFocus,
    pub error: Option<String>,
    pub confirm_reset: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewPanelMode {
    Prompt,
    Running(ReviewRunPhase),
    Draft,
    ConfirmNewSession,
    PostChoice,
    ConfirmPost(ReviewKind),
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewRunPhase {
    Preparing,
    Reviewing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewAgentState {
    Preparing,
    Reviewing,
    Ready,
    Draft,
    Session,
    Failed,
}

#[derive(Debug, Clone)]
pub struct ReviewPanel {
    pub snapshot: ReviewSnapshot,
    pub mode: ReviewPanelMode,
    pub run_kind: ReviewRunKind,
    pub input: String,
    pub applied_playbook: Option<String>,
    pub scroll: u16,
    pub max_scroll: u16,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybookEditorMode {
    Library,
    Name,
    Body,
    ConfirmDelete,
}

#[derive(Debug, Clone)]
pub struct PlaybookEditor {
    pub mode: PlaybookEditorMode,
    pub selected: usize,
    pub name_input: String,
    pub prompt_input: String,
    pub original_custom_name: Option<String>,
    pub error: Option<String>,
}

impl PlaybookEditor {
    fn library(selected: usize) -> Self {
        Self {
            mode: PlaybookEditorMode::Library,
            selected,
            name_input: String::new(),
            prompt_input: String::new(),
            original_custom_name: None,
            error: None,
        }
    }
}

#[derive(Debug, Clone)]
struct ActiveReview {
    snapshot: ReviewSnapshot,
    phase: ReviewRunPhase,
}

impl ConfigEditor {
    fn new(config: Config, error: Option<String>) -> Self {
        Self {
            refresh_input: config.refresh_seconds.to_string(),
            refresh_pristine: true,
            focus: ConfigFocus::Refresh,
            error,
            confirm_reset: false,
        }
    }

    fn config(&self) -> Result<Config, String> {
        let seconds = parse_refresh_seconds(&self.refresh_input)?;
        Config::new(seconds).map_err(|error| error.to_string())
    }
}

#[derive(Debug, Clone, Default)]
pub struct ConfigHitboxes {
    pub refresh: Rect,
    pub save: Rect,
    pub reset: Rect,
    pub cancel: Rect,
}

#[derive(Debug, Clone, Default)]
pub struct PlaybookHitboxes {
    pub rows: Vec<RowHitbox>,
    pub primary: Rect,
    pub new: Rect,
    pub edit: Rect,
    pub delete: Rect,
    pub back: Rect,
}

pub struct App {
    pub data: Option<DashboardData>,
    pub tab: Tab,
    pub selected: [usize; 3],
    pub offsets: [usize; 3],
    pub visible_rows: usize,
    pub loading: bool,
    pub commit_loading: bool,
    pub error: Option<String>,
    pub show_help: bool,
    pub details_expanded: bool,
    pub detail_scroll: u16,
    pub detail_max_scroll: u16,
    pub row_hitboxes: Vec<RowHitbox>,
    pub tab_hitboxes: [Rect; 3],
    pub notice: Option<(String, Instant)>,
    pub config: Config,
    pub config_path: PathBuf,
    pub config_editor: Option<ConfigEditor>,
    pub config_hitboxes: ConfigHitboxes,
    pub review_panel: Option<ReviewPanel>,
    pub playbook_editor: Option<PlaybookEditor>,
    pub playbook_hitboxes: PlaybookHitboxes,
    pub playbook_warning: Option<String>,
    custom_playbooks: Vec<ReviewPlaybook>,
    review_panels: HashMap<String, ReviewPanel>,
    active_reviews: HashMap<String, ActiveReview>,
    ready_reviews: HashMap<String, String>,
    pub refresh_remaining: Duration,
    pub data_source: DataSource,
    refresh_receiver: Option<Receiver<RefreshMessage>>,
    commit_receiver: Option<Receiver<Result<github::CommitInvolvement, String>>>,
    committed_pull_requests: Vec<PullRequest>,
    commit_warnings: Vec<String>,
    refresh_generation: u64,
    refresh_requested: bool,
    last_timer_tick: Instant,
}

impl App {
    pub fn new(config: Config, config_path: PathBuf, data_source: DataSource) -> Self {
        Self {
            data: None,
            tab: Tab::ReviewQueue,
            selected: [0, 0, 0],
            offsets: [0, 0, 0],
            visible_rows: 1,
            loading: false,
            commit_loading: false,
            error: None,
            show_help: false,
            details_expanded: false,
            detail_scroll: 0,
            detail_max_scroll: 0,
            row_hitboxes: Vec::new(),
            tab_hitboxes: [Rect::default(), Rect::default(), Rect::default()],
            notice: None,
            config,
            config_path,
            config_editor: None,
            config_hitboxes: ConfigHitboxes::default(),
            review_panel: None,
            playbook_editor: None,
            playbook_hitboxes: PlaybookHitboxes::default(),
            playbook_warning: None,
            custom_playbooks: Vec::new(),
            review_panels: HashMap::new(),
            active_reviews: HashMap::new(),
            ready_reviews: HashMap::new(),
            refresh_remaining: Duration::ZERO,
            data_source,
            refresh_receiver: None,
            commit_receiver: None,
            committed_pull_requests: Vec::new(),
            commit_warnings: Vec::new(),
            refresh_generation: 0,
            refresh_requested: false,
            last_timer_tick: Instant::now(),
        }
    }

    #[cfg(test)]
    pub fn with_data(data: DashboardData) -> Self {
        let mut app = Self::new(
            Config::default(),
            PathBuf::from("/tmp/kritikon-test.toml"),
            DataSource::Github,
        );
        app.data = Some(data);
        app
    }

    pub fn open_config(&mut self, error: Option<String>) {
        self.show_help = false;
        self.close_review_panel();
        self.config_editor = Some(ConfigEditor::new(self.config, error));
    }

    pub fn set_custom_playbooks(
        &mut self,
        playbooks: Vec<ReviewPlaybook>,
        warning: Option<String>,
    ) {
        self.custom_playbooks = playbooks;
        self.playbook_warning = warning;
    }

    pub fn playbook_catalog(&self) -> Vec<ReviewPlaybook> {
        playbook::catalog(&self.custom_playbooks)
    }

    pub fn playbooks_saved(
        &mut self,
        playbooks: Vec<ReviewPlaybook>,
        selected_name: Option<String>,
        notice: String,
    ) {
        self.custom_playbooks = playbooks;
        self.playbook_warning = None;
        let catalog = self.playbook_catalog();
        if let Some(editor) = &mut self.playbook_editor {
            editor.mode = PlaybookEditorMode::Library;
            editor.selected = selected_name
                .as_deref()
                .and_then(|selected| {
                    catalog
                        .iter()
                        .position(|playbook| playbook.name.eq_ignore_ascii_case(selected))
                })
                .unwrap_or_else(|| editor.selected.min(catalog.len().saturating_sub(1)));
            editor.name_input.clear();
            editor.prompt_input.clear();
            editor.original_custom_name = None;
            editor.error = None;
        }
        self.set_notice(notice);
    }

    pub fn playbook_write_failed(&mut self, error: impl Into<String>) {
        if let Some(editor) = &mut self.playbook_editor {
            editor.error = Some(error.into());
        }
    }

    pub fn show_review_snapshot(&mut self, snapshot: ReviewSnapshot) {
        let target_url = snapshot.target.url.clone();
        self.ready_reviews.remove(&target_url);
        self.review_panels.remove(&target_url);
        let mode = if snapshot.has_session() || snapshot.has_draft() {
            ReviewPanelMode::Draft
        } else {
            ReviewPanelMode::Prompt
        };
        self.present_review_panel(ReviewPanel {
            snapshot,
            mode,
            run_kind: ReviewRunKind::ReReview,
            input: String::new(),
            applied_playbook: None,
            scroll: 0,
            max_scroll: 0,
            error: None,
        });
    }

    pub fn review_started(&mut self, snapshot: ReviewSnapshot) {
        let target_url = snapshot.target.url.clone();
        self.ready_reviews.remove(&target_url);
        self.review_panels.remove(&target_url);
        self.active_reviews.insert(
            target_url,
            ActiveReview {
                snapshot: snapshot.clone(),
                phase: ReviewRunPhase::Preparing,
            },
        );
        self.show_running_review(snapshot, ReviewRunPhase::Preparing);
    }

    pub fn review_session_ready(&mut self, snapshot: ReviewSnapshot) {
        let target_url = snapshot.target.url.clone();
        self.active_reviews.insert(
            target_url.clone(),
            ActiveReview {
                snapshot: snapshot.clone(),
                phase: ReviewRunPhase::Reviewing,
            },
        );
        if self
            .review_panel
            .as_ref()
            .is_some_and(|panel| panel.snapshot.target.url == target_url)
        {
            self.show_running_review(snapshot, ReviewRunPhase::Reviewing);
        }
    }

    pub fn review_completed(&mut self, snapshot: ReviewSnapshot) {
        let target_url = snapshot.target.url.clone();
        let has_draft = snapshot.has_draft();
        self.active_reviews.remove(&target_url);
        if self.review_panel.as_ref().is_some_and(|panel| {
            panel.snapshot.target.url == target_url
                && matches!(panel.mode, ReviewPanelMode::Running(_))
        }) {
            self.show_review_snapshot(snapshot);
        } else {
            self.review_panels.insert(
                target_url.clone(),
                Self::panel_for_snapshot(snapshot.clone(), ReviewPanelMode::Draft, None),
            );
            if has_draft {
                self.ready_reviews.insert(
                    target_url,
                    format!("{}#{}", snapshot.target.repository, snapshot.target.number),
                );
                self.set_notice(format!(
                    "OpenCode review ready: {}#{} — Shift+R to inspect",
                    snapshot.target.repository, snapshot.target.number
                ));
            } else {
                self.ready_reviews.remove(&target_url);
                self.set_notice(format!(
                    "OpenCode finished without a review draft: {}#{} — Shift+R to inspect or rerun",
                    snapshot.target.repository, snapshot.target.number
                ));
            }
        }
    }

    pub fn review_background_failed(&mut self, snapshot: ReviewSnapshot, error: impl Into<String>) {
        let error = error.into();
        let target_url = snapshot.target.url.clone();
        self.active_reviews.remove(&target_url);
        if snapshot.has_draft() {
            let mut preserved = snapshot;
            preserved.warning = Some(format!(
                "The follow-up could not produce a replacement. The previous draft was preserved. {error}"
            ));
            let panel = Self::panel_for_snapshot(preserved, ReviewPanelMode::Draft, None);
            if self
                .review_panel
                .as_ref()
                .is_some_and(|panel| panel.snapshot.target.url == target_url)
            {
                self.present_review_panel(panel);
            } else {
                self.review_panels.insert(target_url, panel);
                self.set_notice(
                    "OpenCode follow-up failed; the previous review draft was preserved",
                );
            }
            return;
        }
        if self
            .review_panel
            .as_ref()
            .is_some_and(|panel| panel.snapshot.target.url == target_url)
        {
            self.present_review_panel(Self::panel_for_snapshot(
                snapshot,
                ReviewPanelMode::Error,
                Some(error),
            ));
        } else {
            self.review_panels.insert(
                target_url,
                Self::panel_for_snapshot(snapshot, ReviewPanelMode::Error, Some(error.clone())),
            );
            self.set_notice(format!(
                "OpenCode review failed — Shift+R to inspect: {error}"
            ));
        }
    }

    pub fn show_review_for_target(&mut self, target_url: &str) -> bool {
        if let Some(active) = self.active_reviews.get(target_url).cloned() {
            self.show_running_review(active.snapshot, active.phase);
            return true;
        }
        let Some(panel) = self.review_panels.remove(target_url) else {
            return false;
        };
        self.ready_reviews.remove(target_url);
        self.present_review_panel(panel);
        true
    }

    pub fn review_chat_closed(&mut self, snapshot: ReviewSnapshot) {
        let target_url = snapshot.target.url.clone();
        if let Some(active) = self.active_reviews.get_mut(&target_url) {
            active.snapshot = snapshot.clone();
            let phase = active.phase;
            self.show_running_review(snapshot, phase);
        } else {
            self.show_review_snapshot(snapshot);
        }
    }

    pub fn active_review_count(&self) -> usize {
        self.active_reviews.len()
    }

    pub fn review_agent_state(&self, target_url: &str) -> Option<ReviewAgentState> {
        if let Some(active) = self.active_reviews.get(target_url) {
            return Some(match active.phase {
                ReviewRunPhase::Preparing => ReviewAgentState::Preparing,
                ReviewRunPhase::Reviewing => ReviewAgentState::Reviewing,
            });
        }
        if self.ready_reviews.contains_key(target_url) {
            return Some(ReviewAgentState::Ready);
        }
        self.review_panel
            .as_ref()
            .filter(|panel| panel.snapshot.target.url == target_url)
            .or_else(|| self.review_panels.get(target_url))
            .and_then(|panel| match panel.mode {
                ReviewPanelMode::Draft
                | ReviewPanelMode::ConfirmNewSession
                | ReviewPanelMode::PostChoice
                | ReviewPanelMode::ConfirmPost(_) => {
                    if panel.snapshot.has_draft() {
                        Some(ReviewAgentState::Draft)
                    } else if panel.snapshot.has_session() {
                        Some(ReviewAgentState::Session)
                    } else {
                        None
                    }
                }
                ReviewPanelMode::Error => Some(ReviewAgentState::Failed),
                ReviewPanelMode::Running(ReviewRunPhase::Preparing) => {
                    Some(ReviewAgentState::Preparing)
                }
                ReviewPanelMode::Running(ReviewRunPhase::Reviewing) => {
                    Some(ReviewAgentState::Reviewing)
                }
                ReviewPanelMode::Prompt => None,
            })
    }

    pub fn ready_review_labels(&self) -> Vec<&str> {
        let mut labels = self
            .ready_reviews
            .values()
            .map(String::as_str)
            .collect::<Vec<_>>();
        labels.sort_unstable();
        labels
    }

    fn show_running_review(&mut self, snapshot: ReviewSnapshot, phase: ReviewRunPhase) {
        self.present_review_panel(Self::panel_for_snapshot(
            snapshot,
            ReviewPanelMode::Running(phase),
            None,
        ));
    }

    pub fn show_review_error(&mut self, target: ReviewTarget, error: impl Into<String>) {
        self.present_review_panel(Self::panel_for_snapshot(
            ReviewSnapshot {
                target,
                session_id: None,
                draft: None,
                draft_path: PathBuf::new(),
                workspace: PathBuf::new(),
                warning: None,
            },
            ReviewPanelMode::Error,
            Some(error.into()),
        ));
    }

    pub fn review_posted(&mut self, kind: ReviewKind) {
        if let Some(panel) = self.review_panel.take() {
            let target_url = panel.snapshot.target.url;
            self.review_panels.remove(&target_url);
            self.ready_reviews.remove(&target_url);
        }
        self.set_notice(format!("Posted {} review", kind.label()));
        self.begin_refresh();
    }

    pub fn review_failed(&mut self, error: impl Into<String>) {
        if let Some(panel) = &mut self.review_panel {
            panel.mode = ReviewPanelMode::Error;
            panel.error = Some(error.into());
        }
    }

    pub fn close_review_panel(&mut self) {
        self.playbook_editor = None;
        let Some(panel) = self.review_panel.take() else {
            return;
        };
        if !matches!(panel.mode, ReviewPanelMode::Running(_)) {
            self.review_panels
                .insert(panel.snapshot.target.url.clone(), panel);
        }
    }

    fn present_review_panel(&mut self, panel: ReviewPanel) {
        self.close_review_panel();
        self.review_panels.remove(&panel.snapshot.target.url);
        self.show_help = false;
        self.config_editor = None;
        self.review_panel = Some(panel);
    }

    fn panel_for_snapshot(
        snapshot: ReviewSnapshot,
        mode: ReviewPanelMode,
        error: Option<String>,
    ) -> ReviewPanel {
        ReviewPanel {
            snapshot,
            mode,
            run_kind: ReviewRunKind::ReReview,
            input: String::new(),
            applied_playbook: None,
            scroll: 0,
            max_scroll: 0,
            error,
        }
    }

    pub fn begin_refresh(&mut self) {
        if self.loading {
            self.refresh_requested = true;
            return;
        }
        self.loading = true;
        self.error = None;
        let source = self.data_source;
        #[cfg(debug_assertions)]
        let generation = self.refresh_generation;
        self.refresh_generation = self.refresh_generation.saturating_add(1);
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let result = match source {
                DataSource::Github => github::fetch_dashboard(),
                #[cfg(debug_assertions)]
                DataSource::Dev(scenario) => dev::fetch_dashboard(scenario, generation),
            }
            .map_err(|error| format!("{error:#}"));
            let _ = sender.send(RefreshMessage { result });
        });
        self.refresh_receiver = Some(receiver);
    }

    pub fn tick(&mut self) {
        let now = Instant::now();
        self.refresh_remaining = self
            .refresh_remaining
            .saturating_sub(now.saturating_duration_since(self.last_timer_tick));
        self.last_timer_tick = now;

        if let Some((_, expires_at)) = &self.notice
            && Instant::now() >= *expires_at
        {
            self.notice = None;
        }

        let received = self
            .refresh_receiver
            .as_ref()
            .map(|receiver| receiver.try_recv());
        match received {
            Some(Ok(RefreshMessage { result: Ok(data) })) => {
                let viewer = data.viewer.clone();
                self.apply_refresh(data);
                self.loading = false;
                self.error = None;
                self.refresh_receiver = None;
                self.schedule_next_refresh();
                self.begin_commit_enrichment(viewer);
            }
            Some(Ok(RefreshMessage { result: Err(error) })) => {
                self.loading = false;
                self.error = Some(error);
                self.refresh_receiver = None;
                self.schedule_next_refresh();
            }
            Some(Err(TryRecvError::Disconnected)) => {
                self.loading = false;
                self.error = Some("The background refresh stopped unexpectedly".into());
                self.refresh_receiver = None;
                self.schedule_next_refresh();
            }
            Some(Err(TryRecvError::Empty)) | None => {}
        }

        let commit_received = self
            .commit_receiver
            .as_ref()
            .map(|receiver| receiver.try_recv());
        match commit_received {
            Some(Ok(Ok(commit_involvement))) => {
                self.commit_loading = false;
                self.commit_receiver = None;
                if let Some(data) = &mut self.data {
                    strip_commit_involvement(data, &self.committed_pull_requests);
                    data.warnings
                        .retain(|warning| !self.commit_warnings.contains(warning));
                }
                self.committed_pull_requests = commit_involvement.pull_requests;
                self.commit_warnings = commit_involvement.warnings;
                if let Some(data) = &mut self.data {
                    merge_commit_involvement(
                        data,
                        &self.committed_pull_requests,
                        &self.commit_warnings,
                    );
                }
            }
            Some(Ok(Err(error))) => {
                self.commit_loading = false;
                self.commit_receiver = None;
                self.commit_warnings = vec![format!(
                    "Commit-based involvement is temporarily unavailable ({error})"
                )];
                if let Some(data) = &mut self.data {
                    merge_commit_involvement(data, &[], &self.commit_warnings);
                }
            }
            Some(Err(TryRecvError::Disconnected)) => {
                self.commit_loading = false;
                self.commit_receiver = None;
            }
            Some(Err(TryRecvError::Empty)) | None => {}
        }

        if !self.loading && std::mem::take(&mut self.refresh_requested) {
            self.begin_refresh();
            return;
        }

        if !self.loading && self.refresh_remaining.is_zero() {
            self.begin_refresh();
        }
    }

    fn apply_refresh(&mut self, data: DashboardData) {
        let mut data = data;
        merge_commit_involvement(
            &mut data,
            &self.committed_pull_requests,
            &self.commit_warnings,
        );
        let anchors = [
            self.selection_anchor(Tab::ReviewQueue),
            self.selection_anchor(Tab::Involved),
            self.selection_anchor(Tab::Owned),
        ];
        let active_url = self.selected_url();
        self.data = Some(data);

        for (tab, anchor) in [
            (Tab::ReviewQueue, &anchors[0]),
            (Tab::Involved, &anchors[1]),
            (Tab::Owned, &anchors[2]),
        ] {
            let tab_index = tab.index();
            let (new_index, item_count) = {
                let items = self.items_for(tab);
                let new_index = anchor
                    .url
                    .as_ref()
                    .and_then(|url| {
                        items
                            .iter()
                            .position(|pull_request| &pull_request.url == url)
                    })
                    .unwrap_or_else(|| self.selected[tab_index].min(items.len().saturating_sub(1)));
                (new_index, items.len())
            };
            self.selected[tab_index] = new_index;
            self.offsets[tab_index] = new_index
                .saturating_sub(anchor.row)
                .min(item_count.saturating_sub(self.visible_rows));
        }

        if active_url != self.selected_url() {
            self.detail_scroll = 0;
        }
        self.ensure_visible();
    }

    fn begin_commit_enrichment(&mut self, viewer: String) {
        if self.commit_loading || self.data_source != DataSource::Github {
            return;
        }
        self.commit_loading = true;
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let result =
                github::fetch_commit_involvement(&viewer).map_err(|error| format!("{error:#}"));
            let _ = sender.send(result);
        });
        self.commit_receiver = Some(receiver);
    }

    fn selection_anchor(&self, tab: Tab) -> SelectionAnchor {
        let index = self.selected[tab.index()];
        SelectionAnchor {
            url: self
                .items_for(tab)
                .get(index)
                .map(|pull_request| pull_request.url.clone()),
            row: index.saturating_sub(self.offsets[tab.index()]),
        }
    }

    fn schedule_next_refresh(&mut self) {
        self.refresh_remaining = Duration::from_secs(self.config.refresh_seconds);
        self.last_timer_tick = Instant::now();
    }

    pub fn apply_config(&mut self, config: Config, reset: bool) {
        self.config = config;
        self.config_editor = None;
        self.schedule_next_refresh();
        self.set_notice(if reset {
            "Configuration reset to defaults and file deleted"
        } else {
            "Configuration saved"
        });
    }

    pub fn config_write_failed(&mut self, error: impl Into<String>) {
        if let Some(editor) = &mut self.config_editor {
            editor.error = Some(error.into());
            editor.confirm_reset = false;
        }
    }

    fn items_for(&self, tab: Tab) -> &[crate::model::PullRequest] {
        let Some(data) = &self.data else {
            return &[];
        };
        match tab {
            Tab::ReviewQueue => &data.review_queue,
            Tab::Involved => &data.involved,
            Tab::Owned => &data.owned,
        }
    }

    pub fn items_len(&self) -> usize {
        self.items_for(self.tab).len()
    }

    pub fn selected_index(&self) -> usize {
        self.selected[self.tab.index()]
    }

    pub fn offset(&self) -> usize {
        self.offsets[self.tab.index()]
    }

    pub fn set_visible_rows(&mut self, rows: usize) {
        self.visible_rows = rows.max(1);
        self.ensure_visible();
    }

    pub fn set_tab(&mut self, tab: Tab) {
        self.tab = tab;
        self.detail_scroll = 0;
        self.clamp_selection();
        self.ensure_visible();
    }

    pub fn move_selection(&mut self, delta: isize) {
        let len = self.items_len();
        if len == 0 {
            return;
        }
        let index = self.tab.index();
        self.selected[index] = self.selected[index]
            .saturating_add_signed(delta)
            .min(len.saturating_sub(1));
        self.detail_scroll = 0;
        self.ensure_visible();
    }

    pub fn select(&mut self, selected: usize) {
        if selected < self.items_len() {
            self.selected[self.tab.index()] = selected;
            self.detail_scroll = 0;
            self.ensure_visible();
        }
    }

    pub fn select_home(&mut self) {
        self.selected[self.tab.index()] = 0;
        self.detail_scroll = 0;
        self.ensure_visible();
    }

    pub fn select_end(&mut self) {
        self.selected[self.tab.index()] = self.items_len().saturating_sub(1);
        self.detail_scroll = 0;
        self.ensure_visible();
    }

    fn clamp_selection(&mut self) {
        for (tab, index) in [(Tab::ReviewQueue, 0), (Tab::Involved, 1), (Tab::Owned, 2)] {
            let len = self.items_for(tab).len();
            self.selected[index] = self.selected[index].min(len.saturating_sub(1));
            self.offsets[index] = self.offsets[index].min(self.selected[index]);
        }
    }

    fn ensure_visible(&mut self) {
        let tab = self.tab.index();
        let selected = self.selected[tab];
        if selected < self.offsets[tab] {
            self.offsets[tab] = selected;
        } else if selected >= self.offsets[tab] + self.visible_rows {
            self.offsets[tab] = selected + 1 - self.visible_rows;
        }
    }

    pub fn selected_url(&self) -> Option<String> {
        self.items_for(self.tab)
            .get(self.selected_index())
            .map(|pull_request| pull_request.url.clone())
    }

    pub fn selected_branch(&self) -> Option<String> {
        self.items_for(self.tab)
            .get(self.selected_index())
            .map(|pull_request| pull_request.head_ref.clone())
    }

    pub fn selected_review_target(&self) -> Option<ReviewTarget> {
        self.items_for(self.tab)
            .get(self.selected_index())
            .map(ReviewTarget::from)
    }

    pub fn set_notice(&mut self, message: impl Into<String>) {
        self.notice = Some((message.into(), Instant::now() + Duration::from_secs(3)));
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'C'))
        {
            return Action::Quit;
        }
        if self.review_panel.is_some() {
            return self.handle_review_key(key);
        }
        if self.config_editor.is_some() {
            return self.handle_config_key(key);
        }

        if self.show_help {
            return match key.code {
                KeyCode::Esc | KeyCode::Char('?') => {
                    self.show_help = false;
                    Action::None
                }
                KeyCode::Char('q') => Action::Quit,
                _ => Action::None,
            };
        }

        if self.details_expanded && key.code == KeyCode::Esc {
            self.details_expanded = false;
            self.detail_scroll = 0;
            return Action::None;
        }

        if self.details_expanded {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.scroll_details(-1);
                    return Action::None;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.scroll_details(1);
                    return Action::None;
                }
                KeyCode::PageUp => {
                    self.scroll_details(-(self.visible_rows as isize));
                    return Action::None;
                }
                KeyCode::PageDown => {
                    self.scroll_details(self.visible_rows as isize);
                    return Action::None;
                }
                KeyCode::Home => {
                    self.detail_scroll = 0;
                    return Action::None;
                }
                KeyCode::End => {
                    self.detail_scroll = self.detail_max_scroll;
                    return Action::None;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Char('q') => Action::Quit,
            KeyCode::Char('?') => {
                self.show_help = true;
                Action::None
            }
            KeyCode::Char('t') => {
                self.open_config(None);
                Action::None
            }
            KeyCode::Char('R') => self
                .selected_review_target()
                .map(Action::OpenReview)
                .unwrap_or(Action::None),
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::SHIFT) => self
                .selected_review_target()
                .map(Action::OpenReview)
                .unwrap_or(Action::None),
            KeyCode::Char('C') => self
                .selected_url()
                .map(Action::CopyUrl)
                .unwrap_or(Action::None),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::SHIFT) => self
                .selected_url()
                .map(Action::CopyUrl)
                .unwrap_or(Action::None),
            KeyCode::Char('c') => self
                .selected_branch()
                .map(Action::CopyBranch)
                .unwrap_or(Action::None),
            KeyCode::Char('d') => {
                self.details_expanded = !self.details_expanded;
                self.detail_scroll = 0;
                Action::None
            }
            KeyCode::Tab | KeyCode::Right => {
                self.set_tab(self.tab.cycle(1));
                Action::None
            }
            KeyCode::BackTab | KeyCode::Left => {
                self.set_tab(self.tab.cycle(-1));
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1);
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1);
                Action::None
            }
            KeyCode::PageUp => {
                self.move_selection(-(self.visible_rows as isize));
                Action::None
            }
            KeyCode::PageDown => {
                self.move_selection(self.visible_rows as isize);
                Action::None
            }
            KeyCode::Home => {
                self.select_home();
                Action::None
            }
            KeyCode::End => {
                self.select_end();
                Action::None
            }
            KeyCode::Enter | KeyCode::Char('o') => self
                .selected_url()
                .map(Action::Open)
                .unwrap_or(Action::None),
            KeyCode::Char('r') => Action::Refresh,
            _ => Action::None,
        }
    }

    fn handle_review_key(&mut self, key: KeyEvent) -> Action {
        if self.playbook_editor.is_some() {
            return self.handle_playbook_key(key);
        }
        let mode = self
            .review_panel
            .as_ref()
            .expect("review panel exists")
            .mode;
        match mode {
            ReviewPanelMode::Prompt => match key.code {
                KeyCode::Char('p' | 'P') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.open_playbook_library();
                    Action::None
                }
                KeyCode::Char('s' | 'S') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.open_save_current_playbook();
                    Action::None
                }
                KeyCode::Esc => {
                    let panel = self.review_panel.as_mut().expect("review panel exists");
                    if panel.snapshot.has_session() || panel.snapshot.has_draft() {
                        panel.mode = ReviewPanelMode::Draft;
                        panel.input.clear();
                        panel.applied_playbook = None;
                        panel.error = None;
                    } else {
                        self.close_review_panel();
                    }
                    Action::None
                }
                KeyCode::Enter => {
                    let panel = self.review_panel.as_mut().expect("review panel exists");
                    if panel.run_kind == ReviewRunKind::FollowUp && panel.input.trim().is_empty() {
                        panel.error = Some(
                            "Describe what OpenCode should revisit in the saved review.".into(),
                        );
                        return Action::None;
                    }
                    let focus = (!panel.input.trim().is_empty()).then(|| panel.input.clone());
                    Action::LaunchReview {
                        target: panel.snapshot.target.clone(),
                        mode: LaunchMode::Review(panel.run_kind),
                        focus,
                    }
                }
                KeyCode::Backspace => {
                    self.review_panel
                        .as_mut()
                        .expect("review panel exists")
                        .input
                        .pop();
                    self.review_panel
                        .as_mut()
                        .expect("review panel exists")
                        .error = None;
                    Action::None
                }
                KeyCode::Delete => {
                    let panel = self.review_panel.as_mut().expect("review panel exists");
                    panel.input.clear();
                    panel.applied_playbook = None;
                    panel.error = None;
                    Action::None
                }
                KeyCode::Char(character)
                    if !key.modifiers.intersects(
                        KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                    ) =>
                {
                    self.review_panel
                        .as_mut()
                        .expect("review panel exists")
                        .input
                        .push(character);
                    self.review_panel
                        .as_mut()
                        .expect("review panel exists")
                        .error = None;
                    Action::None
                }
                _ => Action::None,
            },
            ReviewPanelMode::Running(phase) => match key.code {
                KeyCode::Esc => {
                    self.close_review_panel();
                    Action::None
                }
                KeyCode::Char('o') if phase == ReviewRunPhase::Reviewing => {
                    let panel = self.review_panel.as_ref().expect("review panel exists");
                    Action::LaunchReview {
                        target: panel.snapshot.target.clone(),
                        mode: LaunchMode::Chat,
                        focus: None,
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.scroll_review(-1);
                    Action::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.scroll_review(1);
                    Action::None
                }
                KeyCode::PageUp => {
                    self.scroll_review(-(self.visible_rows as isize));
                    Action::None
                }
                KeyCode::PageDown => {
                    self.scroll_review(self.visible_rows as isize);
                    Action::None
                }
                KeyCode::Home => {
                    self.review_panel
                        .as_mut()
                        .expect("review panel exists")
                        .scroll = 0;
                    Action::None
                }
                KeyCode::End => {
                    let panel = self.review_panel.as_mut().expect("review panel exists");
                    panel.scroll = panel.max_scroll;
                    Action::None
                }
                _ => Action::None,
            },
            ReviewPanelMode::Draft => match key.code {
                KeyCode::Esc => {
                    self.close_review_panel();
                    Action::None
                }
                KeyCode::Char('r') => {
                    let panel = self.review_panel.as_ref().expect("review panel exists");
                    Action::LaunchReview {
                        target: panel.snapshot.target.clone(),
                        mode: LaunchMode::Review(ReviewRunKind::ReReview),
                        focus: None,
                    }
                }
                KeyCode::Char('f')
                    if self.review_panel.as_ref().is_some_and(|panel| {
                        panel.snapshot.has_draft() && panel.snapshot.has_session()
                    }) =>
                {
                    let panel = self.review_panel.as_mut().expect("review panel exists");
                    panel.mode = ReviewPanelMode::Prompt;
                    panel.run_kind = ReviewRunKind::FollowUp;
                    panel.input.clear();
                    panel.applied_playbook = None;
                    panel.error = None;
                    Action::None
                }
                KeyCode::Char('e') => {
                    let panel = self.review_panel.as_mut().expect("review panel exists");
                    panel.mode = ReviewPanelMode::Prompt;
                    panel.run_kind = ReviewRunKind::ReReview;
                    panel.input.clear();
                    panel.applied_playbook = None;
                    panel.error = None;
                    Action::None
                }
                KeyCode::Char('n')
                    if self
                        .review_panel
                        .as_ref()
                        .is_some_and(|panel| panel.snapshot.has_session()) =>
                {
                    self.review_panel
                        .as_mut()
                        .expect("review panel exists")
                        .mode = ReviewPanelMode::ConfirmNewSession;
                    Action::None
                }
                KeyCode::Char('o') => {
                    let panel = self.review_panel.as_ref().expect("review panel exists");
                    if panel.snapshot.has_session() {
                        Action::LaunchReview {
                            target: panel.snapshot.target.clone(),
                            mode: LaunchMode::Chat,
                            focus: None,
                        }
                    } else {
                        Action::None
                    }
                }
                KeyCode::Char('p') => {
                    let panel = self.review_panel.as_mut().expect("review panel exists");
                    if panel.snapshot.has_draft() {
                        panel.mode = ReviewPanelMode::PostChoice;
                    }
                    Action::None
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.scroll_review(-1);
                    Action::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.scroll_review(1);
                    Action::None
                }
                KeyCode::PageUp => {
                    self.scroll_review(-(self.visible_rows as isize));
                    Action::None
                }
                KeyCode::PageDown => {
                    self.scroll_review(self.visible_rows as isize);
                    Action::None
                }
                KeyCode::Home => {
                    self.review_panel
                        .as_mut()
                        .expect("review panel exists")
                        .scroll = 0;
                    Action::None
                }
                KeyCode::End => {
                    let panel = self.review_panel.as_mut().expect("review panel exists");
                    panel.scroll = panel.max_scroll;
                    Action::None
                }
                _ => Action::None,
            },
            ReviewPanelMode::ConfirmNewSession => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => {
                    let panel = self.review_panel.as_mut().expect("review panel exists");
                    panel.mode = ReviewPanelMode::Prompt;
                    panel.run_kind = ReviewRunKind::NewSession;
                    panel.input.clear();
                    panel.applied_playbook = None;
                    panel.error = None;
                    Action::None
                }
                KeyCode::Char('n') | KeyCode::Esc => {
                    self.review_panel
                        .as_mut()
                        .expect("review panel exists")
                        .mode = ReviewPanelMode::Draft;
                    Action::None
                }
                _ => Action::None,
            },
            ReviewPanelMode::PostChoice => match key.code {
                KeyCode::Char('a') => {
                    self.review_panel
                        .as_mut()
                        .expect("review panel exists")
                        .mode = ReviewPanelMode::ConfirmPost(ReviewKind::Approve);
                    Action::None
                }
                KeyCode::Char('c') => {
                    self.review_panel
                        .as_mut()
                        .expect("review panel exists")
                        .mode = ReviewPanelMode::ConfirmPost(ReviewKind::Comment);
                    Action::None
                }
                KeyCode::Char('x') => {
                    self.review_panel
                        .as_mut()
                        .expect("review panel exists")
                        .mode = ReviewPanelMode::ConfirmPost(ReviewKind::RequestChanges);
                    Action::None
                }
                KeyCode::Esc => {
                    self.review_panel
                        .as_mut()
                        .expect("review panel exists")
                        .mode = ReviewPanelMode::Draft;
                    Action::None
                }
                _ => Action::None,
            },
            ReviewPanelMode::ConfirmPost(kind) => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => {
                    let snapshot = self
                        .review_panel
                        .as_ref()
                        .expect("review panel exists")
                        .snapshot
                        .clone();
                    Action::PostReview(snapshot, kind)
                }
                KeyCode::Char('n') | KeyCode::Esc => {
                    self.review_panel
                        .as_mut()
                        .expect("review panel exists")
                        .mode = ReviewPanelMode::Draft;
                    Action::None
                }
                _ => Action::None,
            },
            ReviewPanelMode::Error => match key.code {
                KeyCode::Esc => {
                    self.close_review_panel();
                    Action::None
                }
                KeyCode::Char('r') => {
                    let target = self
                        .review_panel
                        .as_ref()
                        .expect("review panel exists")
                        .snapshot
                        .target
                        .clone();
                    Action::OpenReview(target)
                }
                _ => Action::None,
            },
        }
    }

    fn open_playbook_library(&mut self) {
        let selected = self
            .review_panel
            .as_ref()
            .and_then(|panel| panel.applied_playbook.as_deref())
            .and_then(|selected| {
                self.playbook_catalog()
                    .iter()
                    .position(|playbook| playbook.name.eq_ignore_ascii_case(selected))
            })
            .unwrap_or(0);
        self.playbook_editor = Some(PlaybookEditor::library(selected));
        self.playbook_hitboxes = PlaybookHitboxes::default();
    }

    fn open_save_current_playbook(&mut self) {
        let prompt = self
            .review_panel
            .as_ref()
            .expect("review panel exists")
            .input
            .trim()
            .to_string();
        if prompt.is_empty() {
            self.review_panel
                .as_mut()
                .expect("review panel exists")
                .error = Some("Type review instructions before saving a playbook.".into());
            return;
        }
        self.playbook_editor = Some(PlaybookEditor {
            mode: PlaybookEditorMode::Name,
            selected: 0,
            name_input: String::new(),
            prompt_input: prompt,
            original_custom_name: None,
            error: None,
        });
        self.playbook_hitboxes = PlaybookHitboxes::default();
    }

    fn handle_playbook_key(&mut self, key: KeyEvent) -> Action {
        let mode = self
            .playbook_editor
            .as_ref()
            .expect("playbook editor exists")
            .mode;
        match mode {
            PlaybookEditorMode::Library => match key.code {
                KeyCode::Esc => {
                    self.playbook_editor = None;
                    Action::None
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.move_playbook_selection(-1);
                    Action::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.move_playbook_selection(1);
                    Action::None
                }
                KeyCode::Home => {
                    self.playbook_editor.as_mut().unwrap().selected = 0;
                    Action::None
                }
                KeyCode::End => {
                    self.playbook_editor.as_mut().unwrap().selected =
                        self.playbook_catalog().len().saturating_sub(1);
                    Action::None
                }
                KeyCode::Enter => {
                    self.apply_selected_playbook();
                    Action::None
                }
                KeyCode::Char('n') => {
                    self.playbook_editor = Some(PlaybookEditor {
                        mode: PlaybookEditorMode::Name,
                        selected: 0,
                        name_input: String::new(),
                        prompt_input: String::new(),
                        original_custom_name: None,
                        error: None,
                    });
                    Action::None
                }
                KeyCode::Char('e') => {
                    self.edit_selected_playbook();
                    Action::None
                }
                KeyCode::Char('d') => {
                    let selected = self.selected_playbook();
                    let editor = self.playbook_editor.as_mut().unwrap();
                    if selected.as_ref().is_some_and(|playbook| playbook.built_in) {
                        editor.error = Some(
                            "Built-in playbooks cannot be deleted; edit one to duplicate it."
                                .into(),
                        );
                    } else if selected.is_some() {
                        editor.mode = PlaybookEditorMode::ConfirmDelete;
                        editor.error = None;
                    }
                    Action::None
                }
                _ => Action::None,
            },
            PlaybookEditorMode::Name => match key.code {
                KeyCode::Esc => {
                    let selected = self.playbook_editor.as_ref().unwrap().selected;
                    self.playbook_editor = Some(PlaybookEditor::library(selected));
                    Action::None
                }
                KeyCode::Enter => {
                    self.continue_playbook_name();
                    Action::None
                }
                KeyCode::Backspace => {
                    let editor = self.playbook_editor.as_mut().unwrap();
                    editor.name_input.pop();
                    editor.error = None;
                    Action::None
                }
                KeyCode::Delete => {
                    let editor = self.playbook_editor.as_mut().unwrap();
                    editor.name_input.clear();
                    editor.error = None;
                    Action::None
                }
                KeyCode::Char(character)
                    if !key.modifiers.intersects(
                        KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                    ) =>
                {
                    let editor = self.playbook_editor.as_mut().unwrap();
                    editor.name_input.push(character);
                    editor.error = None;
                    Action::None
                }
                _ => Action::None,
            },
            PlaybookEditorMode::Body => match key.code {
                KeyCode::Char('s' | 'S') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.save_playbook_action()
                }
                KeyCode::Esc => {
                    let selected = self.playbook_editor.as_ref().unwrap().selected;
                    self.playbook_editor = Some(PlaybookEditor::library(selected));
                    Action::None
                }
                KeyCode::Enter => {
                    let editor = self.playbook_editor.as_mut().unwrap();
                    editor.prompt_input.push('\n');
                    editor.error = None;
                    Action::None
                }
                KeyCode::Backspace => {
                    let editor = self.playbook_editor.as_mut().unwrap();
                    editor.prompt_input.pop();
                    editor.error = None;
                    Action::None
                }
                KeyCode::Delete => {
                    let editor = self.playbook_editor.as_mut().unwrap();
                    editor.prompt_input.clear();
                    editor.error = None;
                    Action::None
                }
                KeyCode::Char(character)
                    if !key.modifiers.intersects(
                        KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                    ) =>
                {
                    let editor = self.playbook_editor.as_mut().unwrap();
                    editor.prompt_input.push(character);
                    editor.error = None;
                    Action::None
                }
                _ => Action::None,
            },
            PlaybookEditorMode::ConfirmDelete => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => self.delete_playbook_action(),
                KeyCode::Char('n') | KeyCode::Esc => {
                    let editor = self.playbook_editor.as_mut().unwrap();
                    editor.mode = PlaybookEditorMode::Library;
                    editor.error = None;
                    Action::None
                }
                _ => Action::None,
            },
        }
    }

    fn selected_playbook(&self) -> Option<ReviewPlaybook> {
        let selected = self.playbook_editor.as_ref()?.selected;
        self.playbook_catalog().get(selected).cloned()
    }

    fn move_playbook_selection(&mut self, delta: isize) {
        let len = self.playbook_catalog().len();
        let editor = self.playbook_editor.as_mut().unwrap();
        if len == 0 {
            editor.selected = 0;
            return;
        }
        editor.selected = (editor.selected as isize + delta).rem_euclid(len as isize) as usize;
        editor.error = None;
    }

    fn apply_selected_playbook(&mut self) {
        let Some(playbook) = self.selected_playbook() else {
            return;
        };
        let panel = self.review_panel.as_mut().expect("review panel exists");
        panel.input = playbook.prompt;
        panel.applied_playbook = Some(playbook.name);
        panel.error = None;
        self.playbook_editor = None;
    }

    fn edit_selected_playbook(&mut self) {
        let Some(playbook) = self.selected_playbook() else {
            return;
        };
        let selected = self.playbook_editor.as_ref().unwrap().selected;
        self.playbook_editor = Some(PlaybookEditor {
            mode: PlaybookEditorMode::Name,
            selected,
            name_input: if playbook.built_in {
                format!("{} copy", playbook.name)
            } else {
                playbook.name.clone()
            },
            prompt_input: playbook.prompt,
            original_custom_name: (!playbook.built_in).then_some(playbook.name),
            error: None,
        });
    }

    fn continue_playbook_name(&mut self) {
        let (name, original) = {
            let editor = self.playbook_editor.as_ref().unwrap();
            (
                editor.name_input.trim().to_string(),
                editor.original_custom_name.clone(),
            )
        };
        if let Err(error) = playbook::validate_name(&name) {
            self.playbook_editor.as_mut().unwrap().error = Some(error.to_string());
            return;
        }
        let duplicate = self.playbook_catalog().into_iter().any(|playbook| {
            playbook.name.eq_ignore_ascii_case(&name)
                && original
                    .as_deref()
                    .is_none_or(|original| !playbook.name.eq_ignore_ascii_case(original))
        });
        if duplicate {
            self.playbook_editor.as_mut().unwrap().error =
                Some("A playbook with that name already exists.".into());
            return;
        }
        let editor = self.playbook_editor.as_mut().unwrap();
        editor.name_input = name;
        editor.mode = PlaybookEditorMode::Body;
        editor.error = None;
    }

    fn save_playbook_action(&mut self) -> Action {
        let editor = self.playbook_editor.as_ref().unwrap();
        let playbook = match ReviewPlaybook::custom(&editor.name_input, &editor.prompt_input) {
            Ok(playbook) => playbook,
            Err(error) => {
                self.playbook_editor.as_mut().unwrap().error = Some(error.to_string());
                return Action::None;
            }
        };
        let original = editor.original_custom_name.clone();
        let mut playbooks = self.custom_playbooks.clone();
        if let Some(original) = original {
            let Some(index) = playbooks
                .iter()
                .position(|existing| existing.name.eq_ignore_ascii_case(&original))
            else {
                self.playbook_editor.as_mut().unwrap().error =
                    Some("The playbook being edited no longer exists.".into());
                return Action::None;
            };
            playbooks[index] = playbook.clone();
        } else {
            playbooks.push(playbook.clone());
        }
        if let Err(error) = playbook::validate_custom_playbooks(&playbooks) {
            self.playbook_editor.as_mut().unwrap().error = Some(error.to_string());
            return Action::None;
        }
        Action::SavePlaybooks {
            playbooks,
            selected_name: Some(playbook.name.clone()),
            notice: format!("Saved review playbook: {}", playbook.name),
        }
    }

    fn delete_playbook_action(&mut self) -> Action {
        let Some(selected) = self.selected_playbook() else {
            return Action::None;
        };
        if selected.built_in {
            self.playbook_editor.as_mut().unwrap().error =
                Some("Built-in playbooks cannot be deleted.".into());
            return Action::None;
        }
        let mut playbooks = self.custom_playbooks.clone();
        playbooks.retain(|playbook| !playbook.name.eq_ignore_ascii_case(&selected.name));
        Action::SavePlaybooks {
            playbooks,
            selected_name: None,
            notice: format!("Deleted review playbook: {}", selected.name),
        }
    }

    fn scroll_review(&mut self, delta: isize) {
        let panel = self.review_panel.as_mut().expect("review panel exists");
        let distance = delta.unsigned_abs().min(u16::MAX as usize) as u16;
        panel.scroll = if delta.is_negative() {
            panel.scroll.saturating_sub(distance)
        } else {
            panel.scroll.saturating_add(distance)
        }
        .min(panel.max_scroll);
    }

    fn handle_config_key(&mut self, key: KeyEvent) -> Action {
        let editor = self.config_editor.as_mut().expect("editor exists");
        if editor.confirm_reset {
            return match key.code {
                KeyCode::Char('y') | KeyCode::Char('x') | KeyCode::Enter => Action::ResetConfig,
                KeyCode::Esc | KeyCode::Char('n') => {
                    editor.confirm_reset = false;
                    editor.error = None;
                    Action::None
                }
                _ => Action::None,
            };
        }

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.config_editor = None;
                Action::None
            }
            KeyCode::Tab | KeyCode::Down => {
                editor.focus = editor.focus.cycle(1);
                Action::None
            }
            KeyCode::BackTab | KeyCode::Up => {
                editor.focus = editor.focus.cycle(-1);
                Action::None
            }
            KeyCode::Char('s') => self.config_save_action(),
            KeyCode::Char('x') => {
                editor.confirm_reset = true;
                editor.error = None;
                Action::None
            }
            KeyCode::Enter => match editor.focus {
                ConfigFocus::Reset => {
                    editor.confirm_reset = true;
                    Action::None
                }
                ConfigFocus::Cancel => {
                    self.config_editor = None;
                    Action::None
                }
                _ => self.config_save_action(),
            },
            KeyCode::Char(character)
                if editor.focus == ConfigFocus::Refresh && character.is_ascii_digit() =>
            {
                if editor.refresh_pristine {
                    editor.refresh_input.clear();
                    editor.refresh_pristine = false;
                }
                editor.refresh_input.push(character);
                editor.error = None;
                Action::None
            }
            KeyCode::Backspace if editor.focus == ConfigFocus::Refresh => {
                editor.refresh_pristine = false;
                editor.refresh_input.pop();
                editor.error = None;
                Action::None
            }
            KeyCode::Delete if editor.focus == ConfigFocus::Refresh => {
                editor.refresh_pristine = false;
                editor.refresh_input.clear();
                editor.error = None;
                Action::None
            }
            _ => Action::None,
        }
    }

    fn config_save_action(&mut self) -> Action {
        let editor = self.config_editor.as_mut().expect("editor exists");
        match editor.config() {
            Ok(config) => Action::SaveConfig(config),
            Err(error) => {
                editor.error = Some(error);
                Action::None
            }
        }
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> Action {
        if self.show_help {
            return Action::None;
        }
        if self.playbook_editor.is_some() {
            return self.handle_playbook_mouse(mouse);
        }
        if self.review_panel.is_some() {
            match mouse.kind {
                MouseEventKind::ScrollUp => self.scroll_review(-3),
                MouseEventKind::ScrollDown => self.scroll_review(3),
                _ => {}
            }
            return Action::None;
        }
        if self.config_editor.is_some() {
            return self.handle_config_mouse(mouse);
        }

        match mouse.kind {
            MouseEventKind::ScrollUp => {
                if self.details_expanded {
                    self.scroll_details(-3);
                } else {
                    self.move_selection(-3);
                }
                Action::None
            }
            MouseEventKind::ScrollDown => {
                if self.details_expanded {
                    self.scroll_details(3);
                } else {
                    self.move_selection(3);
                }
                Action::None
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let x = mouse.column;
                let y = mouse.row;
                if contains(self.tab_hitboxes[0], x, y) {
                    self.set_tab(Tab::ReviewQueue);
                    return Action::None;
                }
                if contains(self.tab_hitboxes[1], x, y) {
                    self.set_tab(Tab::Involved);
                    return Action::None;
                }
                if contains(self.tab_hitboxes[2], x, y) {
                    self.set_tab(Tab::Owned);
                    return Action::None;
                }
                if let Some(hitbox) = self
                    .row_hitboxes
                    .iter()
                    .find(|hitbox| contains(hitbox.rect, x, y))
                    .cloned()
                {
                    self.select(hitbox.index);
                    return self
                        .selected_url()
                        .map(Action::Open)
                        .unwrap_or(Action::None);
                }
                Action::None
            }
            _ => Action::None,
        }
    }

    fn handle_playbook_mouse(&mut self, mouse: MouseEvent) -> Action {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                if self
                    .playbook_editor
                    .as_ref()
                    .is_some_and(|editor| editor.mode == PlaybookEditorMode::Library)
                {
                    self.move_playbook_selection(-1);
                }
                Action::None
            }
            MouseEventKind::ScrollDown => {
                if self
                    .playbook_editor
                    .as_ref()
                    .is_some_and(|editor| editor.mode == PlaybookEditorMode::Library)
                {
                    self.move_playbook_selection(1);
                }
                Action::None
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let x = mouse.column;
                let y = mouse.row;
                let hitboxes = self.playbook_hitboxes.clone();
                if let Some(hitbox) = hitboxes
                    .rows
                    .iter()
                    .find(|hitbox| contains(hitbox.rect, x, y))
                {
                    if let Some(editor) = &mut self.playbook_editor {
                        editor.selected = hitbox.index;
                        editor.error = None;
                    }
                    return Action::None;
                }
                let mode = self.playbook_editor.as_ref().unwrap().mode;
                if contains(hitboxes.primary, x, y) {
                    return self.handle_playbook_key(match mode {
                        PlaybookEditorMode::Body => {
                            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)
                        }
                        _ => KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                    });
                }
                if contains(hitboxes.new, x, y) {
                    return self.handle_playbook_key(KeyEvent::new(
                        KeyCode::Char('n'),
                        KeyModifiers::NONE,
                    ));
                }
                if contains(hitboxes.edit, x, y) {
                    return self.handle_playbook_key(KeyEvent::new(
                        KeyCode::Char('e'),
                        KeyModifiers::NONE,
                    ));
                }
                if contains(hitboxes.delete, x, y) {
                    return self.handle_playbook_key(KeyEvent::new(
                        KeyCode::Char('d'),
                        KeyModifiers::NONE,
                    ));
                }
                if contains(hitboxes.back, x, y) {
                    return self
                        .handle_playbook_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
                }
                Action::None
            }
            _ => Action::None,
        }
    }

    fn handle_config_mouse(&mut self, mouse: MouseEvent) -> Action {
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return Action::None;
        }
        let x = mouse.column;
        let y = mouse.row;
        let hitboxes = self.config_hitboxes.clone();
        let editor = self.config_editor.as_mut().expect("editor exists");
        if editor.confirm_reset {
            if contains(hitboxes.reset, x, y) {
                return Action::ResetConfig;
            }
            if contains(hitboxes.cancel, x, y) {
                editor.confirm_reset = false;
                editor.error = None;
            }
            return Action::None;
        }
        if contains(hitboxes.refresh, x, y) {
            editor.focus = ConfigFocus::Refresh;
            return Action::None;
        }
        if contains(hitboxes.save, x, y) {
            editor.focus = ConfigFocus::Save;
            return self.config_save_action();
        }
        if contains(hitboxes.reset, x, y) {
            editor.focus = ConfigFocus::Reset;
            editor.confirm_reset = true;
            return Action::None;
        }
        if contains(hitboxes.cancel, x, y) {
            self.config_editor = None;
        }
        Action::None
    }

    fn scroll_details(&mut self, delta: isize) {
        let distance = delta.unsigned_abs().min(u16::MAX as usize) as u16;
        self.detail_scroll = if delta.is_negative() {
            self.detail_scroll.saturating_sub(distance)
        } else {
            self.detail_scroll.saturating_add(distance)
        }
        .min(self.detail_max_scroll);
    }
}

fn merge_commit_involvement(
    data: &mut DashboardData,
    committed: &[PullRequest],
    warnings: &[String],
) {
    for committed_pr in committed {
        if let Some(existing) = data
            .involved
            .iter_mut()
            .find(|pull_request| pull_request.url == committed_pr.url)
        {
            existing.add_involvement(InvolvementReason::Committed);
        } else {
            data.involved.push(committed_pr.clone());
        }
    }
    data.involved
        .sort_by_key(|pull_request| std::cmp::Reverse(pull_request.updated_at));
    for warning in warnings {
        if !data.warnings.contains(warning) {
            data.warnings.push(warning.clone());
        }
    }
}

fn strip_commit_involvement(data: &mut DashboardData, previously_committed: &[PullRequest]) {
    let urls = previously_committed
        .iter()
        .map(|pull_request| pull_request.url.as_str())
        .collect::<std::collections::HashSet<_>>();
    for pull_request in &mut data.involved {
        if urls.contains(pull_request.url.as_str()) {
            pull_request
                .involvement
                .retain(|reason| *reason != InvolvementReason::Committed);
        }
    }
    data.involved
        .retain(|pull_request| !pull_request.involvement.is_empty());
}

struct SelectionAnchor {
    url: Option<String>,
    row: usize,
}

struct RefreshMessage {
    result: Result<DashboardData, String>,
}

fn contains(rect: Rect, x: u16, y: u16) -> bool {
    x >= rect.x
        && x < rect.x.saturating_add(rect.width)
        && y >= rect.y
        && y < rect.y.saturating_add(rect.height)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use crate::model::{CheckState, MergeableState, PullRequest};

    use super::*;

    fn pr(number: u64) -> PullRequest {
        PullRequest {
            number,
            title: format!("PR {number}"),
            url: format!("https://github.com/acme/app/pull/{number}"),
            repository: "acme/app".into(),
            author: "alice".into(),
            is_draft: false,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            additions: 1,
            deletions: 1,
            changed_files: 1,
            base_ref: "main".into(),
            head_ref: "feature".into(),
            mergeable: MergeableState::Mergeable,
            review_decision: None,
            reviews: vec![],
            total_review_events: 0,
            review_requests: vec![],
            total_review_requests: 0,
            comments: 0,
            labels: vec![],
            checks: Some(CheckState::Success),
            requested_via: vec!["@viewer".into()],
            involvement: vec![],
        }
    }

    fn dashboard(
        review_queue: Vec<PullRequest>,
        involved: Vec<PullRequest>,
        owned: Vec<PullRequest>,
    ) -> DashboardData {
        DashboardData {
            viewer: "viewer".into(),
            review_queue,
            involved,
            owned,
            warnings: vec![],
            fetched_at: Utc::now(),
            teams: vec!["acme/core".into()],
        }
    }

    fn app() -> App {
        App::with_data(dashboard(
            vec![pr(1), pr(2), pr(3), pr(4)],
            vec![pr(6), pr(7)],
            vec![pr(5)],
        ))
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn shifted_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    fn control_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    #[test]
    fn keyboard_navigation_switches_tabs_scrolls_and_opens() {
        let mut app = app();
        app.set_visible_rows(2);
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.selected_index(), 2);
        assert_eq!(app.offset(), 1);

        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Action::Open("https://github.com/acme/app/pull/3".into())
        );

        app.handle_key(key(KeyCode::Char('2')));
        assert_eq!(app.tab, Tab::ReviewQueue);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.tab, Tab::Involved);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.tab, Tab::Owned);
        assert_eq!(app.items_len(), 1);
    }

    #[test]
    fn a_single_row_click_selects_and_opens() {
        let mut app = app();
        app.row_hitboxes = vec![RowHitbox {
            rect: Rect::new(2, 5, 40, 2),
            index: 2,
        }];
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 10,
            row: 6,
            modifiers: KeyModifiers::NONE,
        };

        assert_eq!(
            app.handle_mouse(mouse),
            Action::Open("https://github.com/acme/app/pull/3".into())
        );
        assert_eq!(app.selected_index(), 2);
    }

    #[test]
    fn details_can_be_expanded_and_scrolled() {
        let mut app = app();
        app.handle_key(key(KeyCode::Char('d')));
        assert!(app.details_expanded);

        app.detail_max_scroll = 20;
        app.handle_key(key(KeyCode::PageDown));
        assert!(app.detail_scroll > 0);

        app.handle_key(key(KeyCode::Esc));
        assert!(!app.details_expanded);
        assert_eq!(app.detail_scroll, 0);
    }

    #[test]
    fn refresh_preserves_the_selected_pr_and_its_screen_row() {
        let mut app = app();
        app.set_visible_rows(2);
        app.select(2);
        assert_eq!(app.offset(), 1);
        let selected = app.selected_url();

        app.apply_refresh(dashboard(
            vec![pr(4), pr(3), pr(2), pr(1)],
            vec![pr(7), pr(6)],
            vec![pr(5)],
        ));

        assert_eq!(app.selected_url(), selected);
        assert_eq!(app.selected_index() - app.offset(), 1);
    }

    #[test]
    fn commit_enrichment_merges_reasons_without_duplicate_rows() {
        let mut commented = pr(6);
        commented.involvement = vec![InvolvementReason::Commented];
        let mut data = dashboard(vec![], vec![commented], vec![]);
        let mut committed_existing = pr(6);
        committed_existing.involvement = vec![InvolvementReason::Committed];
        let mut committed_only = pr(8);
        committed_only.involvement = vec![InvolvementReason::Committed];

        merge_commit_involvement(
            &mut data,
            &[committed_existing, committed_only],
            &["commit scan note".into()],
        );

        assert_eq!(data.involved.len(), 2);
        let existing = data
            .involved
            .iter()
            .find(|pull_request| pull_request.number == 6)
            .unwrap();
        assert_eq!(
            existing.involvement,
            vec![InvolvementReason::Committed, InvolvementReason::Commented]
        );
        assert_eq!(data.warnings, vec!["commit scan note"]);
    }

    #[test]
    fn configuration_editor_validates_and_builds_actions() {
        let mut app = app();
        app.handle_key(key(KeyCode::Char('t')));
        let editor = app.config_editor.as_mut().unwrap();
        editor.refresh_input = "4".into();
        assert_eq!(app.handle_key(key(KeyCode::Char('s'))), Action::None);
        assert!(app.config_editor.as_ref().unwrap().error.is_some());

        app.config_editor.as_mut().unwrap().refresh_input = "60".into();
        assert_eq!(
            app.handle_key(key(KeyCode::Char('s'))),
            Action::SaveConfig(Config::new(60).unwrap())
        );
    }

    #[test]
    fn reset_requires_confirmation() {
        let mut app = app();
        app.handle_key(key(KeyCode::Char('t')));
        assert_eq!(app.handle_key(key(KeyCode::Char('x'))), Action::None);
        assert!(app.config_editor.as_ref().unwrap().confirm_reset);
        assert_eq!(app.handle_key(key(KeyCode::Char('y'))), Action::ResetConfig);
    }

    #[test]
    fn branch_url_and_timer_shortcuts_are_unambiguous() {
        let mut app = app();

        assert_eq!(
            app.handle_key(key(KeyCode::Char('c'))),
            Action::CopyBranch("feature".into())
        );
        assert!(app.config_editor.is_none());

        assert_eq!(
            app.handle_key(shifted_key(KeyCode::Char('C'))),
            Action::CopyUrl("https://github.com/acme/app/pull/1".into())
        );
        assert_eq!(
            app.handle_key(shifted_key(KeyCode::Char('c'))),
            Action::CopyUrl("https://github.com/acme/app/pull/1".into())
        );
        assert_eq!(
            app.handle_key(key(KeyCode::Char('C'))),
            Action::CopyUrl("https://github.com/acme/app/pull/1".into())
        );

        assert_eq!(app.handle_key(key(KeyCode::Char('t'))), Action::None);
        assert!(app.config_editor.is_some());
    }

    fn review_snapshot(session: bool, draft: bool) -> ReviewSnapshot {
        review_snapshot_for(1, session, draft)
    }

    fn review_snapshot_for(number: u64, session: bool, draft: bool) -> ReviewSnapshot {
        ReviewSnapshot {
            target: ReviewTarget::from(&pr(number)),
            session_id: session.then(|| format!("ses_review_{number}")),
            draft: draft.then(|| "# Summary\n\nReady to review.\n".into()),
            draft_path: PathBuf::from(format!("/tmp/kritikon-review-{number}.md")),
            workspace: PathBuf::from(format!("/tmp/kritikon-review-workspace-{number}")),
            warning: None,
        }
    }

    #[test]
    fn shift_r_opens_agent_review_without_stealing_lowercase_refresh() {
        let mut app = app();
        assert_eq!(app.handle_key(key(KeyCode::Char('r'))), Action::Refresh);
        assert_eq!(
            app.handle_key(shifted_key(KeyCode::Char('R'))),
            Action::OpenReview(ReviewTarget::from(&pr(1)))
        );
        assert_eq!(
            app.handle_key(shifted_key(KeyCode::Char('r'))),
            Action::OpenReview(ReviewTarget::from(&pr(1)))
        );
    }

    #[test]
    fn new_review_accepts_optional_focus_and_launches_template() {
        let mut app = app();
        let snapshot = review_snapshot(false, false);
        app.show_review_snapshot(snapshot.clone());
        assert_eq!(
            app.review_panel.as_ref().unwrap().mode,
            ReviewPanelMode::Prompt
        );

        for character in "race conditions".chars() {
            assert_eq!(app.handle_key(key(KeyCode::Char(character))), Action::None);
        }
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Action::LaunchReview {
                target: snapshot.target,
                mode: LaunchMode::Review(ReviewRunKind::ReReview),
                focus: Some("race conditions".into()),
            }
        );
    }

    #[test]
    fn review_playbooks_are_selected_as_editable_focus_without_auto_launching() {
        let mut app = app();
        let snapshot = review_snapshot(false, false);
        app.show_review_snapshot(snapshot.clone());

        assert_eq!(
            app.handle_key(control_key(KeyCode::Char('p'))),
            Action::None
        );
        assert_eq!(
            app.playbook_editor.as_ref().unwrap().mode,
            PlaybookEditorMode::Library
        );
        assert_eq!(app.playbook_catalog().len(), 4);
        assert_eq!(app.handle_key(key(KeyCode::Down)), Action::None);
        let selected = app.playbook_catalog()[1].clone();
        assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::None);
        assert!(app.playbook_editor.is_none());
        let panel = app.review_panel.as_ref().unwrap();
        assert_eq!(panel.input, selected.prompt);
        assert_eq!(
            panel.applied_playbook.as_deref(),
            Some(selected.name.as_str())
        );

        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Action::LaunchReview {
                target: snapshot.target,
                mode: LaunchMode::Review(ReviewRunKind::ReReview),
                focus: Some(selected.prompt),
            }
        );
    }

    #[test]
    fn custom_playbooks_can_be_named_saved_reused_and_deleted() {
        let mut app = app();
        app.show_review_snapshot(review_snapshot(false, false));
        for character in "Check feature flags and rollback safety.".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        assert_eq!(
            app.handle_key(control_key(KeyCode::Char('s'))),
            Action::None
        );
        assert_eq!(
            app.playbook_editor.as_ref().unwrap().mode,
            PlaybookEditorMode::Name
        );
        for character in "Release safety".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::None);
        assert_eq!(
            app.playbook_editor.as_ref().unwrap().mode,
            PlaybookEditorMode::Body
        );
        app.handle_key(key(KeyCode::Enter));
        for character in "Check mixed-version deployment behavior.".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        let expected_prompt =
            "Check feature flags and rollback safety.\nCheck mixed-version deployment behavior.";
        let (playbooks, selected_name, notice) =
            match app.handle_key(control_key(KeyCode::Char('s'))) {
                Action::SavePlaybooks {
                    playbooks,
                    selected_name,
                    notice,
                } => (playbooks, selected_name, notice),
                action => panic!("expected playbook save, got {action:?}"),
            };
        assert_eq!(playbooks.len(), 1);
        assert_eq!(playbooks[0].name, "Release safety");
        assert_eq!(playbooks[0].prompt, expected_prompt);
        app.playbooks_saved(playbooks, selected_name, notice);
        assert_eq!(app.playbook_catalog().len(), 5);
        assert_eq!(app.playbook_editor.as_ref().unwrap().selected, 4);

        assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::None);
        assert_eq!(app.review_panel.as_ref().unwrap().input, expected_prompt);

        app.handle_key(control_key(KeyCode::Char('p')));
        app.handle_key(key(KeyCode::End));
        assert_eq!(app.handle_key(key(KeyCode::Char('d'))), Action::None);
        assert_eq!(
            app.playbook_editor.as_ref().unwrap().mode,
            PlaybookEditorMode::ConfirmDelete
        );
        let (playbooks, selected_name, notice) = match app.handle_key(key(KeyCode::Enter)) {
            Action::SavePlaybooks {
                playbooks,
                selected_name,
                notice,
            } => (playbooks, selected_name, notice),
            action => panic!("expected playbook deletion, got {action:?}"),
        };
        assert!(playbooks.is_empty());
        app.playbooks_saved(playbooks, selected_name, notice);
        assert_eq!(app.playbook_catalog().len(), 4);
    }

    #[test]
    fn playbook_validation_and_built_in_protection_are_visible_in_the_flow() {
        let mut app = app();
        app.show_review_snapshot(review_snapshot(false, false));
        assert_eq!(
            app.handle_key(control_key(KeyCode::Char('s'))),
            Action::None
        );
        assert!(
            app.review_panel
                .as_ref()
                .unwrap()
                .error
                .as_deref()
                .is_some_and(|error| error.contains("Type review instructions"))
        );

        app.handle_key(control_key(KeyCode::Char('p')));
        app.handle_key(key(KeyCode::Char('d')));
        let editor = app.playbook_editor.as_ref().unwrap();
        assert_eq!(editor.mode, PlaybookEditorMode::Library);
        assert!(
            editor
                .error
                .as_deref()
                .is_some_and(|error| error.contains("cannot be deleted"))
        );
    }

    #[test]
    fn saved_review_can_chat_rerun_and_requires_post_confirmation() {
        let mut app = app();
        let snapshot = review_snapshot(true, true);
        app.show_review_snapshot(snapshot.clone());
        assert_eq!(
            app.review_panel.as_ref().unwrap().mode,
            ReviewPanelMode::Draft
        );
        assert_eq!(
            app.handle_key(key(KeyCode::Char('o'))),
            Action::LaunchReview {
                target: snapshot.target.clone(),
                mode: LaunchMode::Chat,
                focus: None,
            }
        );
        assert_eq!(
            app.handle_key(key(KeyCode::Char('r'))),
            Action::LaunchReview {
                target: snapshot.target.clone(),
                mode: LaunchMode::Review(ReviewRunKind::ReReview),
                focus: None,
            }
        );

        assert_eq!(app.handle_key(key(KeyCode::Char('p'))), Action::None);
        assert_eq!(
            app.review_panel.as_ref().unwrap().mode,
            ReviewPanelMode::PostChoice
        );
        assert_eq!(app.handle_key(key(KeyCode::Char('c'))), Action::None);
        assert_eq!(
            app.review_panel.as_ref().unwrap().mode,
            ReviewPanelMode::ConfirmPost(ReviewKind::Comment)
        );
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Action::PostReview(snapshot, ReviewKind::Comment)
        );
    }

    #[test]
    fn saved_review_separates_follow_up_rereview_and_new_session_flows() {
        let mut app = app();
        let snapshot = review_snapshot(true, true);
        app.show_review_snapshot(snapshot.clone());

        assert_eq!(app.handle_key(key(KeyCode::Char('f'))), Action::None);
        let panel = app.review_panel.as_ref().unwrap();
        assert_eq!(panel.mode, ReviewPanelMode::Prompt);
        assert_eq!(panel.run_kind, ReviewRunKind::FollowUp);
        assert_eq!(panel.snapshot.draft, snapshot.draft);

        assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::None);
        assert!(
            app.review_panel
                .as_ref()
                .unwrap()
                .error
                .as_deref()
                .is_some_and(|error| error.contains("what OpenCode should revisit"))
        );
        for character in "verify the cancellation finding".chars() {
            assert_eq!(app.handle_key(key(KeyCode::Char(character))), Action::None);
        }
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Action::LaunchReview {
                target: snapshot.target.clone(),
                mode: LaunchMode::Review(ReviewRunKind::FollowUp),
                focus: Some("verify the cancellation finding".into()),
            }
        );
        assert_eq!(app.handle_key(key(KeyCode::Esc)), Action::None);
        assert_eq!(
            app.review_panel.as_ref().unwrap().mode,
            ReviewPanelMode::Draft
        );
        assert_eq!(app.handle_key(key(KeyCode::Char('e'))), Action::None);
        assert_eq!(
            app.review_panel.as_ref().unwrap().run_kind,
            ReviewRunKind::ReReview
        );
        for character in "focus on auth boundaries".chars() {
            assert_eq!(app.handle_key(key(KeyCode::Char(character))), Action::None);
        }
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Action::LaunchReview {
                target: snapshot.target.clone(),
                mode: LaunchMode::Review(ReviewRunKind::ReReview),
                focus: Some("focus on auth boundaries".into()),
            }
        );
        assert_eq!(app.handle_key(key(KeyCode::Esc)), Action::None);

        assert_eq!(
            app.handle_key(key(KeyCode::Char('r'))),
            Action::LaunchReview {
                target: snapshot.target.clone(),
                mode: LaunchMode::Review(ReviewRunKind::ReReview),
                focus: None,
            }
        );

        assert_eq!(app.handle_key(key(KeyCode::Char('n'))), Action::None);
        assert_eq!(
            app.review_panel.as_ref().unwrap().mode,
            ReviewPanelMode::ConfirmNewSession
        );
        assert_eq!(app.handle_key(key(KeyCode::Esc)), Action::None);
        assert_eq!(
            app.review_panel.as_ref().unwrap().mode,
            ReviewPanelMode::Draft
        );
        assert_eq!(app.handle_key(key(KeyCode::Char('n'))), Action::None);
        assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::None);
        let panel = app.review_panel.as_ref().unwrap();
        assert_eq!(panel.mode, ReviewPanelMode::Prompt);
        assert_eq!(panel.run_kind, ReviewRunKind::NewSession);
        assert_eq!(panel.snapshot.draft, snapshot.draft);
        assert_eq!(panel.snapshot.session_id, snapshot.session_id);
        for character in "focus on authorization".chars() {
            assert_eq!(app.handle_key(key(KeyCode::Char(character))), Action::None);
        }
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Action::LaunchReview {
                target: snapshot.target.clone(),
                mode: LaunchMode::Review(ReviewRunKind::NewSession),
                focus: Some("focus on authorization".into()),
            }
        );

        assert_eq!(app.handle_key(key(KeyCode::Esc)), Action::None);
        assert_eq!(app.handle_key(key(KeyCode::Char('n'))), Action::None);
        assert_eq!(app.handle_key(key(KeyCode::Char('y'))), Action::None);
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Action::LaunchReview {
                target: snapshot.target,
                mode: LaunchMode::Review(ReviewRunKind::NewSession),
                focus: None,
            }
        );
    }

    #[test]
    fn failed_follow_up_restores_the_previous_draft_instead_of_an_error_only_panel() {
        let mut app = app();
        let snapshot = review_snapshot(true, true);
        let target_url = snapshot.target.url.clone();
        app.review_started(snapshot.clone());

        app.review_background_failed(snapshot.clone(), "replacement file was not written");

        let panel = app.review_panel.as_ref().unwrap();
        assert_eq!(panel.mode, ReviewPanelMode::Draft);
        assert_eq!(panel.snapshot.draft, snapshot.draft);
        assert!(
            panel
                .snapshot
                .warning
                .as_deref()
                .is_some_and(|warning| warning.contains("previous draft was preserved"))
        );
        assert_eq!(
            app.review_agent_state(&target_url),
            Some(ReviewAgentState::Draft)
        );
    }

    #[test]
    fn background_review_can_be_closed_reopened_attached_and_completed() {
        let mut app = app();
        let initial = review_snapshot(false, false);
        let target_url = initial.target.url.clone();
        app.review_started(initial);
        assert_eq!(app.active_review_count(), 1);
        assert_eq!(
            app.review_panel.as_ref().unwrap().mode,
            ReviewPanelMode::Running(ReviewRunPhase::Preparing)
        );
        assert_eq!(app.handle_key(key(KeyCode::Char('o'))), Action::None);

        assert_eq!(app.handle_key(key(KeyCode::Esc)), Action::None);
        assert!(app.review_panel.is_none());
        assert!(app.show_review_for_target(&target_url));

        let ready = review_snapshot(true, false);
        app.review_session_ready(ready.clone());
        assert_eq!(
            app.review_panel.as_ref().unwrap().mode,
            ReviewPanelMode::Running(ReviewRunPhase::Reviewing)
        );
        assert_eq!(
            app.handle_key(key(KeyCode::Char('o'))),
            Action::LaunchReview {
                target: ready.target.clone(),
                mode: LaunchMode::Chat,
                focus: None,
            }
        );

        let completed = review_snapshot(true, true);
        app.review_completed(completed.clone());
        assert_eq!(app.active_review_count(), 0);
        assert_eq!(
            app.review_panel.as_ref().unwrap().mode,
            ReviewPanelMode::Draft
        );
        assert_eq!(
            app.review_panel.as_ref().unwrap().snapshot.draft,
            completed.draft
        );
    }

    #[test]
    fn completed_session_without_markdown_is_not_reported_as_a_ready_draft() {
        let mut app = app();
        let initial = review_snapshot(false, false);
        let target_url = initial.target.url.clone();
        app.review_started(initial);
        app.close_review_panel();

        app.review_completed(review_snapshot(true, false));

        assert_eq!(app.active_review_count(), 0);
        assert_eq!(
            app.review_agent_state(&target_url),
            Some(ReviewAgentState::Session)
        );
        assert!(app.ready_review_labels().is_empty());
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|(notice, _)| notice.contains("finished without a review draft"))
        );

        assert!(app.show_review_for_target(&target_url));
        let panel = app.review_panel.as_ref().unwrap();
        assert_eq!(panel.mode, ReviewPanelMode::Draft);
        assert!(panel.snapshot.has_session());
        assert!(!panel.snapshot.has_draft());
        assert_eq!(
            app.review_agent_state(&target_url),
            Some(ReviewAgentState::Session)
        );
        assert_eq!(app.handle_key(key(KeyCode::Char('p'))), Action::None);
        assert_eq!(
            app.review_panel.as_ref().unwrap().mode,
            ReviewPanelMode::Draft
        );
    }

    #[test]
    fn concurrent_review_panels_and_failures_are_scoped_to_the_exact_pull_request() {
        let mut app = app();
        let first = review_snapshot_for(1, false, false);
        let second = review_snapshot_for(2, false, false);
        let first_url = first.target.url.clone();
        let second_url = second.target.url.clone();

        app.review_started(first);
        app.close_review_panel();
        app.review_started(second);
        app.close_review_panel();
        app.review_session_ready(review_snapshot_for(1, true, false));

        assert_eq!(
            app.review_agent_state(&first_url),
            Some(ReviewAgentState::Reviewing)
        );
        assert_eq!(
            app.review_agent_state(&second_url),
            Some(ReviewAgentState::Preparing)
        );

        assert!(app.show_review_for_target(&second_url));
        let panel = app.review_panel.as_ref().unwrap();
        assert_eq!(panel.snapshot.target.number, 2);
        assert_eq!(
            panel.mode,
            ReviewPanelMode::Running(ReviewRunPhase::Preparing)
        );
        app.close_review_panel();

        assert!(app.show_review_for_target(&first_url));
        let panel = app.review_panel.as_ref().unwrap();
        assert_eq!(panel.snapshot.target.number, 1);
        assert_eq!(
            panel.mode,
            ReviewPanelMode::Running(ReviewRunPhase::Reviewing)
        );

        app.review_background_failed(
            review_snapshot_for(2, false, false),
            "second PR failed independently",
        );
        assert_eq!(app.review_panel.as_ref().unwrap().snapshot.target.number, 1);
        assert_eq!(
            app.review_agent_state(&second_url),
            Some(ReviewAgentState::Failed)
        );

        app.close_review_panel();
        assert!(app.show_review_for_target(&second_url));
        let panel = app.review_panel.as_ref().unwrap();
        assert_eq!(panel.snapshot.target.number, 2);
        assert_eq!(panel.mode, ReviewPanelMode::Error);
        assert_eq!(
            panel.error.as_deref(),
            Some("second PR failed independently")
        );
    }

    #[test]
    fn unfinished_review_prompts_are_preserved_per_pull_request() {
        let mut app = app();
        let first = review_snapshot_for(1, false, false);
        let second = review_snapshot_for(2, false, false);
        let first_url = first.target.url.clone();
        let second_url = second.target.url.clone();

        app.show_review_snapshot(first);
        for character in "focus one".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        app.close_review_panel();

        app.show_review_snapshot(second);
        for character in "focus two".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        app.close_review_panel();

        assert!(app.show_review_for_target(&first_url));
        assert_eq!(app.review_panel.as_ref().unwrap().snapshot.target.number, 1);
        assert_eq!(app.review_panel.as_ref().unwrap().input, "focus one");
        app.close_review_panel();

        assert!(app.show_review_for_target(&second_url));
        assert_eq!(app.review_panel.as_ref().unwrap().snapshot.target.number, 2);
        assert_eq!(app.review_panel.as_ref().unwrap().input, "focus two");
    }

    #[test]
    fn completed_background_review_stays_marked_ready_until_opened() {
        let mut app = app();
        let initial = review_snapshot(false, false);
        app.review_started(initial);
        app.handle_key(key(KeyCode::Esc));

        let completed = review_snapshot(true, true);
        app.review_completed(completed.clone());
        assert!(app.review_panel.is_none());
        assert_eq!(app.ready_review_labels(), vec!["acme/app#1"]);

        app.show_review_snapshot(completed);
        assert!(app.ready_review_labels().is_empty());
    }

    #[test]
    fn ctrl_c_still_quits_with_uppercase_terminal_input() {
        let mut app = app();

        assert_eq!(
            app.handle_key(KeyEvent::new(
                KeyCode::Char('C'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )),
            Action::Quit
        );
    }

    #[test]
    fn maximum_u64_interval_is_schedulable_without_overflow() {
        let mut app = app();
        app.apply_config(Config::new(u64::MAX).unwrap(), false);
        assert_eq!(app.refresh_remaining.as_secs(), u64::MAX);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn refresh_work_starts_in_the_background() {
        let mut app = App::new(
            Config::new(5).unwrap(),
            PathBuf::from("/tmp/kritikon-test.toml"),
            DataSource::Dev(DevScenario::AllStates),
        );
        let started = Instant::now();
        app.begin_refresh();
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(app.loading);
        assert!(app.data.is_none());

        let deadline = Instant::now() + Duration::from_secs(2);
        while app.loading && Instant::now() < deadline {
            app.tick();
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!app.loading);
        assert!(app.data.is_some());
    }

    #[cfg(debug_assertions)]
    #[test]
    fn repeated_refresh_requests_are_coalesced() {
        let mut app = App::new(
            Config::new(5).unwrap(),
            PathBuf::from("/tmp/kritikon-test.toml"),
            DataSource::Dev(DevScenario::AllStates),
        );
        app.begin_refresh();
        app.begin_refresh();
        app.begin_refresh();
        assert!(app.refresh_requested);

        let deadline = Instant::now() + Duration::from_secs(3);
        while (app.loading || app.refresh_generation < 2) && Instant::now() < deadline {
            app.tick();
            thread::sleep(Duration::from_millis(10));
        }

        assert!(!app.loading);
        let data = app.data.as_ref().unwrap();
        assert!(!data.review_queue.is_empty());
        assert!(!data.involved.is_empty());
        assert_eq!(app.refresh_generation, 2);
    }
}
