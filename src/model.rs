use std::collections::HashMap;

use chrono::{DateTime, Utc};

#[derive(Debug, Clone)]
pub struct DashboardData {
    pub viewer: String,
    pub review_queue: Vec<PullRequest>,
    pub involved: Vec<PullRequest>,
    pub owned: Vec<PullRequest>,
    pub warnings: Vec<String>,
    pub fetched_at: DateTime<Utc>,
    pub teams: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub repository: String,
    pub author: String,
    pub is_draft: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub additions: u64,
    pub deletions: u64,
    pub changed_files: u64,
    pub base_ref: String,
    pub head_ref: String,
    pub mergeable: MergeableState,
    pub review_decision: Option<ReviewDecision>,
    pub reviews: Vec<Review>,
    pub total_review_events: usize,
    pub review_requests: Vec<ReviewRequest>,
    pub total_review_requests: usize,
    pub comments: usize,
    pub labels: Vec<String>,
    pub checks: Option<CheckState>,
    pub requested_via: Vec<String>,
    pub involvement: Vec<InvolvementReason>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InvolvementReason {
    Committed,
    Reviewed,
    Commented,
    Assigned,
    Mentioned,
    Participated,
}

impl InvolvementReason {
    pub const DISPLAY_ORDER: [Self; 6] = [
        Self::Committed,
        Self::Reviewed,
        Self::Commented,
        Self::Assigned,
        Self::Mentioned,
        Self::Participated,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Committed => "COMMITTED",
            Self::Reviewed => "REVIEWED",
            Self::Commented => "COMMENTED",
            Self::Assigned => "ASSIGNED",
            Self::Mentioned => "MENTIONED",
            Self::Participated => "PARTICIPATING",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewState {
    Pending,
    Commented,
    Approved,
    ChangesRequested,
    Dismissed,
    Unknown,
}

impl ReviewState {
    pub fn from_api(value: &str) -> Self {
        match value {
            "PENDING" => Self::Pending,
            "COMMENTED" => Self::Commented,
            "APPROVED" => Self::Approved,
            "CHANGES_REQUESTED" => Self::ChangesRequested,
            "DISMISSED" => Self::Dismissed,
            _ => Self::Unknown,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::Commented => "COMMENTED",
            Self::Approved => "APPROVED",
            Self::ChangesRequested => "CHANGES REQUESTED",
            Self::Dismissed => "DISMISSED",
            Self::Unknown => "UNKNOWN REVIEW",
        }
    }

    fn is_decisive(self) -> bool {
        matches!(
            self,
            Self::Approved | Self::ChangesRequested | Self::Dismissed
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
    Unknown,
}

impl ReviewDecision {
    pub fn from_api(value: &str) -> Self {
        match value {
            "APPROVED" => Self::Approved,
            "CHANGES_REQUESTED" => Self::ChangesRequested,
            "REVIEW_REQUIRED" => Self::ReviewRequired,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeableState {
    Mergeable,
    Conflicting,
    Unknown,
}

impl MergeableState {
    pub fn from_api(value: &str) -> Self {
        match value {
            "MERGEABLE" => Self::Mergeable,
            "CONFLICTING" => Self::Conflicting,
            _ => Self::Unknown,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Mergeable => "Mergeable",
            Self::Conflicting => "Conflicts",
            Self::Unknown => "Calculating",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckState {
    Success,
    Failure,
    Error,
    Pending,
    Expected,
    Unknown,
}

impl CheckState {
    pub fn from_api(value: &str) -> Self {
        match value {
            "SUCCESS" => Self::Success,
            "FAILURE" => Self::Failure,
            "ERROR" => Self::Error,
            "PENDING" => Self::Pending,
            "EXPECTED" => Self::Expected,
            _ => Self::Unknown,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Success => "Passing",
            Self::Failure => "Failing",
            Self::Error => "Error",
            Self::Pending => "Pending",
            Self::Expected => "Expected",
            Self::Unknown => "Unknown",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Review {
    pub author: String,
    pub state: ReviewState,
    pub submitted_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct ReviewRequest {
    pub name: String,
    pub kind: ReviewerKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewerKind {
    User,
    Team,
    Other,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewSummary {
    pub approved: usize,
    pub changes_requested: usize,
    pub commented: usize,
    pub dismissed: usize,
    pub pending: usize,
    pub unknown: usize,
}

impl ReviewSummary {
    pub fn total_reviewers(&self) -> usize {
        self.approved
            + self.changes_requested
            + self.commented
            + self.dismissed
            + self.pending
            + self.unknown
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayReviewState {
    Approved,
    ChangesRequested,
    ReviewRequired,
    CommentsOnly,
    DismissedOnly,
    PendingOnly,
    NoReviews,
    Unknown,
}

impl DisplayReviewState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Approved => "APPROVED",
            Self::ChangesRequested => "CHANGES REQUESTED",
            Self::ReviewRequired => "REVIEW REQUIRED",
            Self::CommentsOnly => "COMMENTS",
            Self::DismissedOnly => "DISMISSED",
            Self::PendingOnly => "PENDING",
            Self::NoReviews => "NO REVIEWS",
            Self::Unknown => "UNKNOWN",
        }
    }
}

impl PullRequest {
    pub fn add_involvement(&mut self, reason: InvolvementReason) {
        if !self.involvement.contains(&reason) {
            self.involvement.push(reason);
            self.involvement.sort_by_key(|candidate| {
                InvolvementReason::DISPLAY_ORDER
                    .iter()
                    .position(|ordered| ordered == candidate)
                    .unwrap_or(usize::MAX)
            });
        }
    }

    pub fn involvement_label(&self) -> String {
        self.involvement
            .iter()
            .map(|reason| reason.label())
            .collect::<Vec<_>>()
            .join(" + ")
    }

    /// Returns one effective state per reviewer. Informational comments do not
    /// overwrite a prior approval/change request from the same reviewer.
    pub fn effective_reviews(&self) -> Vec<Review> {
        let mut ordered = self.reviews.clone();
        ordered.sort_by_key(|review| review.submitted_at);

        let mut by_author: HashMap<String, Review> = HashMap::new();
        for review in ordered {
            match by_author.get(&review.author) {
                Some(previous)
                    if review.state == ReviewState::Commented && previous.state.is_decisive() => {}
                Some(previous)
                    if review.state == ReviewState::Pending
                        && previous.state != ReviewState::Pending => {}
                _ => {
                    by_author.insert(review.author.clone(), review);
                }
            }
        }

        let mut effective: Vec<_> = by_author.into_values().collect();
        effective.sort_by(|a, b| {
            b.submitted_at
                .cmp(&a.submitted_at)
                .then_with(|| a.author.cmp(&b.author))
        });
        effective
    }

    pub fn review_summary(&self) -> ReviewSummary {
        let mut summary = ReviewSummary::default();
        for review in self.effective_reviews() {
            match review.state {
                ReviewState::Approved => summary.approved += 1,
                ReviewState::ChangesRequested => summary.changes_requested += 1,
                ReviewState::Commented => summary.commented += 1,
                ReviewState::Dismissed => summary.dismissed += 1,
                ReviewState::Pending => summary.pending += 1,
                ReviewState::Unknown => summary.unknown += 1,
            }
        }
        summary
    }

    pub fn display_review_state(&self) -> DisplayReviewState {
        match self.review_decision {
            Some(ReviewDecision::Approved) => return DisplayReviewState::Approved,
            Some(ReviewDecision::ChangesRequested) => {
                return DisplayReviewState::ChangesRequested;
            }
            Some(ReviewDecision::ReviewRequired) => {
                return DisplayReviewState::ReviewRequired;
            }
            Some(ReviewDecision::Unknown) => return DisplayReviewState::Unknown,
            None => {}
        }

        let summary = self.review_summary();
        if summary.changes_requested > 0 {
            DisplayReviewState::ChangesRequested
        } else if summary.approved > 0 {
            DisplayReviewState::Approved
        } else if summary.commented > 0 {
            DisplayReviewState::CommentsOnly
        } else if summary.dismissed > 0 {
            DisplayReviewState::DismissedOnly
        } else if summary.pending > 0 {
            DisplayReviewState::PendingOnly
        } else if summary.unknown > 0 {
            DisplayReviewState::Unknown
        } else {
            DisplayReviewState::NoReviews
        }
    }

    pub fn viewer_review(&self, viewer: &str) -> Option<Review> {
        self.effective_reviews()
            .into_iter()
            .find(|review| review.author.eq_ignore_ascii_case(viewer))
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn review(author: &str, state: ReviewState, minute: u32) -> Review {
        Review {
            author: author.into(),
            state,
            submitted_at: Some(Utc.with_ymd_and_hms(2026, 8, 3, 12, minute, 0).unwrap()),
        }
    }

    fn pull_request(reviews: Vec<Review>) -> PullRequest {
        PullRequest {
            number: 1,
            title: "Test".into(),
            url: "https://github.com/acme/repo/pull/1".into(),
            repository: "acme/repo".into(),
            author: "owner".into(),
            is_draft: false,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            additions: 1,
            deletions: 1,
            changed_files: 1,
            base_ref: "main".into(),
            head_ref: "feature".into(),
            mergeable: MergeableState::Unknown,
            review_decision: None,
            reviews,
            total_review_events: 0,
            review_requests: vec![],
            total_review_requests: 0,
            comments: 0,
            labels: vec![],
            checks: None,
            requested_via: vec![],
            involvement: vec![],
        }
    }

    #[test]
    fn a_comment_does_not_erase_a_prior_decision() {
        let pr = pull_request(vec![
            review("alice", ReviewState::Approved, 1),
            review("alice", ReviewState::Commented, 2),
        ]);

        assert_eq!(pr.effective_reviews()[0].state, ReviewState::Approved);
    }

    #[test]
    fn a_new_decision_replaces_an_old_decision() {
        let pr = pull_request(vec![
            review("alice", ReviewState::ChangesRequested, 1),
            review("alice", ReviewState::Approved, 2),
        ]);

        assert_eq!(pr.effective_reviews()[0].state, ReviewState::Approved);
    }

    #[test]
    fn all_graphql_review_states_have_a_display_state() {
        let states = [
            (ReviewState::Pending, DisplayReviewState::PendingOnly),
            (ReviewState::Commented, DisplayReviewState::CommentsOnly),
            (ReviewState::Approved, DisplayReviewState::Approved),
            (
                ReviewState::ChangesRequested,
                DisplayReviewState::ChangesRequested,
            ),
            (ReviewState::Dismissed, DisplayReviewState::DismissedOnly),
        ];

        for (review_state, display_state) in states {
            let pr = pull_request(vec![review("alice", review_state, 1)]);
            assert_eq!(pr.display_review_state(), display_state);
        }
    }

    #[test]
    fn involvement_reasons_are_unique_and_command_center_ordered() {
        let mut pr = pull_request(vec![]);
        pr.add_involvement(InvolvementReason::Commented);
        pr.add_involvement(InvolvementReason::Committed);
        pr.add_involvement(InvolvementReason::Commented);
        pr.add_involvement(InvolvementReason::Reviewed);

        assert_eq!(
            pr.involvement,
            vec![
                InvolvementReason::Committed,
                InvolvementReason::Reviewed,
                InvolvementReason::Commented,
            ]
        );
        assert_eq!(pr.involvement_label(), "COMMITTED + REVIEWED + COMMENTED");
    }
}
