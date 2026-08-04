use chrono::{DateTime, Utc};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
};

use crate::{
    app::{App, ConfigFocus, RowHitbox, Tab},
    config::MIN_REFRESH_SECONDS,
    model::{
        CheckState, DisplayReviewState, MergeableState, PullRequest, ReviewState, ReviewerKind,
    },
};

const ROW_HEIGHT: u16 = 2;
const ACCENT: Color = Color::Rgb(80, 200, 190);
const MUTED: Color = Color::Rgb(130, 140, 150);

pub fn render(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(6),
            Constraint::Length(2),
        ])
        .split(area);

    render_header(frame, chunks[0], app);
    render_tabs(frame, chunks[1], app);
    render_body(frame, chunks[2], app);
    render_footer(frame, chunks[3], app);

    if app.show_help {
        render_help(frame, area, app);
    }
    if app.config_editor.is_some() {
        render_config(frame, area, app);
    }
}

fn render_header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let title = Line::from(vec![
        Span::styled(
            " KRITIKON ",
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            app.data
                .as_ref()
                .map(|data| format!("@{}", data.viewer))
                .unwrap_or_else(|| "GitHub review command center".into()),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ]);

    let subtitle = if let Some(data) = &app.data {
        let refresh = if app.loading {
            "refreshing…".into()
        } else {
            let seconds = app
                .refresh_remaining
                .as_secs()
                .saturating_add(u64::from(app.refresh_remaining.subsec_nanos() > 0));
            format!("next in {seconds}s")
        };
        Line::from(vec![
            Span::styled(" Open PRs only", Style::default().fg(ACCENT)),
            Span::styled("  ·  ", Style::default().fg(MUTED)),
            Span::styled(
                format!("synced {}", relative_time(data.fetched_at)),
                Style::default().fg(MUTED),
            ),
            Span::styled("  ·  ", Style::default().fg(MUTED)),
            Span::styled(
                format!("every {}s · {refresh}", app.config.refresh_seconds),
                Style::default().fg(MUTED),
            ),
            if app.commit_loading {
                Span::styled(
                    "  ·  syncing commit PRs…",
                    Style::default().fg(Color::Yellow),
                )
            } else {
                Span::raw("")
            },
            app.data_source
                .label()
                .map(|scenario| {
                    Span::styled(
                        format!("  ·  DEV {scenario}"),
                        Style::default().fg(Color::Yellow),
                    )
                })
                .unwrap_or_else(|| Span::raw("")),
            if data.warnings.is_empty() {
                Span::raw("")
            } else {
                Span::styled(
                    format!("  ·  {} warning(s)", data.warnings.len()),
                    Style::default().fg(Color::Yellow),
                )
            },
        ])
    } else if app.loading {
        Line::from(Span::styled(
            " Loading open pull requests and review activity…",
            Style::default().fg(MUTED),
        ))
    } else {
        Line::from(Span::styled(
            " Uses your authenticated GitHub CLI session",
            Style::default().fg(MUTED),
        ))
    };

    frame.render_widget(Paragraph::new(vec![title, subtitle]), area);
}

fn render_tabs(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let tabs = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(34),
            Constraint::Percentage(33),
            Constraint::Percentage(33),
        ])
        .split(area);
    app.tab_hitboxes = [tabs[0], tabs[1], tabs[2]];

    let review_count = app.data.as_ref().map_or(0, |data| data.review_queue.len());
    let involved_count = app.data.as_ref().map_or(0, |data| data.involved.len());
    let owned_count = app.data.as_ref().map_or(0, |data| data.owned.len());
    render_tab(
        frame,
        tabs[0],
        format!("TO REVIEW   {review_count}"),
        app.tab == Tab::ReviewQueue,
    );
    render_tab(
        frame,
        tabs[1],
        format!("INVOLVED   {involved_count}"),
        app.tab == Tab::Involved,
    );
    render_tab(
        frame,
        tabs[2],
        format!("MY PRS   {owned_count}"),
        app.tab == Tab::Owned,
    );
}

fn render_tab(frame: &mut Frame<'_>, area: Rect, label: String, active: bool) {
    let style = if active {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(MUTED)
    };
    let border_style = if active {
        Style::default().fg(ACCENT)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    frame.render_widget(
        Paragraph::new(label)
            .alignment(Alignment::Center)
            .style(style)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(border_style),
            ),
        area,
    );
}

