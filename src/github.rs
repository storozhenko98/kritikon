use std::{
    collections::{HashMap, HashSet},
    io::Write,
    process::{Command, Stdio},
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::model::{
    CheckState, DashboardData, MergeableState, PullRequest, Review, ReviewDecision, ReviewRequest,
    ReviewState, ReviewerKind,
};

const PAGE_SIZE: u32 = 50;
const GITHUB_SEARCH_LIMIT: usize = 1_000;

const SEARCH_QUERY: &str = r#"
query ReviewMonitorSearch($query: String!, $cursor: String, $pageSize: Int!) {
  search(query: $query, type: ISSUE, first: $pageSize, after: $cursor) {
    issueCount
    pageInfo { hasNextPage endCursor }
    nodes {
      ... on PullRequest {
        number
        title
        url
        isDraft
        createdAt
        updatedAt
        additions
        deletions
        changedFiles
        baseRefName
        headRefName
        mergeable
        reviewDecision
        author { login }
        repository { nameWithOwner }
        comments { totalCount }
        labels(first: 10) { nodes { name } }
        reviews(last: 100) {
          totalCount
          nodes {
            author { login }
            state
            submittedAt
          }
        }
        reviewRequests(first: 100) {
          totalCount
          nodes {
            requestedReviewer {
              __typename
              ... on User { login }
              ... on Team { name slug organization { login } }
            }
          }
        }
        commits(last: 1) {
          nodes { commit { statusCheckRollup { state } } }
        }
      }
    }
  }
}
"#;

pub fn fetch_dashboard(include_team_requests: bool) -> Result<DashboardData> {
    let viewer = fetch_viewer()?;
    let mut warnings = Vec::new();

    let owned_query = format!("is:pr is:open author:{viewer} sort:updated-desc");
    let mut owned = fetch_search(&owned_query, None, &mut warnings)
        .context("could not load your open pull requests")?;

    let direct_query = format!("is:pr is:open user-review-requested:{viewer} sort:updated-desc");
    let mut requested = fetch_search(&direct_query, Some(format!("@{viewer}")), &mut warnings)
        .context("could not load direct review requests")?;

    let teams = if include_team_requests {
        match fetch_teams() {
            Ok(teams) => teams,
            Err(error) => {
                warnings.push(format!(
                    "Team review requests unavailable; direct requests are still shown ({error:#})"
                ));
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    for team in &teams {
        let team_name = format!("{}/{}", team.organization.login, team.slug);
        let query = format!("is:pr is:open team-review-requested:{team_name} sort:updated-desc");
        match fetch_search(&query, Some(team_name.clone()), &mut warnings) {
            Ok(team_prs) => requested.extend(team_prs),
            Err(error) => warnings.push(format!(
                "Could not load requests for {team_name} ({error:#})"
            )),
        }
    }

    deduplicate_requested(&mut requested);
    owned.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    requested.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

    Ok(DashboardData {
        viewer,
        requested,
        owned,
        warnings,
        fetched_at: Utc::now(),
        team_count: teams.len(),
    })
}

fn fetch_viewer() -> Result<String> {
    let output = run_gh(&["api", "user"], None)
        .context("GitHub CLI authentication failed; install `gh` and run `gh auth login`")?;
    let user: ApiViewer =
        serde_json::from_slice(&output).context("invalid `gh api user` response")?;
    Ok(user.login)
}

fn fetch_teams() -> Result<Vec<ApiTeam>> {
    let output = run_gh(
        &["api", "--paginate", "--slurp", "user/teams?per_page=100"],
        None,
    )?;
    let pages: Vec<Vec<ApiTeam>> =
        serde_json::from_slice(&output).context("invalid team-list response")?;
    Ok(pages.into_iter().flatten().collect())
}

fn fetch_search(
    search: &str,
    requested_via: Option<String>,
    warnings: &mut Vec<String>,
) -> Result<Vec<PullRequest>> {
    let mut cursor: Option<String> = None;
    let mut pull_requests = Vec::new();
    let mut warned_about_limit = false;

    loop {
        let payload = GraphQlRequest {
            query: SEARCH_QUERY,
            variables: SearchVariables {
                query: search,
                cursor: cursor.as_deref(),
                page_size: PAGE_SIZE,
            },
        };
        let body = serde_json::to_vec(&payload)?;
        let output = run_gh(&["api", "graphql", "--input", "-"], Some(&body))?;
        let response: GraphQlEnvelope =
            serde_json::from_slice(&output).context("invalid GitHub GraphQL response")?;

        if !response.errors.is_empty() {
            let messages = response
                .errors
                .iter()
                .map(|error| error.message.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            bail!("GitHub GraphQL error: {messages}");
        }

        let connection = response
            .data
            .ok_or_else(|| anyhow!("GitHub returned no data"))?
            .search;

        if connection.issue_count > GITHUB_SEARCH_LIMIT && !warned_about_limit {
            warnings.push(format!(
                "GitHub search reports {} matching PRs; only its first {GITHUB_SEARCH_LIMIT} results are available",
                connection.issue_count
            ));
            warned_about_limit = true;
        }

        for node in connection.nodes {
            let mut pull_request = PullRequest::from(node);
            if let Some(via) = &requested_via {
                pull_request.requested_via.push(via.clone());
            }
            pull_requests.push(pull_request);
        }

        if !connection.page_info.has_next_page || pull_requests.len() >= GITHUB_SEARCH_LIMIT {
            break;
        }
        cursor = connection.page_info.end_cursor;
        if cursor.is_none() {
            break;
        }
    }

    Ok(pull_requests)
}

fn deduplicate_requested(pull_requests: &mut Vec<PullRequest>) {
    let mut by_url: HashMap<String, PullRequest> = HashMap::new();
    for pull_request in pull_requests.drain(..) {
        match by_url.get_mut(&pull_request.url) {
            Some(existing) => existing.requested_via.extend(pull_request.requested_via),
            None => {
                by_url.insert(pull_request.url.clone(), pull_request);
            }
        }
    }

    for pull_request in by_url.values_mut() {
        let mut seen = HashSet::new();
        pull_request
            .requested_via
            .retain(|name| seen.insert(name.clone()));
    }
    *pull_requests = by_url.into_values().collect();
}

fn run_gh(args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>> {
    let mut command = Command::new("gh");
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if input.is_some() {
        command.stdin(Stdio::piped());
    }

    let mut child = command.spawn().with_context(
        || "could not start GitHub CLI (`gh`); install it from https://cli.github.com/",
    )?;
    if let Some(input) = input {
        child
            .stdin
            .take()
            .context("could not open stdin for `gh`")?
            .write_all(input)
            .context("could not send request to `gh`")?;
    }

    let output = child
        .wait_with_output()
        .context("could not wait for `gh`")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            bail!("`gh {}` exited with {}", args.join(" "), output.status);
        }
        bail!("{stderr}");
    }
    Ok(output.stdout)
}

#[derive(Debug, Deserialize)]
struct ApiViewer {
    login: String,
}

#[derive(Debug, Deserialize)]
struct ApiTeam {
    slug: String,
    organization: ApiOrganization,
}

#[derive(Debug, Deserialize)]
struct ApiOrganization {
    login: String,
}

#[derive(Serialize)]
struct GraphQlRequest<'a> {
    query: &'a str,
    variables: SearchVariables<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SearchVariables<'a> {
    query: &'a str,
    cursor: Option<&'a str>,
    page_size: u32,
}

#[derive(Debug, Deserialize)]
struct GraphQlEnvelope {
    data: Option<SearchData>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
}

#[derive(Debug, Deserialize)]
struct GraphQlError {
    message: String,
}

#[derive(Debug, Deserialize)]
struct SearchData {
    search: SearchConnection,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchConnection {
    issue_count: usize,
    page_info: PageInfo,
    nodes: Vec<ApiPullRequest>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiPullRequest {
    number: u64,
    title: String,
    url: String,
    is_draft: bool,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    additions: u64,
    deletions: u64,
    changed_files: u64,
    base_ref_name: String,
    head_ref_name: String,
    mergeable: String,
    review_decision: Option<String>,
    author: Option<ApiActor>,
    repository: ApiRepository,
    comments: CountConnection,
    labels: LabelConnection,
    reviews: ReviewConnection,
    review_requests: ReviewRequestConnection,
    commits: CommitConnection,
}

#[derive(Debug, Deserialize)]
struct ApiActor {
    login: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiRepository {
    name_with_owner: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CountConnection {
    total_count: usize,
}

#[derive(Debug, Deserialize)]
struct LabelConnection {
    nodes: Vec<ApiLabel>,
}

#[derive(Debug, Deserialize)]
struct ApiLabel {
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewConnection {
    total_count: usize,
    nodes: Vec<ApiReview>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiReview {
    author: Option<ApiActor>,
    state: String,
    submitted_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewRequestConnection {
    total_count: usize,
    nodes: Vec<ApiReviewRequestNode>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiReviewRequestNode {
    requested_reviewer: Option<ApiRequestedReviewer>,
}

#[derive(Debug, Deserialize)]
struct ApiRequestedReviewer {
    #[serde(rename = "__typename")]
    type_name: String,
    login: Option<String>,
    name: Option<String>,
    slug: Option<String>,
    organization: Option<ApiOrganization>,
}

#[derive(Debug, Deserialize)]
struct CommitConnection {
    nodes: Vec<ApiCommitNode>,
}

#[derive(Debug, Deserialize)]
struct ApiCommitNode {
    commit: ApiCommit,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiCommit {
    status_check_rollup: Option<ApiStatusRollup>,
}

#[derive(Debug, Deserialize)]
struct ApiStatusRollup {
    state: String,
}

impl From<ApiPullRequest> for PullRequest {
    fn from(value: ApiPullRequest) -> Self {
        let reviews = value
            .reviews
            .nodes
            .into_iter()
            .map(|review| Review {
                author: review
                    .author
                    .map(|author| author.login)
                    .unwrap_or_else(|| "ghost".into()),
                state: ReviewState::from_api(&review.state),
                submitted_at: review.submitted_at,
            })
            .collect();

        let review_requests = value
            .review_requests
            .nodes
            .into_iter()
            .filter_map(|node| node.requested_reviewer)
            .map(|reviewer| match reviewer.type_name.as_str() {
                "User" => ReviewRequest {
                    name: format!("@{}", reviewer.login.unwrap_or_else(|| "unknown".into())),
                    kind: ReviewerKind::User,
                },
                "Team" => {
                    let org = reviewer
                        .organization
                        .map(|organization| organization.login)
                        .unwrap_or_else(|| "team".into());
                    let slug = reviewer
                        .slug
                        .or(reviewer.name)
                        .unwrap_or_else(|| "unknown".into());
                    ReviewRequest {
                        name: format!("{org}/{slug}"),
                        kind: ReviewerKind::Team,
                    }
                }
                _ => ReviewRequest {
                    name: reviewer
                        .login
                        .or(reviewer.name)
                        .unwrap_or(reviewer.type_name),
                    kind: ReviewerKind::Other,
                },
            })
            .collect();

        let checks = value
            .commits
            .nodes
            .last()
            .and_then(|node| node.commit.status_check_rollup.as_ref())
            .map(|rollup| CheckState::from_api(&rollup.state));

        Self {
            number: value.number,
            title: value.title,
            url: value.url,
            repository: value.repository.name_with_owner,
            author: value
                .author
                .map(|author| author.login)
                .unwrap_or_else(|| "ghost".into()),
            is_draft: value.is_draft,
            created_at: value.created_at,
            updated_at: value.updated_at,
            additions: value.additions,
            deletions: value.deletions,
            changed_files: value.changed_files,
            base_ref: value.base_ref_name,
            head_ref: value.head_ref_name,
            mergeable: MergeableState::from_api(&value.mergeable),
            review_decision: value
                .review_decision
                .as_deref()
                .map(ReviewDecision::from_api),
            reviews,
            total_review_events: value.reviews.total_count,
            review_requests,
            total_review_requests: value.review_requests.total_count,
            comments: value.comments.total_count,
            labels: value
                .labels
                .nodes
                .into_iter()
                .map(|label| label.name)
                .collect(),
            checks,
            requested_via: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_is_permanently_scoped_to_open_pull_requests() {
        let source = include_str!("github.rs");
        assert!(source.contains("is:pr is:open author:"));
        assert!(source.contains("is:pr is:open user-review-requested:"));
        assert!(source.contains("is:pr is:open team-review-requested:"));
    }

    #[test]
    fn parses_every_review_state_and_requested_reviewer_kind() {
        let json = r#"{
          "data": {"search": {
            "issueCount": 1,
            "pageInfo": {"hasNextPage": false, "endCursor": null},
            "nodes": [{
              "number": 42,
              "title": "All review states",
              "url": "https://github.com/acme/app/pull/42",
              "isDraft": true,
              "createdAt": "2026-08-01T00:00:00Z",
              "updatedAt": "2026-08-03T00:00:00Z",
              "additions": 10,
              "deletions": 3,
              "changedFiles": 2,
              "baseRefName": "main",
              "headRefName": "feature",
              "mergeable": "CONFLICTING",
              "reviewDecision": "CHANGES_REQUESTED",
              "author": {"login": "owner"},
              "repository": {"nameWithOwner": "acme/app"},
              "comments": {"totalCount": 4},
              "labels": {"nodes": [{"name": "backend"}]},
              "reviews": {"totalCount": 5, "nodes": [
                {"author":{"login":"a"},"state":"PENDING","submittedAt":null},
                {"author":{"login":"b"},"state":"COMMENTED","submittedAt":"2026-08-01T01:00:00Z"},
                {"author":{"login":"c"},"state":"APPROVED","submittedAt":"2026-08-01T02:00:00Z"},
                {"author":{"login":"d"},"state":"CHANGES_REQUESTED","submittedAt":"2026-08-01T03:00:00Z"},
                {"author":{"login":"e"},"state":"DISMISSED","submittedAt":"2026-08-01T04:00:00Z"}
              ]},
              "reviewRequests": {"totalCount": 2, "nodes": [
                {"requestedReviewer":{"__typename":"User","login":"alice"}},
                {"requestedReviewer":{"__typename":"Team","name":"Core","slug":"core","organization":{"login":"acme"}}}
              ]},
              "commits": {"nodes": [{"commit":{"statusCheckRollup":{"state":"FAILURE"}}}]}
            }]
          }}
        }"#;

        let response: GraphQlEnvelope = serde_json::from_str(json).unwrap();
        let api_pr = response
            .data
            .unwrap()
            .search
            .nodes
            .into_iter()
            .next()
            .unwrap();
        let pr = PullRequest::from(api_pr);

        assert!(pr.is_draft);
        assert_eq!(pr.reviews.len(), 5);
        assert_eq!(pr.review_summary().total_reviewers(), 5);
        assert_eq!(pr.review_requests[0].kind, ReviewerKind::User);
        assert_eq!(pr.review_requests[1].name, "acme/core");
        assert_eq!(pr.checks, Some(CheckState::Failure));
        assert_eq!(pr.mergeable, MergeableState::Conflicting);
    }
}
