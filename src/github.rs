use std::{
    collections::{HashMap, HashSet},
    io::Write,
    process::{Command, Stdio},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::model::{
    CheckState, DashboardData, InvolvementReason, MergeableState, PullRequest, Review,
    ReviewDecision, ReviewRequest, ReviewState, ReviewerKind,
};

const PAGE_SIZE: u32 = 50;
const GITHUB_SEARCH_LIMIT: usize = 1_000;
const COMMIT_DISCOVERY_TTL: Duration = Duration::from_secs(300);

const SEARCH_QUERY: &str = r#"
query KritikonSearch($query: String!, $cursor: String, $pageSize: Int!) {
  search(query: $query, type: ISSUE, first: $pageSize, after: $cursor) {
    issueCount
    pageInfo { hasNextPage endCursor }
    nodes {
      ...PullRequestFields
    }
  }
}
"#;

const NODE_QUERY: &str = r#"
query KritikonNodes($ids: [ID!]!) {
  nodes(ids: $ids) {
    ...PullRequestFields
  }
}
"#;

const PULL_REQUEST_FRAGMENT: &str = r#"
fragment PullRequestFields on PullRequest {
        id
        state
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
        comments(last: 100) { totalCount nodes { author { login } } }
        assignees(first: 100) { nodes { login } }
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
        commits(last: 100) {
          nodes {
            commit {
              authors(first: 10) { nodes { user { login } } }
              statusCheckRollup { state }
            }
          }
        }
}
"#;

const ASSOCIATED_PULL_REQUESTS_QUERY: &str = r#"
query KritikonCommitAssociations($ids: [ID!]!) {
  nodes(ids: $ids) {
    ... on Commit {
      associatedPullRequests(first: 10) {
        nodes { id state }
      }
    }
  }
}
"#;

pub fn fetch_dashboard() -> Result<DashboardData> {
    let viewer = fetch_viewer()?;
    let mut warnings = Vec::new();

    let owned_query = format!("is:pr is:open author:{viewer} sort:updated-desc");
    let mut owned = fetch_search(
        &owned_query,
        None,
        InvolvementDiscovery::None,
        &mut warnings,
    )
    .context("could not load your open pull requests")?;

    let direct_query = format!("is:pr is:open user-review-requested:{viewer} sort:updated-desc");
    let mut review_queue = fetch_search(
        &direct_query,
        Some(format!("@{viewer}")),
        InvolvementDiscovery::None,
        &mut warnings,
    )
    .context("could not load direct review requests")?;

    let teams = match fetch_teams() {
        Ok(teams) => teams,
        Err(error) => {
            warnings.push(format!("Team review requests unavailable ({error:#})"));
            Vec::new()
        }
    };

    for team in &teams {
        let team_name = format!("{}/{}", team.organization.login, team.slug);
        let query = format!("is:pr is:open team-review-requested:{team_name} sort:updated-desc");
        match fetch_search(
            &query,
            Some(team_name.clone()),
            InvolvementDiscovery::None,
            &mut warnings,
        ) {
            Ok(team_prs) => review_queue.extend(team_prs),
            Err(error) => warnings.push(format!(
                "Could not load requests for {team_name} ({error:#})"
            )),
        }
    }

    deduplicate_pull_requests(&mut review_queue);

    let involves_query =
        format!("is:pr is:open involves:{viewer} -author:{viewer} sort:updated-desc");
    let mut involved = fetch_search(
        &involves_query,
        None,
        InvolvementDiscovery::Involves(&viewer),
        &mut warnings,
    )
    .context("could not load open pull requests you participate in")?;

    let reviewed_query =
        format!("is:pr is:open reviewed-by:{viewer} -author:{viewer} sort:updated-desc");
    let reviewed = fetch_search(
        &reviewed_query,
        None,
        InvolvementDiscovery::Reason(InvolvementReason::Reviewed),
        &mut warnings,
    )
    .context("could not load open pull requests you reviewed")?;
    involved.extend(reviewed);

    involved.retain(|pull_request| !pull_request.author.eq_ignore_ascii_case(&viewer));
    deduplicate_pull_requests(&mut involved);
    owned.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    review_queue.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    involved.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

    let mut team_names = teams
        .iter()
        .map(|team| format!("{}/{}", team.organization.login, team.slug))
        .collect::<Vec<_>>();
    team_names.sort();

    Ok(DashboardData {
        viewer,
        review_queue,
        involved,
        owned,
        warnings,
        fetched_at: Utc::now(),
        teams: team_names,
    })
}

#[derive(Debug)]
pub struct CommitInvolvement {
    pub pull_requests: Vec<PullRequest>,
}

pub fn fetch_commit_involvement(viewer: &str) -> Result<CommitInvolvement> {
    let discovery = discover_committed_pull_request_ids(viewer)?;
    let mut pull_requests = fetch_pull_requests_by_ids(&discovery.pull_request_ids)?;
    pull_requests.retain(|pull_request| !pull_request.author.eq_ignore_ascii_case(viewer));
    for pull_request in &mut pull_requests {
        pull_request.add_involvement(InvolvementReason::Committed);
    }
    deduplicate_pull_requests(&mut pull_requests);
    pull_requests.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(CommitInvolvement { pull_requests })
}

pub fn fetch_complete_dashboard() -> Result<DashboardData> {
    let mut dashboard = fetch_dashboard()?;
    match fetch_commit_involvement(&dashboard.viewer) {
        Ok(commit_involvement) => {
            dashboard.involved.extend(commit_involvement.pull_requests);
            deduplicate_pull_requests(&mut dashboard.involved);
            dashboard
                .involved
                .sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        }
        Err(error) => dashboard.warnings.push(format!(
            "Commit-based involvement is temporarily unavailable ({error:#})"
        )),
    }
    Ok(dashboard)
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

#[derive(Debug, Clone, Copy)]
enum InvolvementDiscovery<'a> {
    None,
    Involves(&'a str),
    Reason(InvolvementReason),
}

impl InvolvementDiscovery<'_> {
    fn reasons(self, pull_request: &ApiPullRequest) -> Vec<InvolvementReason> {
        match self {
            Self::None => Vec::new(),
            Self::Reason(reason) => vec![reason],
            Self::Involves(viewer) => {
                let mut reasons = Vec::new();
                if pull_request
                    .assignees
                    .nodes
                    .iter()
                    .any(|actor| actor.login.eq_ignore_ascii_case(viewer))
                {
                    reasons.push(InvolvementReason::Assigned);
                }
                if pull_request.comments.nodes.iter().any(|comment| {
                    comment
                        .author
                        .as_ref()
                        .is_some_and(|actor| actor.login.eq_ignore_ascii_case(viewer))
                }) {
                    reasons.push(InvolvementReason::Commented);
                }
                if pull_request.reviews.nodes.iter().any(|review| {
                    review
                        .author
                        .as_ref()
                        .is_some_and(|actor| actor.login.eq_ignore_ascii_case(viewer))
                }) {
                    reasons.push(InvolvementReason::Reviewed);
                }
                if pull_request.commits.nodes.iter().any(|node| {
                    node.commit.authors.nodes.iter().any(|author| {
                        author
                            .user
                            .as_ref()
                            .is_some_and(|actor| actor.login.eq_ignore_ascii_case(viewer))
                    })
                }) {
                    reasons.push(InvolvementReason::Committed);
                }
                if reasons.is_empty() {
                    reasons.push(InvolvementReason::Mentioned);
                }
                reasons
            }
        }
    }
}

fn fetch_search(
    search: &str,
    requested_via: Option<String>,
    involvement_discovery: InvolvementDiscovery<'_>,
    warnings: &mut Vec<String>,
) -> Result<Vec<PullRequest>> {
    let mut cursor: Option<String> = None;
    let mut pull_requests = Vec::new();
    let mut warned_about_limit = false;

    loop {
        let query = format!("{SEARCH_QUERY}\n{PULL_REQUEST_FRAGMENT}");
        let payload = SearchGraphQlRequest {
            query: &query,
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
            let reasons = involvement_discovery.reasons(&node);
            let mut pull_request = PullRequest::from(node);
            if let Some(via) = &requested_via {
                pull_request.requested_via.push(via.clone());
            }
            for reason in reasons {
                pull_request.add_involvement(reason);
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

fn deduplicate_pull_requests(pull_requests: &mut Vec<PullRequest>) {
    let mut by_url: HashMap<String, PullRequest> = HashMap::new();
    for pull_request in pull_requests.drain(..) {
        match by_url.get_mut(&pull_request.url) {
            Some(existing) => {
                existing.requested_via.extend(pull_request.requested_via);
                for reason in pull_request.involvement {
                    existing.add_involvement(reason);
                }
            }
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

#[derive(Debug, Clone)]
struct CommitDiscovery {
    pull_request_ids: Vec<String>,
}

#[derive(Debug, Clone)]
struct CommitDiscoveryCache {
    viewer: String,
    refreshed_at: Instant,
    discovery: CommitDiscovery,
}

static COMMIT_DISCOVERY_CACHE: OnceLock<Mutex<Option<CommitDiscoveryCache>>> = OnceLock::new();

fn discover_committed_pull_request_ids(viewer: &str) -> Result<CommitDiscovery> {
    let cache = COMMIT_DISCOVERY_CACHE.get_or_init(|| Mutex::new(None));
    if let Some(cached) = cache
        .lock()
        .map_err(|_| anyhow!("commit discovery cache is unavailable"))?
        .as_ref()
        .filter(|cached| {
            cached.viewer.eq_ignore_ascii_case(viewer)
                && cached.refreshed_at.elapsed() < COMMIT_DISCOVERY_TTL
        })
    {
        return Ok(cached.discovery.clone());
    }

    let query = format!("author:{viewer}");
    let query_field = format!("q={query}");
    let output = run_gh(
        &[
            "api",
            "--paginate",
            "--slurp",
            "-X",
            "GET",
            "search/commits",
            "-f",
            &query_field,
            "-f",
            "per_page=100",
            "-f",
            "sort=author-date",
            "-f",
            "order=desc",
        ],
        None,
    )
    .context("could not search commits authored by you")?;
    let pages: Vec<CommitSearchPage> =
        serde_json::from_slice(&output).context("invalid GitHub commit-search response")?;
    let mut seen_commits = HashSet::new();
    let commit_ids = pages
        .into_iter()
        .flat_map(|page| page.items)
        .map(|item| item.node_id)
        .filter(|id| seen_commits.insert(id.clone()))
        .take(GITHUB_SEARCH_LIMIT)
        .collect::<Vec<_>>();

    let mut pull_request_ids = Vec::new();
    for chunk in commit_ids.chunks(100) {
        let payload = NodeGraphQlRequest {
            query: ASSOCIATED_PULL_REQUESTS_QUERY,
            variables: NodeVariables { ids: chunk },
        };
        let body = serde_json::to_vec(&payload)?;
        let output = run_gh(&["api", "graphql", "--input", "-"], Some(&body))?;
        let response: AssociationEnvelope =
            serde_json::from_slice(&output).context("invalid commit-to-pull-request response")?;
        if !response.errors.is_empty() {
            bail!(
                "GitHub GraphQL error: {}",
                graphql_error_text(&response.errors)
            );
        }
        for association in response.data.into_iter().flat_map(|data| data.nodes) {
            let Some(association) = association else {
                continue;
            };
            pull_request_ids.extend(
                association
                    .associated_pull_requests
                    .nodes
                    .into_iter()
                    .filter(|pull_request| pull_request.state == "OPEN")
                    .map(|pull_request| pull_request.id),
            );
        }
    }
    let mut seen_pull_requests = HashSet::new();
    pull_request_ids.retain(|id| seen_pull_requests.insert(id.clone()));
    let discovery = CommitDiscovery { pull_request_ids };
    *cache
        .lock()
        .map_err(|_| anyhow!("commit discovery cache is unavailable"))? =
        Some(CommitDiscoveryCache {
            viewer: viewer.into(),
            refreshed_at: Instant::now(),
            discovery: discovery.clone(),
        });
    Ok(discovery)
}

fn fetch_pull_requests_by_ids(ids: &[String]) -> Result<Vec<PullRequest>> {
    let mut pull_requests = Vec::new();
    let query = format!("{NODE_QUERY}\n{PULL_REQUEST_FRAGMENT}");
    for chunk in ids.chunks(100) {
        let payload = NodeGraphQlRequest {
            query: &query,
            variables: NodeVariables { ids: chunk },
        };
        let body = serde_json::to_vec(&payload)?;
        let output = run_gh(&["api", "graphql", "--input", "-"], Some(&body))?;
        let response: PullRequestNodesEnvelope =
            serde_json::from_slice(&output).context("invalid pull-request node response")?;
        if !response.errors.is_empty() {
            bail!(
                "GitHub GraphQL error: {}",
                graphql_error_text(&response.errors)
            );
        }
        pull_requests.extend(
            response
                .data
                .into_iter()
                .flat_map(|data| data.nodes)
                .flatten()
                .filter(|pull_request| pull_request.state == "OPEN")
                .map(PullRequest::from),
        );
    }
    Ok(pull_requests)
}

fn graphql_error_text(errors: &[GraphQlError]) -> String {
    errors
        .iter()
        .map(|error| error.message.as_str())
        .collect::<Vec<_>>()
        .join("; ")
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
struct SearchGraphQlRequest<'a> {
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

#[derive(Serialize)]
struct NodeGraphQlRequest<'a> {
    query: &'a str,
    variables: NodeVariables<'a>,
}

#[derive(Serialize)]
struct NodeVariables<'a> {
    ids: &'a [String],
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
struct CommitSearchPage {
    items: Vec<CommitSearchItem>,
}

#[derive(Debug, Deserialize)]
struct CommitSearchItem {
    node_id: String,
}

#[derive(Debug, Deserialize)]
struct AssociationEnvelope {
    data: Option<AssociationData>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
}

#[derive(Debug, Deserialize)]
struct AssociationData {
    nodes: Vec<Option<ApiCommitAssociation>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiCommitAssociation {
    associated_pull_requests: AssociatedPullRequestConnection,
}

#[derive(Debug, Deserialize)]
struct AssociatedPullRequestConnection {
    nodes: Vec<AssociatedPullRequest>,
}

#[derive(Debug, Deserialize)]
struct AssociatedPullRequest {
    id: String,
    state: String,
}

#[derive(Debug, Deserialize)]
struct PullRequestNodesEnvelope {
    data: Option<PullRequestNodesData>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
}

#[derive(Debug, Deserialize)]
struct PullRequestNodesData {
    nodes: Vec<Option<ApiPullRequest>>,
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
    state: String,
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
    comments: CommentConnection,
    assignees: ActorConnection,
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
struct CommentConnection {
    total_count: usize,
    nodes: Vec<ApiComment>,
}

#[derive(Debug, Deserialize)]
struct ApiComment {
    author: Option<ApiActor>,
}

#[derive(Debug, Deserialize)]
struct ActorConnection {
    nodes: Vec<ApiActor>,
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
    authors: CommitAuthorConnection,
    status_check_rollup: Option<ApiStatusRollup>,
}

#[derive(Debug, Deserialize)]
struct CommitAuthorConnection {
    nodes: Vec<ApiCommitAuthor>,
}

#[derive(Debug, Deserialize)]
struct ApiCommitAuthor {
    user: Option<ApiActor>,
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
            involvement: Vec::new(),
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
        assert!(source.contains("is:pr is:open involves:"));
        assert!(source.contains("is:pr is:open reviewed-by:"));
    }

    #[test]
    fn parses_every_review_state_and_requested_reviewer_kind() {
        let json = r#"{
          "data": {"search": {
            "issueCount": 1,
            "pageInfo": {"hasNextPage": false, "endCursor": null},
            "nodes": [{
              "number": 42,
              "state": "OPEN",
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
              "comments": {"totalCount": 4, "nodes": [{"author":{"login":"alice"}}]},
              "assignees": {"nodes": [{"login":"alice"}]},
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
              "commits": {"nodes": [{"commit":{"authors":{"nodes":[{"user":{"login":"alice"}}]},"statusCheckRollup":{"state":"FAILURE"}}}]}
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

    #[test]
    fn derives_exact_involvement_reasons_from_graphql_data() {
        let json = r#"{
          "number": 7,
          "state": "OPEN",
          "title": "Participated",
          "url": "https://github.com/acme/app/pull/7",
          "isDraft": false,
          "createdAt": "2026-08-01T00:00:00Z",
          "updatedAt": "2026-08-03T00:00:00Z",
          "additions": 1,
          "deletions": 1,
          "changedFiles": 1,
          "baseRefName": "main",
          "headRefName": "feature",
          "mergeable": "MERGEABLE",
          "reviewDecision": null,
          "author": {"login": "owner"},
          "repository": {"nameWithOwner": "acme/app"},
          "comments": {"totalCount": 1, "nodes": [{"author":{"login":"viewer"}}]},
          "assignees": {"nodes": [{"login":"viewer"}]},
          "labels": {"nodes": []},
          "reviews": {"totalCount": 1, "nodes": [
            {"author":{"login":"viewer"},"state":"APPROVED","submittedAt":"2026-08-03T00:00:00Z"}
          ]},
          "reviewRequests": {"totalCount": 0, "nodes": []},
          "commits": {"nodes": [{"commit":{"authors":{"nodes":[{"user":{"login":"viewer"}}]},"statusCheckRollup":null}}]}
        }"#;
        let pull_request: ApiPullRequest = serde_json::from_str(json).unwrap();
        assert_eq!(
            InvolvementDiscovery::Involves("viewer").reasons(&pull_request),
            vec![
                InvolvementReason::Assigned,
                InvolvementReason::Commented,
                InvolvementReason::Reviewed,
                InvolvementReason::Committed,
            ]
        );
    }
}