fn render_body(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    if app.details_expanded && app.data.is_some() {
        app.row_hitboxes.clear();
        render_details(frame, area, app, false);
        return;
    }

    let (list_area, detail_area) = if area.width >= 110 {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(57), Constraint::Percentage(43)])
            .split(area);
        (chunks[0], chunks[1])
    } else if area.height >= 12 {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(56), Constraint::Percentage(44)])
            .split(area);
        (chunks[0], chunks[1])
    } else {
        (area, Rect::default())
    };

    render_list(frame, list_area, app);
    if detail_area.width > 0 && detail_area.height > 0 {
        render_details(frame, detail_area, app, area.width < 110);
    }

    if app.data.is_none() && !app.loading {
        render_startup_error(frame, area, app);
    }
}

fn render_list(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let title = match app.tab {
        Tab::ReviewQueue => " Needs your review — direct + every team ",
        Tab::Involved => " Open PRs you have taken part in ",
        Tab::Owned => " Your open pull requests ",
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let capacity = (inner.height / ROW_HEIGHT).max(1) as usize;
    app.set_visible_rows(capacity);
    app.row_hitboxes.clear();

    let Some(data) = &app.data else {
        if app.loading {
            frame.render_widget(
                Paragraph::new("Fetching review status…")
                    .style(Style::default().fg(MUTED))
                    .alignment(Alignment::Center),
                inner,
            );
        }
        return;
    };

    let items = match app.tab {
        Tab::ReviewQueue => &data.review_queue,
        Tab::Involved => &data.involved,
        Tab::Owned => &data.owned,
    };
    if items.is_empty() {
        let message = match app.tab {
            Tab::ReviewQueue => "No open PRs are waiting for your review.",
            Tab::Involved => "No other open PRs currently show your participation.",
            Tab::Owned => "You have no open pull requests.",
        };
        frame.render_widget(
            Paragraph::new(message)
                .style(Style::default().fg(MUTED))
                .alignment(Alignment::Center),
            inner,
        );
        return;
    }

    let start = app.offset();
    let end = (start + capacity).min(items.len());
    let selected = app.selected_index();
    let mut hitboxes = Vec::new();
    for (visible_index, item_index) in (start..end).enumerate() {
        let y = inner.y + (visible_index as u16 * ROW_HEIGHT);
        let rect = Rect::new(inner.x, y, inner.width, ROW_HEIGHT.min(inner.bottom() - y));
        let pull_request = &items[item_index];
        render_row(
            frame,
            rect,
            pull_request,
            item_index == selected,
            app.tab,
            &data.viewer,
        );
        hitboxes.push(RowHitbox {
            rect,
            index: item_index,
        });
    }
    app.row_hitboxes = hitboxes;
}

fn render_row(
    frame: &mut Frame<'_>,
    area: Rect,
    pull_request: &PullRequest,
    selected: bool,
    tab: Tab,
    viewer: &str,
) {
    let row_style = if selected {
        Style::default().bg(Color::Rgb(38, 53, 58))
    } else {
        Style::default()
    };
    let marker = if selected { "› " } else { "  " };
    let mut title = vec![
        Span::styled(marker, Style::default().fg(ACCENT)),
        Span::styled(
            format!("{}#{}", pull_request.repository, pull_request.number),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
    ];
    if pull_request.is_draft {
        title.push(badge(" DRAFT ", Color::Magenta));
        title.push(Span::raw(" "));
    }
    let state = pull_request.display_review_state();
    title.push(badge(
        format!(" {} ", state.label()),
        review_display_color(state),
    ));
    title.push(Span::raw("  "));
    title.push(Span::styled(
        pull_request.title.clone(),
        Style::default().add_modifier(Modifier::BOLD),
    ));

    let metadata = match tab {
        Tab::ReviewQueue => {
            let via = if pull_request.requested_via.is_empty() {
                "request pending".into()
            } else {
                format!("via {}", pull_request.requested_via.join(", "))
            };
            let prior = pull_request
                .viewer_review(viewer)
                .map(|review| format!(" · your last: {}", review.state.label()))
                .unwrap_or_default();
            format!(
                "  @{} · updated {} · {}{} · {} review event(s)",
                pull_request.author,
                relative_time(pull_request.updated_at),
                via,
                prior,
                pull_request.total_review_events
            )
        }
        Tab::Involved => {
            let reasons = if pull_request.involvement.is_empty() {
                "PARTICIPATING".into()
            } else {
                pull_request.involvement_label()
            };
            let checks = pull_request.checks.map_or("No checks", CheckState::label);
            format!(
                "  {reasons} · @{} · updated {} · CI {}",
                pull_request.author,
                relative_time(pull_request.updated_at),
                checks
            )
        }
        Tab::Owned => {
            let checks = pull_request.checks.map_or("No checks", CheckState::label);
            format!(
                "  updated {} · {} reviewer(s) · awaiting {} · CI {}",
                relative_time(pull_request.updated_at),
                pull_request.review_summary().total_reviewers(),
                pull_request.total_review_requests,
                checks
            )
        }
    };

    frame.render_widget(
        Paragraph::new(vec![
            Line::from(title),
            Line::from(Span::styled(metadata, Style::default().fg(MUTED))),
        ])
        .style(row_style),
        area,
    );
}

fn render_details(frame: &mut Frame<'_>, area: Rect, app: &mut App, compact: bool) {
    let title = if app.details_expanded {
        " Selected PR details — d/Esc to return "
    } else {
        " Selected PR details — d to expand "
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area).inner(Margin {
        horizontal: 1,
        vertical: 0,
    });
    frame.render_widget(block, area);

    let Some(data) = &app.data else {
        return;
    };
    let item = match app.tab {
        Tab::ReviewQueue => data.review_queue.get(app.selected_index()),
        Tab::Involved => data.involved.get(app.selected_index()),
        Tab::Owned => data.owned.get(app.selected_index()),
    };
    let Some(pr) = item else {
        frame.render_widget(
            Paragraph::new("Nothing selected").style(Style::default().fg(MUTED)),
            inner,
        );
        return;
    };

    if compact {
        app.detail_scroll = 0;
        app.detail_max_scroll = 0;
        render_compact_details(frame, inner, pr, app.tab, &data.viewer);
        return;
    }

    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!("{}#{}", pr.repository, pr.number),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(if pr.is_draft {
                "  [DRAFT]"
            } else {
                "  [READY]"
            }),
        ]),
        Line::from(Span::styled(
            pr.title.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
    ];

    if app.tab == Tab::ReviewQueue {
        let via = if pr.requested_via.is_empty() {
            "Unknown request source".into()
        } else {
            pr.requested_via.join(", ")
        };
        lines.push(Line::from(vec![
            Span::styled("Request ", Style::default().fg(MUTED)),
            Span::styled(via, Style::default().fg(Color::Yellow)),
        ]));
        lines.push(Line::from(vec![
            Span::styled("You     ", Style::default().fg(MUTED)),
            Span::raw(
                pr.viewer_review(&data.viewer)
                    .map(|review| format!("Last review: {}", review.state.label()))
                    .unwrap_or_else(|| "No submitted review yet".into()),
            ),
        ]));
    }

    if app.tab == Tab::Involved {
        let reasons = if pr.involvement.is_empty() {
            "PARTICIPATING".into()
        } else {
            pr.involvement_label()
        };
        lines.push(Line::from(vec![
            Span::styled("Why here ", Style::default().fg(MUTED)),
            Span::styled(
                reasons,
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
        ]));
    }

    let display_state = pr.display_review_state();
    let summary = pr.review_summary();
    lines.push(Line::from(vec![
        Span::styled("Review  ", Style::default().fg(MUTED)),
        Span::styled(
            display_state.label(),
            Style::default()
                .fg(review_display_color(display_state))
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            " · {} approved · {} changes · {} comments · {} dismissed · {} pending",
            summary.approved,
            summary.changes_requested,
            summary.commented,
            summary.dismissed,
            summary.pending
        )),
    ]));

    let waiting = if pr.review_requests.is_empty() {
        "Nobody".into()
    } else {
        pr.review_requests
            .iter()
            .map(|request| match request.kind {
                ReviewerKind::User => request.name.clone(),
                ReviewerKind::Team => format!("{} (team)", request.name),
                ReviewerKind::Other => request.name.clone(),
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    lines.push(Line::from(vec![
        Span::styled("Waiting ", Style::default().fg(MUTED)),
        Span::raw(waiting),
        if pr.total_review_requests > pr.review_requests.len() {
            Span::styled(
                format!(
                    " +{} more",
                    pr.total_review_requests - pr.review_requests.len()
                ),
                Style::default().fg(MUTED),
            )
        } else {
            Span::raw("")
        },
    ]));

    lines.push(Line::from(vec![
        Span::styled("Merge   ", Style::default().fg(MUTED)),
        Span::styled(
            pr.mergeable.label(),
            Style::default().fg(merge_color(pr.mergeable)),
        ),
        Span::styled("  ·  Checks ", Style::default().fg(MUTED)),
        Span::styled(
            pr.checks.map_or("No checks", CheckState::label),
            Style::default().fg(check_color(pr.checks)),
        ),
    ]));

    lines.push(Line::from(vec![
        Span::styled("Author  ", Style::default().fg(MUTED)),
        Span::raw(format!("@{}", pr.author)),
        Span::styled("  ·  Created ", Style::default().fg(MUTED)),
        Span::raw(relative_time(pr.created_at)),
        Span::styled("  ·  Updated ", Style::default().fg(MUTED)),
        Span::raw(relative_time(pr.updated_at)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("Branch  ", Style::default().fg(MUTED)),
        Span::raw(format!("{} ← {}", pr.base_ref, pr.head_ref)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("Change  ", Style::default().fg(MUTED)),
        Span::styled(
            format!("+{}", pr.additions),
            Style::default().fg(Color::Green),
        ),
        Span::raw(" "),
        Span::styled(
            format!("-{}", pr.deletions),
            Style::default().fg(Color::Red),
        ),
        Span::raw(format!(
            " · {} files · {} comments",
            pr.changed_files, pr.comments
        )),
    ]));

    if !pr.labels.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("Labels  ", Style::default().fg(MUTED)),
            Span::raw(pr.labels.join(", ")),
        ]));
    }

    let effective_reviews = pr.effective_reviews();
    if !effective_reviews.is_empty() {
        lines.push(Line::from(Span::styled(
            "Reviewer breakdown",
            Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
        )));
        let available = if app.details_expanded {
            effective_reviews.len()
        } else {
            inner.height.saturating_sub(lines.len() as u16) as usize
        };
        for review in effective_reviews.iter().take(available.max(1)) {
            let submitted = review
                .submitted_at
                .map(relative_time)
                .unwrap_or_else(|| "not submitted".into());
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{:<18}", review.state.label()),
                    Style::default().fg(review_color(review.state)),
                ),
                Span::raw(format!(" @{} · {}", review.author, submitted)),
            ]));
        }
        if effective_reviews.len() > available.max(1) {
            lines.push(Line::from(Span::styled(
                format!(
                    "… {} more reviewer(s)",
                    effective_reviews.len() - available.max(1)
                ),
                Style::default().fg(MUTED),
            )));
        }
    }

    let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: true });
    let line_count = paragraph.line_count(inner.width) as u16;
    app.detail_max_scroll = line_count.saturating_sub(inner.height);
    app.detail_scroll = app.detail_scroll.min(app.detail_max_scroll);
    frame.render_widget(paragraph.scroll((app.detail_scroll, 0)), inner);
}

