use std::{thread, time::Duration};

use anyhow::{Result, bail};
use chrono::{Duration as ChronoDuration, Utc};
use clap::ValueEnum;

use crate::model::{
    CheckState, DashboardData, InvolvementReason, MergeableState, PullRequest, Review,
    ReviewDecision, ReviewRequest, ReviewState, ReviewerKind,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum DevScenario {
    AllStates,
    Empty,
    Many,
    Error,
    ConfigError,
}

impl DevScenario {
    pub fn label(self) -> &'static str {
        match self {
            Self::AllStates => "all-states",
            Self::Empty => "empty",
            Self::Many => "many",
            Self::Error => "error",
            Self::ConfigError => "config-error",
        }
    }
}

pub fn fetch_dashboard(scenario: DevScenario, generation: u64) -> Result<DashboardData> {
    // Deliberate latency makes it easy to verify that the existing screen stays
    // interactive while refresh work happens on the background thread.
    thread::sleep(Duration::from_millis(350));
    if scenario == DevScenario::Error {
        bail!("simulated GitHub transport failure (development scenario)");
    }

    let mut data = match scenario {
        DevScenario::Empty => empty_dashboard(),
        DevScenario::Many => many_dashboard(),
        DevScenario::AllStates | DevScenario::ConfigError => all_states_dashboard(),
        DevScenario::Error => unreachable!(),
    };
    data.fetched_at = Utc::now();

    // Alternate ordering and a comment count on each refresh. A correctly
    // implemented UI keeps the selected PR stable by URL through this change.
    if generation % 2 == 1 {
        data.review_queue.reverse();
        data.involved.reverse();
        data.owned.reverse();
    }
    if let Some(first) = data.owned.first_mut() {
        first.comments = first.comments.saturating_add(generation as usize);
    }
    Ok(data)
}

fn empty_dashboard() -> DashboardData {
    DashboardData {
        viewer: "dev-user".into(),
        review_queue: Vec::new(),
        involved: Vec::new(),
        owned: Vec::new(),
        warnings: Vec::new(),
        fetched_at: Utc::now(),
        teams: vec![
            "acme/platform".into(),
            "acme/security".into(),
            "acme/reliability".into(),
        ],
    }
}

fn all_states_dashboard() -> DashboardData {
    let specs = [
        ("No reviews yet", None, None, false),
        (
            "Informational comments",
            Some(ReviewState::Commented),
            None,
            false,
        ),
        (
            "Approved and ready",
            Some(ReviewState::Approved),
            Some(ReviewDecision::Approved),
            false,
        ),
        (
            "Changes were requested",
            Some(ReviewState::ChangesRequested),
            Some(ReviewDecision::ChangesRequested),
            false,
        ),
        (
            "Dismissed review",
            Some(ReviewState::Dismissed),
            None,
            false,
        ),
        (
            "Pending draft review",
            Some(ReviewState::Pending),
            None,
            true,
        ),
        (
            "More review is required",
            Some(ReviewState::Commented),
            Some(ReviewDecision::ReviewRequired),
            false,
        ),
    ];

    let owned = specs
        .iter()
        .enumerate()
        .map(|(index, (title, state, decision, draft))| {
            sample_pr((index + 1) as u64, title, *state, *decision, *draft, index)
        })
        .collect::<Vec<_>>();
    let mut review_queue = owned.clone();
    for (index, pull_request) in review_queue.iter_mut().enumerate() {
        pull_request.author = format!("review-author-{index}");
        pull_request.requested_via = if index % 2 == 0 {
            vec!["@dev-user".into()]
        } else {
            vec!["acme/platform".into()]
        };
        pull_request.url = format!("https://github.com/acme/review-queue/pull/{}", index + 101);
        pull_request.number = (index + 101) as u64;
    }

    let mut involved = owned.clone();
    for (index, pull_request) in involved.iter_mut().enumerate() {
        pull_request.author = format!("collaborator-{index}");
        pull_request.involvement = match index % 6 {
            0 => vec![InvolvementReason::Committed],
            1 => vec![InvolvementReason::Reviewed],
            2 => vec![InvolvementReason::Commented],
            3 => vec![InvolvementReason::Assigned],
            4 => vec![InvolvementReason::Mentioned],
            _ => vec![
                InvolvementReason::Committed,
                InvolvementReason::Commented,
                InvolvementReason::Reviewed,
            ],
        };
        pull_request.url = format!("https://github.com/acme/involved/pull/{}", index + 201);
        pull_request.number = (index + 201) as u64;
    }

    DashboardData {
        viewer: "dev-user".into(),
        review_queue,
        involved,
        owned,
        warnings: Vec::new(),
        fetched_at: Utc::now(),
        teams: vec![
            "acme/platform".into(),
            "acme/security".into(),
            "acme/reliability".into(),
        ],
    }
}

