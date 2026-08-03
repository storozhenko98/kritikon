use std::{
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::{Duration, Instant},
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::{github, model::DashboardData};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Requested,
    Owned,
}

impl Tab {
    pub fn index(self) -> usize {
        match self {
            Self::Requested => 0,
            Self::Owned => 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    Refresh,
    Open(String),
}

#[derive(Debug, Clone)]
pub struct RowHitbox {
    pub rect: Rect,
    pub index: usize,
}

pub struct App {
    pub data: Option<DashboardData>,
    pub tab: Tab,
    pub selected: [usize; 2],
    pub offsets: [usize; 2],
    pub visible_rows: usize,
    pub loading: bool,
    pub error: Option<String>,
    pub show_help: bool,
    pub details_expanded: bool,
    pub detail_scroll: u16,
    pub detail_max_scroll: u16,
    pub row_hitboxes: Vec<RowHitbox>,
    pub tab_hitboxes: [Rect; 2],
    pub notice: Option<(String, Instant)>,
    pub include_team_requests: bool,
    pub refresh_interval: Option<Duration>,
    pub next_refresh: Option<Instant>,
    refresh_receiver: Option<Receiver<Result<DashboardData, String>>>,
}

impl App {
    pub fn new(include_team_requests: bool, refresh_seconds: u64) -> Self {
        Self {
            data: None,
            tab: Tab::Requested,
            selected: [0, 0],
            offsets: [0, 0],
            visible_rows: 1,
            loading: false,
            error: None,
            show_help: false,
            details_expanded: false,
            detail_scroll: 0,
            detail_max_scroll: 0,
            row_hitboxes: Vec::new(),
            tab_hitboxes: [Rect::default(), Rect::default()],
            notice: None,
            include_team_requests,
            refresh_interval: (refresh_seconds > 0).then(|| Duration::from_secs(refresh_seconds)),
            next_refresh: None,
            refresh_receiver: None,
        }
    }

    #[cfg(test)]
    pub fn with_data(data: DashboardData) -> Self {
        let mut app = Self::new(true, 0);
        app.data = Some(data);
        app
    }

    pub fn begin_refresh(&mut self) {
        if self.loading {
            return;
        }
        self.loading = true;
        self.error = None;
        let include_team_requests = self.include_team_requests;
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let result = github::fetch_dashboard(include_team_requests)
                .map_err(|error| format!("{error:#}"));
            let _ = sender.send(result);
        });
        self.refresh_receiver = Some(receiver);
    }

    pub fn tick(&mut self) {
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
            Some(Ok(Ok(data))) => {
                self.data = Some(data);
                self.loading = false;
                self.error = None;
                self.refresh_receiver = None;
                self.clamp_selection();
                self.schedule_next_refresh();
            }
            Some(Ok(Err(error))) => {
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

        if !self.loading
            && self
                .next_refresh
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.begin_refresh();
        }
    }

    fn schedule_next_refresh(&mut self) {
        self.next_refresh = self
            .refresh_interval
            .map(|interval| Instant::now() + interval);
    }

    pub fn items_len(&self) -> usize {
        let Some(data) = &self.data else {
            return 0;
        };
        match self.tab {
            Tab::Requested => data.requested.len(),
            Tab::Owned => data.owned.len(),
        }
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
        self.ensure_visible();
    }

    pub fn select(&mut self, selected: usize) {
        if selected < self.items_len() {
            self.selected[self.tab.index()] = selected;
            self.ensure_visible();
        }
    }

    pub fn select_home(&mut self) {
        self.selected[self.tab.index()] = 0;
        self.ensure_visible();
    }

    pub fn select_end(&mut self) {
        self.selected[self.tab.index()] = self.items_len().saturating_sub(1);
        self.ensure_visible();
    }

    fn clamp_selection(&mut self) {
        for (tab, index) in [(Tab::Requested, 0), (Tab::Owned, 1)] {
            let len = self.data.as_ref().map_or(0, |data| match tab {
                Tab::Requested => data.requested.len(),
                Tab::Owned => data.owned.len(),
            });
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
        let data = self.data.as_ref()?;
        let pull_request = match self.tab {
            Tab::Requested => data.requested.get(self.selected_index()),
            Tab::Owned => data.owned.get(self.selected_index()),
        }?;
        Some(pull_request.url.clone())
    }

    pub fn set_notice(&mut self, message: impl Into<String>) {
        self.notice = Some((message.into(), Instant::now() + Duration::from_secs(3)));
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Action::Quit;
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
            KeyCode::Char('d') => {
                self.details_expanded = !self.details_expanded;
                self.detail_scroll = 0;
                Action::None
            }
            KeyCode::Char('1') => {
                self.set_tab(Tab::Requested);
                Action::None
            }
            KeyCode::Char('2') => {
                self.set_tab(Tab::Owned);
                Action::None
            }
            KeyCode::Tab | KeyCode::Right | KeyCode::Left => {
                self.set_tab(match self.tab {
                    Tab::Requested => Tab::Owned,
                    Tab::Owned => Tab::Requested,
                });
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

    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> Action {
        if self.show_help {
            return Action::None;
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
                    self.set_tab(Tab::Requested);
                    return Action::None;
                }
                if contains(self.tab_hitboxes[1], x, y) {
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

fn contains(rect: Rect, x: u16, y: u16) -> bool {
    x >= rect.x
        && x < rect.x.saturating_add(rect.width)
        && y >= rect.y
        && y < rect.y.saturating_add(rect.height)
}

#[cfg(test)]
mod tests {
    use crate::model::{CheckState, MergeableState, PullRequest};
    use chrono::Utc;

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
        }
    }

    fn app() -> App {
        App::with_data(DashboardData {
            viewer: "viewer".into(),
            requested: vec![pr(1), pr(2), pr(3), pr(4)],
            owned: vec![pr(5)],
            warnings: vec![],
            fetched_at: Utc::now(),
            team_count: 1,
        })
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
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
    fn details_can_be_expanded_and_escape_returns_to_the_list() {
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
}