fn render_compact_details(
    frame: &mut Frame<'_>,
    area: Rect,
    pr: &PullRequest,
    tab: Tab,
    viewer: &str,
) {
    let summary = pr.review_summary();
    let state = pr.display_review_state();
    let waiting = if pr.review_requests.is_empty() {
        "nobody".into()
    } else {
        pr.review_requests
            .iter()
            .map(|request| request.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let context = match tab {
        Tab::ReviewQueue => {
            let via = if pr.requested_via.is_empty() {
                "unknown".into()
            } else {
                pr.requested_via.join(", ")
            };
            let your_state = pr
                .viewer_review(viewer)
                .map(|review| review.state.label())
                .unwrap_or("NO REVIEW YET");
            format!("Request  {via} · You: {your_state}")
        }
        Tab::Involved => format!(
            "Why here  {}",
            if pr.involvement.is_empty() {
                "PARTICIPATING".into()
            } else {
                pr.involvement_label()
            }
        ),
        Tab::Owned => format!("Waiting  {waiting}"),
    };

    let lines = vec![
        Line::from(vec![
            Span::styled(
                format!("{}#{}", pr.repository, pr.number),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(if pr.is_draft {
                "  [DRAFT]"
            } else {
                "  [READY]"
            }),
        ]),
        Line::from(Span::styled(
            pr.title.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(vec![
            Span::styled("Review   ", Style::default().fg(MUTED)),
            Span::styled(
                state.label(),
                Style::default()
                    .fg(review_display_color(state))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(
                " · A{} C{} M{} D{} P{}",
                summary.approved,
                summary.changes_requested,
                summary.commented,
                summary.dismissed,
                summary.pending
            )),
        ]),
        Line::from(context),
        Line::from(vec![
            Span::styled("Status   ", Style::default().fg(MUTED)),
            Span::styled(
                pr.mergeable.label(),
                Style::default().fg(merge_color(pr.mergeable)),
            ),
            Span::raw(" · CI "),
            Span::styled(
                pr.checks.map_or("No checks", CheckState::label),
                Style::default().fg(check_color(pr.checks)),
            ),
            Span::raw(format!(" · awaiting {}", pr.total_review_requests)),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let status = if let Some((notice, _)) = &app.notice {
        Span::styled(notice.clone(), Style::default().fg(ACCENT))
    } else if app.loading && app.data.is_some() {
        Span::styled("Refreshing…", Style::default().fg(Color::Yellow))
    } else if app.commit_loading && app.data.is_some() {
        Span::styled(
            "Syncing open PRs connected to your commits…",
            Style::default().fg(Color::Yellow),
        )
    } else if let Some(error) = &app.error {
        Span::styled(
            format!("Refresh failed: {error}"),
            Style::default().fg(Color::Red),
        )
    } else if let Some(data) = &app.data {
        data.warnings
            .first()
            .map(|warning| Span::styled(warning.clone(), Style::default().fg(Color::Yellow)))
            .unwrap_or_else(|| Span::raw(""))
    } else {
        Span::raw("")
    };

    let keys = if app.details_expanded {
        "↑↓/jk/PgUp/PgDn scroll details  Home/End jump  Enter open  c copy branch  d/Esc return  t timer  ? help  q quit"
    } else if area.width >= 100 {
        "↑↓/jk move  PgUp/PgDn  Tab/←→ views  Enter/o open  c copy branch  d details  t timer  r refresh  ? help  q quit"
    } else {
        "↑↓ move  Tab/←→ views  Enter open  c copy  d details  t timer  r refresh  ? help  q quit"
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(status),
            Line::from(Span::styled(keys, Style::default().fg(MUTED))),
        ]),
        area,
    );
}

fn render_startup_error(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(error) = &app.error else {
        return;
    };
    let popup = centered_rect(72.min(area.width.saturating_sub(2)), 9, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "Could not load GitHub data",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )),
            Line::raw(""),
            Line::from(error.clone()),
            Line::raw(""),
            Line::from(Span::styled(
                "Run `gh auth status`, then press r to retry or q to quit.",
                Style::default().fg(MUTED),
            )),
        ])
        .wrap(Wrap { trim: true })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::Red))
                .title(" GitHub connection "),
        ),
        popup,
    );
}