fn many_dashboard() -> DashboardData {
    let states = [
        None,
        Some(ReviewState::Commented),
        Some(ReviewState::Approved),
        Some(ReviewState::ChangesRequested),
        Some(ReviewState::Dismissed),
        Some(ReviewState::Pending),
    ];
    let owned = (0..80)
        .map(|index| {
            let state = states[index % states.len()];
            let decision = match state {
                Some(ReviewState::Approved) => Some(ReviewDecision::Approved),
                Some(ReviewState::ChangesRequested) => Some(ReviewDecision::ChangesRequested),
                _ => None,
            };
            sample_pr(
                (index + 1) as u64,
                &format!("Development PR with a deliberately descriptive title {index}"),
                state,
                decision,
                index % 9 == 0,
                index,
            )
        })
        .collect::<Vec<_>>();
    let mut review_queue = owned.iter().take(40).cloned().collect::<Vec<_>>();
    for (index, pull_request) in review_queue.iter_mut().enumerate() {
        pull_request.requested_via = if index % 2 == 0 {
            vec!["@dev-user".into()]
        } else {
            vec!["acme/reviewers".into()]
        };
    }
    let mut involved = owned.iter().skip(10).take(55).cloned().collect::<Vec<_>>();
    for (index, pull_request) in involved.iter_mut().enumerate() {
        pull_request.involvement = if index % 3 == 0 {
            vec![InvolvementReason::Committed, InvolvementReason::Commented]
        } else if index % 3 == 1 {
            vec![InvolvementReason::Reviewed]
        } else {
            vec![InvolvementReason::Mentioned]
        };
    }
    DashboardData {
        viewer: "dev-user".into(),
        review_queue,
        involved,
        owned,
        warnings: vec!["Simulated warning for layout testing".into()],
        fetched_at: Utc::now(),
        teams: vec!["acme/platform".into(), "acme/security".into()],
    }
}

fn sample_pr(
    number: u64,
    title: &str,
    review_state: Option<ReviewState>,
    review_decision: Option<ReviewDecision>,
    is_draft: bool,
    index: usize,
) -> PullRequest {
    let reviews = review_state
        .map(|state| {
            vec![Review {
                author: format!("reviewer-{index}"),
                state,
                submitted_at: (state != ReviewState::Pending).then(Utc::now),
            }]
        })
        .unwrap_or_default();
    PullRequest {
        number,
        title: title.into(),
        url: format!("https://github.com/acme/kritikon/pull/{number}"),
        repository: "acme/kritikon".into(),
        author: "dev-user".into(),
        is_draft,
        created_at: Utc::now() - ChronoDuration::days((index + 1) as i64),
        updated_at: Utc::now() - ChronoDuration::minutes((index * 7) as i64),
        additions: 20 + index as u64 * 3,
        deletions: 4 + index as u64,
        changed_files: 1 + index as u64 % 12,
        base_ref: "main".into(),
        head_ref: format!("dev/scenario-{number}"),
        mergeable: if index.is_multiple_of(5) {
            MergeableState::Conflicting
        } else {
            MergeableState::Mergeable
        },
        review_decision,
        total_review_events: reviews.len(),
        reviews,
        review_requests: vec![ReviewRequest {
            name: "@next-reviewer".into(),
            kind: ReviewerKind::User,
        }],
        total_review_requests: 1,
        comments: index,
        labels: vec!["development".into(), format!("scenario/{index}")],
        checks: Some(if index.is_multiple_of(4) {
            CheckState::Failure
        } else {
            CheckState::Success
        }),
        requested_via: vec!["@dev-user".into()],
        involvement: Vec::new(),
    }
}