fn render_help(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let width = 82.min(area.width.saturating_sub(2));
    let height = 31.min(area.height.saturating_sub(2));
    let popup = centered_rect(width, height, area);
    frame.render_widget(Clear, popup);
    let help = vec![
        Line::from(Span::styled(
            "Navigation",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::raw("  ↑/↓ or j/k  move     PgUp/PgDn  page     Home/End  jump"),
        Line::raw("  Tab/Shift+Tab or ←/→  switch views     Enter/o  open in browser"),
        Line::raw("  c  copy head branch     d  expand details     t  refresh timer"),
        Line::raw("  r  refresh     q  quit"),
        Line::raw("  Mouse wheel scrolls; a single left-click on a PR opens it."),
        Line::raw(""),
        Line::from(Span::styled(
            "Command-center views",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::raw("  TO REVIEW  outstanding requests to you or any team you belong to"),
        Line::raw(
            "  INVOLVED   PRs you committed to, reviewed, commented on, were assigned or mentioned in",
        ),
        Line::raw("  MY PRS     every open PR you authored, including drafts and no-review PRs"),
        review_sources_line(app),
        Line::raw("  Commit discovery indexes 1,000 commits, then refreshes incrementally."),
        Line::raw(""),
        Line::from(Span::styled(
            "Every review state",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        legend(
            "APPROVED",
            "An approving review is currently effective",
            Color::Green,
        ),
        legend(
            "CHANGES REQUESTED",
            "A reviewer is blocking on changes",
            Color::Red,
        ),
        legend(
            "REVIEW REQUIRED",
            "Branch rules still require more review",
            Color::Yellow,
        ),
        legend(
            "COMMENTS",
            "Informational review(s), with no decision",
            Color::Blue,
        ),
        legend(
            "DISMISSED",
            "The effective review was dismissed",
            Color::Magenta,
        ),
        legend(
            "PENDING",
            "A draft review has not been submitted",
            Color::Yellow,
        ),
        legend(
            "NO REVIEWS",
            "No submitted or pending review is visible",
            MUTED,
        ),
        Line::raw(""),
        Line::from(Span::styled(
            "Other states and sources",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::raw("  DRAFT / READY · Mergeable / Conflicts / Calculating"),
        Line::raw("  Checks: Passing / Failing / Error / Pending / Expected / No checks"),
        Line::raw("  Only open PRs are queried. Drafts and PRs with no reviews are included."),
    ];
    frame.render_widget(
        Paragraph::new(help).wrap(Wrap { trim: true }).block(
            Block::default()
                .title(" Help & state legend ")
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(ACCENT)),
        ),
        popup,
    );
}

fn review_sources_line(app: &App) -> Line<'static> {
    let Some(data) = &app.data else {
        return Line::raw("  Review requests always include you directly and all visible teams.");
    };
    let sources = if data.teams.is_empty() {
        format!("@{} directly; no visible teams were returned", data.viewer)
    } else {
        format!("@{} directly + {}", data.viewer, data.teams.join(", "))
    };
    Line::from(vec![
        Span::styled("  Monitoring: ", Style::default().fg(MUTED)),
        Span::raw(sources),
    ])
}

fn render_config(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let Some(editor) = app.config_editor.clone() else {
        return;
    };
    let width = 82.min(area.width.saturating_sub(2));
    let height = 17.min(area.height.saturating_sub(2));
    let popup = centered_rect(width, height, area);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(" Configuration ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT));
    let inner = block.inner(popup).inner(Margin {
        horizontal: 1,
        vertical: 0,
    });
    frame.render_widget(block, popup);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("File  ", Style::default().fg(MUTED)),
            Span::raw(app.config_path.display().to_string()),
        ])),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    frame.render_widget(
        Paragraph::new("Refresh interval (seconds)")
            .style(Style::default().add_modifier(Modifier::BOLD)),
        Rect::new(inner.x, inner.y + 2, inner.width, 1),
    );
    let refresh_rect = Rect::new(inner.x, inner.y + 3, 26.min(inner.width), 3);
    app.config_hitboxes.refresh = refresh_rect;
    let input_style = focus_style(editor.focus == ConfigFocus::Refresh);
    let cursor = if editor.focus == ConfigFocus::Refresh {
        "▌"
    } else {
        ""
    };
    frame.render_widget(
        Paragraph::new(format!("{}{}", editor.refresh_input, cursor))
            .style(input_style)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(input_style),
            ),
        refresh_rect,
    );
    frame.render_widget(
        Paragraph::new(format!(
            "Whole seconds only · minimum {MIN_REFRESH_SECONDS} · no configured maximum"
        ))
        .style(Style::default().fg(MUTED)),
        Rect::new(inner.x, inner.y + 6, inner.width, 1),
    );

    let message = if editor.confirm_reset {
        Line::from(Span::styled(
            "Delete the config file and restore the 30-second timer? Press y, x, or Enter to confirm; Esc cancels.",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ))
    } else if let Some(error) = &editor.error {
        Line::from(Span::styled(error.clone(), Style::default().fg(Color::Red)))
    } else {
        Line::from(Span::styled(
            "This only controls refresh timing. Direct and all visible team requests are always included.",
            Style::default().fg(MUTED),
        ))
    };
    frame.render_widget(
        Paragraph::new(message).wrap(Wrap { trim: true }),
        Rect::new(inner.x, inner.y + 8, inner.width, 2),
    );

    let button_area = Rect::new(inner.x, inner.y + 10, inner.width, 3);
    let buttons = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(30),
            Constraint::Percentage(40),
            Constraint::Percentage(30),
        ])
        .split(button_area);
    app.config_hitboxes.save = buttons[0];
    app.config_hitboxes.reset = buttons[1];
    app.config_hitboxes.cancel = buttons[2];
    render_config_button(
        frame,
        buttons[0],
        "Save",
        editor.focus == ConfigFocus::Save,
        Color::Green,
    );
    render_config_button(
        frame,
        buttons[1],
        "Reset & delete",
        editor.focus == ConfigFocus::Reset,
        Color::Yellow,
    );
    render_config_button(
        frame,
        buttons[2],
        "Cancel",
        editor.focus == ConfigFocus::Cancel,
        MUTED,
    );
    frame.render_widget(
        Paragraph::new("Tab/↑↓ controls · type seconds · Enter/s save · x reset · Esc cancel")
            .style(Style::default().fg(MUTED)),
        Rect::new(inner.x, inner.y + 13, inner.width, 1),
    );
}

fn render_config_button(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    focused: bool,
    color: Color,
) {
    frame.render_widget(
        Paragraph::new(label)
            .alignment(Alignment::Center)
            .style(if focused {
                Style::default()
                    .fg(Color::Black)
                    .bg(color)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(color)
            })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(if focused {
                        Style::default().fg(color)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    }),
            ),
        area,
    );
}

fn focus_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    }
}

fn legend<'a>(label: &'a str, description: &'a str, color: Color) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("  {label:<19}"), Style::default().fg(color)),
        Span::raw(description),
    ])
}

fn badge(label: impl Into<String>, color: Color) -> Span<'static> {
    Span::styled(
        label.into(),
        Style::default()
            .fg(Color::Black)
            .bg(color)
            .add_modifier(Modifier::BOLD),
    )
}

fn review_display_color(state: DisplayReviewState) -> Color {
    match state {
        DisplayReviewState::Approved => Color::Green,
        DisplayReviewState::ChangesRequested => Color::Red,
        DisplayReviewState::ReviewRequired | DisplayReviewState::PendingOnly => Color::Yellow,
        DisplayReviewState::CommentsOnly => Color::Blue,
        DisplayReviewState::DismissedOnly => Color::Magenta,
        DisplayReviewState::NoReviews | DisplayReviewState::Unknown => MUTED,
    }
}

fn review_color(state: ReviewState) -> Color {
    match state {
        ReviewState::Approved => Color::Green,
        ReviewState::ChangesRequested => Color::Red,
        ReviewState::Commented => Color::Blue,
        ReviewState::Dismissed => Color::Magenta,
        ReviewState::Pending => Color::Yellow,
        ReviewState::Unknown => MUTED,
    }
}

fn merge_color(state: MergeableState) -> Color {
    match state {
        MergeableState::Mergeable => Color::Green,
        MergeableState::Conflicting => Color::Red,
        MergeableState::Unknown => Color::Yellow,
    }
}

fn check_color(state: Option<CheckState>) -> Color {
    match state {
        Some(CheckState::Success) => Color::Green,
        Some(CheckState::Failure | CheckState::Error) => Color::Red,
        Some(CheckState::Pending | CheckState::Expected) => Color::Yellow,
        Some(CheckState::Unknown) | None => MUTED,
    }
}

pub fn relative_time(timestamp: DateTime<Utc>) -> String {
    let seconds = (Utc::now() - timestamp).num_seconds().max(0);
    match seconds {
        0..=59 => "now".into(),
        60..=3_599 => format!("{}m ago", seconds / 60),
        3_600..=86_399 => format!("{}h ago", seconds / 3_600),
        86_400..=604_799 => format!("{}d ago", seconds / 86_400),
        _ => timestamp.format("%Y-%m-%d").to_string(),
    }
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use ratatui::{Terminal, backend::TestBackend};

    use crate::{
        app::App,
        model::{
            DashboardData, InvolvementReason, MergeableState, PullRequest, Review, ReviewDecision,
            ReviewRequest, ReviewState, ReviewerKind,
        },
    };

    use super::*;

    fn sample_pr() -> PullRequest {
        PullRequest {
            number: 42,
            title: "Make the review dashboard calm but complete".into(),
            url: "https://github.com/acme/app/pull/42".into(),
            repository: "acme/app".into(),
            author: "alice".into(),
            is_draft: true,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            additions: 120,
            deletions: 12,
            changed_files: 4,
            base_ref: "main".into(),
            head_ref: "review-dashboard".into(),
            mergeable: MergeableState::Conflicting,
            review_decision: Some(ReviewDecision::ChangesRequested),
            reviews: vec![Review {
                author: "bob".into(),
                state: ReviewState::ChangesRequested,
                submitted_at: Some(Utc::now()),
            }],
            total_review_events: 1,
            review_requests: vec![ReviewRequest {
                name: "@carol".into(),
                kind: ReviewerKind::User,
            }],
            total_review_requests: 1,
            comments: 3,
            labels: vec!["ui".into()],
            checks: Some(CheckState::Failure),
            requested_via: vec!["@viewer".into()],
            involvement: vec![InvolvementReason::Committed, InvolvementReason::Commented],
        }
    }

    #[test]
    fn renders_a_compact_list_and_detailed_selected_pr() {
        let pr = sample_pr();
        let data = DashboardData {
            viewer: "viewer".into(),
            review_queue: vec![pr.clone()],
            involved: vec![pr.clone()],
            owned: vec![pr],
            warnings: vec![],
            fetched_at: Utc::now(),
            teams: vec!["acme/core".into(), "acme/platform".into()],
        };
        let mut app = App::with_data(data);
        let backend = TestBackend::new(140, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        let buffer = terminal.backend().buffer();
        let rendered = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("KRITIKON"));
        assert!(rendered.contains("TO REVIEW"));
        assert!(rendered.contains("INVOLVED"));
        assert!(rendered.contains("MY PRS"));
        assert!(rendered.contains("CHANGES REQUESTED"));
        assert!(rendered.contains("review-dashboard"));
        assert!(rendered.contains("Reviewer breakdown"));
    }

    #[test]
    fn compact_layout_prioritizes_review_status() {
        let pr = sample_pr();
        let data = DashboardData {
            viewer: "viewer".into(),
            review_queue: vec![pr],
            involved: vec![],
            owned: vec![],
            warnings: vec![],
            fetched_at: Utc::now(),
            teams: vec!["acme/core".into()],
        };
        let mut app = App::with_data(data);
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        let buffer = terminal.backend().buffer();
        let rendered = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Review   CHANGES REQUESTED"));
        assert!(rendered.contains("Request  @viewer"));
        assert!(rendered.contains("Status   Conflicts"));
    }

    #[test]
    fn configuration_editor_is_clear_and_mouse_targets_are_registered() {
        let pr = sample_pr();
        let data = DashboardData {
            viewer: "viewer".into(),
            review_queue: vec![pr],
            involved: vec![],
            owned: vec![],
            warnings: vec![],
            fetched_at: Utc::now(),
            teams: vec!["acme/core".into()],
        };
        let mut app = App::with_data(data);
        app.open_config(None);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        let buffer = terminal.backend().buffer();
        let rendered = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Refresh interval (seconds)"));
        assert!(rendered.contains("minimum 5"));
        assert!(rendered.contains("only controls refresh timing"));
        assert!(!rendered.contains("Direct only"));
        assert!(!rendered.contains("Teams only"));
        assert!(rendered.contains("Reset & delete"));
        assert!(app.config_hitboxes.save.width > 0);
    }

    #[test]
    fn involved_view_explains_why_each_pr_is_present() {
        let pr = sample_pr();
        let data = DashboardData {
            viewer: "viewer".into(),
            review_queue: vec![],
            involved: vec![pr],
            owned: vec![],
            warnings: vec![],
            fetched_at: Utc::now(),
            teams: vec!["acme/core".into()],
        };
        let mut app = App::with_data(data);
        app.set_tab(Tab::Involved);
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        let buffer = terminal.backend().buffer();
        let rendered = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("COMMITTED + COMMENTED"));
        assert!(rendered.contains("Why here"));
    }

    #[test]
    fn help_names_every_monitored_team() {
        let data = DashboardData {
            viewer: "viewer".into(),
            review_queue: vec![],
            involved: vec![],
            owned: vec![],
            warnings: vec![],
            fetched_at: Utc::now(),
            teams: vec!["acme/core".into(), "acme/platform".into()],
        };
        let mut app = App::with_data(data);
        app.show_help = true;
        let backend = TestBackend::new(100, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        let buffer = terminal.backend().buffer();
        let rendered = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("@viewer directly"));
        assert!(rendered.contains("acme/core"));
        assert!(rendered.contains("acme/platform"));
        assert!(rendered.contains("refreshes incrementally"));
    }
}
