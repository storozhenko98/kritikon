use std::{
    collections::{HashMap, HashSet},
    io::Write,
    process::{Command, Stdio},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};

use crate::model::{
    CheckState, DashboardData, InvolvementReason, MergeableState, PullRequest, Review,
    ReviewDecision, ReviewRequest, ReviewState, ReviewerKind,
};

const PAGE_SIZE: u32 = 50;
const GITHUB_SEARCH_LIMIT: usize = 1_000;
const DETAIL_BATCH_SIZE: usize = 50;
const COMMIT_DISCOVERY_TTL: Duration = Duration::from_secs(30 * 60);
const COMMIT_FULL_RECONCILE_TTL: Duration = Duration::from_secs(6 * 60 * 60);
const DETAIL_RECONCILE_TTL: Duration = Duration::from_secs(30 * 60);
const CACHE_ENTRY_TTL: Duration = Duration::from_secs(2 * 60 * 60);
const RECONCILE_RATE_FLOOR: u32 = 500;
const LOW_RATE_WARNING: u32 = 250;
const REFRESH_RATE_FLOOR: u32 = 50;

const SEARCH_INDEX_QUERY: &str = r#"
query KritikonSearchIndex($query: String!, $cursor: String, $pageSize: Int!) {
  search(query: $query, type: ISSUE, first: $pageSize, after: $cursor) {
    issueCount
    pageInfo { hasNextPage endCursor }
    nodes {
      ... on PullRequest { id updatedAt }
    }
  }
  rateLimit { cost remaining resetAt }
}
"#;

const NODE_QUERY: &str = r#"
query KritikonNodes($ids: [ID!]!) {
  nodes(ids: $ids) {
    ...PullRequestFields
  }
  rateLimit { cost remaining resetAt }
}
"#;

const STATUS_QUERY: &str = r#"
query KritikonStatuses($ids: [ID!]!) {
  nodes(ids: $ids) {
    ... on PullRequest {
      id
      state
      updatedAt
      mergeable
      reviewDecision
      commits(last: 1) {
        nodes { commit { statusCheckRollup { state } } }
      }
    }
  }
  rateLimit { cost remaining resetAt }
}
"#;

const NODE_INDEX_QUERY: &str = r#"
query KritikonNodeIndex($ids: [ID!]!) {
  nodes(ids: $ids) {
    ... on PullRequest { id state updatedAt }
  }
  rateLimit { cost remaining resetAt }
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
  rateLimit { cost remaining resetAt }
}
"#;

pub fn fetch_dashboard() -> Result<DashboardData> {
    let viewer = fetch_viewer()?;
    ensure_rate_limit_snapshot();
    if let Some(dashboard) = cached_dashboard_when_rate_limited() {
        return Ok(dashboard);
    }

    match fetch_dashboard_uncached(viewer) {
        Ok(dashboard) => Ok(dashboard),
        Err(_error) if deferred_rate_limit().is_some() => {
            cached_dashboard_when_rate_limited().ok_or(_error)
        }
        Err(error) => Err(error),
    }
}

fn fetch_dashboard_uncached(viewer: String) -> Result<DashboardData> {
    let mut warnings = Vec::new();

    let owned_query = format!("is:pr is:open author:{viewer} sort:updated-desc");
    let owned_hits = fetch_search_index(&owned_query, &mut warnings)
        .context("could not load your open pull requests")?;

    let direct_query = format!("is:pr is:open user-review-requested:{viewer} sort:updated-desc");
    let direct_hits = fetch_search_index(&direct_query, &mut warnings)
        .context("could not load direct review requests")?;
    let mut review_sources = HashMap::<String, Vec<String>>::new();
    record_review_sources(&mut review_sources, &direct_hits, format!("@{viewer}"));

    let teams = match fetch_teams() {
        Ok(teams) => teams,
        Err(error) => {
            warnings.push(format!("Team review requests unavailable ({error:#})"));
            Vec::new()
        }
    };

    let mut review_hits = direct_hits;
    for team in &teams {
        let team_name = format!("{}/{}", team.organization.login, team.slug);
        let query = format!("is:pr is:open team-review-requested:{team_name} sort:updated-desc");
        match fetch_search_index(&query, &mut warnings) {
            Ok(team_hits) => {
                record_review_sources(&mut review_sources, &team_hits, team_name);
                review_hits.extend(team_hits);
            }
            Err(error) => warnings.push(format!(
                "Could not load requests for {team_name} ({error:#})"
            )),
        }
    }
    deduplicate_indexes(&mut review_hits);

    let involves_query =
        format!("is:pr is:open involves:{viewer} -author:{viewer} sort:updated-desc");
    let mut involved_hits = fetch_search_index(&involves_query, &mut warnings)
        .context("could not load open pull requests you participate in")?;

    let reviewed_query =
        format!("is:pr is:open reviewed-by:{viewer} -author:{viewer} sort:updated-desc");
    let reviewed_hits = fetch_search_index(&reviewed_query, &mut warnings)
        .context("could not load open pull requests you reviewed")?;
    let reviewed_ids = reviewed_hits
        .iter()
        .map(|hit| hit.id.clone())
        .collect::<HashSet<_>>();
    involved_hits.extend(reviewed_hits);
    deduplicate_indexes(&mut involved_hits);

    let mut all_hits = Vec::new();
    all_hits.extend(owned_hits.iter().cloned());
    all_hits.extend(review_hits.iter().cloned());
    all_hits.extend(involved_hits.iter().cloned());
    deduplicate_indexes(&mut all_hits);
    let hydrated = hydrate_pull_requests(&viewer, &all_hits, &mut warnings)?;

    let mut owned = materialize(&owned_hits, &hydrated);
    for pull_request in &mut owned {
        pull_request.involvement.clear();
    }

    let mut review_queue = review_hits
        .iter()
        .filter_map(|hit| {
            let mut pull_request = hydrated.get(&hit.id)?.clone();
            pull_request.involvement.clear();
            pull_request.requested_via = review_sources.get(&hit.id).cloned().unwrap_or_default();
            Some(pull_request)
        })
        .collect::<Vec<_>>();

    let mut involved = involved_hits
        .iter()
        .filter_map(|hit| {
            let mut pull_request = hydrated.get(&hit.id)?.clone();
            if reviewed_ids.contains(&hit.id) {
                pull_request.add_involvement(InvolvementReason::Reviewed);
            }
            Some(pull_request)
        })
        .collect::<Vec<_>>();

    involved.retain(|pull_request| !pull_request.author.eq_ignore_ascii_case(&viewer));
    deduplicate_pull_requests(&mut involved);
    owned.sort_by_key(|pull_request| std::cmp::Reverse(pull_request.updated_at));
    review_queue.sort_by_key(|pull_request| std::cmp::Reverse(pull_request.updated_at));
    involved.sort_by_key(|pull_request| std::cmp::Reverse(pull_request.updated_at));

    let mut team_names = teams
        .iter()
        .map(|team| format!("{}/{}", team.organization.login, team.slug))
        .collect::<Vec<_>>();
    team_names.sort();

    append_rate_warning(&mut warnings);

    let dashboard = DashboardData {
        viewer,
        review_queue,
        involved,
        owned,
        warnings,
        fetched_at: Utc::now(),
        teams: team_names,
    };
    if let Ok(mut state) = api_state().lock() {
        state.dashboard = Some(dashboard.clone());
    }
    Ok(dashboard)
}

#[derive(Debug)]
pub struct CommitInvolvement {
    pub pull_requests: Vec<PullRequest>,
    pub warnings: Vec<String>,
}

pub fn fetch_commit_involvement(viewer: &str) -> Result<CommitInvolvement> {
    if let Some(limit) = deferred_rate_limit() {
        bail!(
            "GitHub GraphQL budget is protected at {} points; commit discovery will resume after {}",
            limit.remaining,
            reset_time_label(limit.reset_at)
        );
    }
    let discovery = discover_committed_pull_request_ids(viewer)?;
    let indexes = fetch_pull_request_indexes_by_ids(&discovery.pull_request_ids)?;
    let mut warnings = Vec::new();
    let hydrated = hydrate_pull_requests(viewer, &indexes, &mut warnings)?;
    let mut pull_requests = materialize(&indexes, &hydrated);
    pull_requests.retain(|pull_request| !pull_request.author.eq_ignore_ascii_case(viewer));
    for pull_request in &mut pull_requests {
        pull_request.add_involvement(InvolvementReason::Committed);
    }
    deduplicate_pull_requests(&mut pull_requests);
    pull_requests.sort_by_key(|pull_request| std::cmp::Reverse(pull_request.updated_at));
    Ok(CommitInvolvement {
        pull_requests,
        warnings,
    })
}

pub fn fetch_complete_dashboard() -> Result<DashboardData> {
    let mut dashboard = fetch_dashboard()?;
    match fetch_commit_involvement(&dashboard.viewer) {
        Ok(commit_involvement) => {
            dashboard.involved.extend(commit_involvement.pull_requests);
            dashboard.warnings.extend(commit_involvement.warnings);
            deduplicate_pull_requests(&mut dashboard.involved);
            dashboard
                .involved
                .sort_by_key(|pull_request| std::cmp::Reverse(pull_request.updated_at));
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
    let mut state = api_state()
        .lock()
        .map_err(|_| anyhow!("GitHub identity cache is unavailable"))?;
    if state
        .viewer
        .as_deref()
        .is_some_and(|viewer| !viewer.eq_ignore_ascii_case(&user.login))
    {
        state.pull_requests.clear();
        state.dashboard = None;
        state.rate_limit = None;
    }
    state.viewer = Some(user.login.clone());
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

fn participation_reasons(pull_request: &ApiPullRequest, viewer: &str) -> Vec<InvolvementReason> {
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

fn fetch_search_index(
    search: &str,
    warnings: &mut Vec<String>,
) -> Result<Vec<ApiPullRequestIndex>> {
    let mut cursor: Option<String> = None;
    let mut pull_requests = Vec::new();
    let mut warned_about_limit = false;

    loop {
        let payload = SearchGraphQlRequest {
            query: SEARCH_INDEX_QUERY,
            variables: SearchVariables {
                query: search,
                cursor: cursor.as_deref(),
                page_size: PAGE_SIZE,
            },
        };
        let body = serde_json::to_vec(&payload)?;
        let output = run_graphql(&body)?;
        let response: SearchGraphQlEnvelope =
            serde_json::from_slice(&output).context("invalid GitHub GraphQL search response")?;

        if !response.errors.is_empty() {
            bail!(
                "GitHub GraphQL error: {}",
                graphql_error_text(&response.errors)
            );
        }
        let data = response
            .data
            .ok_or_else(|| anyhow!("GitHub returned no data"))?;
        record_rate_limit(data.rate_limit);
        let connection = data.search;

        if connection.issue_count > GITHUB_SEARCH_LIMIT && !warned_about_limit {
            warnings.push(format!(
                "GitHub search reports {} matching PRs; only its first {GITHUB_SEARCH_LIMIT} results are available",
                connection.issue_count
            ));
            warned_about_limit = true;
        }

        pull_requests.extend(connection.nodes.into_iter().flatten());

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

fn record_review_sources(
    sources: &mut HashMap<String, Vec<String>>,
    hits: &[ApiPullRequestIndex],
    requested_via: String,
) {
    for hit in hits {
        let entry = sources.entry(hit.id.clone()).or_default();
        if !entry.contains(&requested_via) {
            entry.push(requested_via.clone());
        }
    }
}

fn deduplicate_indexes(indexes: &mut Vec<ApiPullRequestIndex>) {
    let mut positions = HashMap::<String, usize>::new();
    let mut deduplicated = Vec::<ApiPullRequestIndex>::new();
    for index in indexes.drain(..) {
        match positions.get(&index.id).copied() {
            Some(position) if index.updated_at > deduplicated[position].updated_at => {
                deduplicated[position] = index;
            }
            Some(_) => {}
            None => {
                positions.insert(index.id.clone(), deduplicated.len());
                deduplicated.push(index);
            }
        }
    }
    *indexes = deduplicated;
}

fn materialize(
    indexes: &[ApiPullRequestIndex],
    hydrated: &HashMap<String, PullRequest>,
) -> Vec<PullRequest> {
    indexes
        .iter()
        .filter_map(|index| hydrated.get(&index.id).cloned())
        .collect()
}

#[derive(Debug, Clone)]
struct CachedPullRequest {
    updated_at: DateTime<Utc>,
    details_refreshed_at: Instant,
    last_seen_at: Instant,
    pull_request: PullRequest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetailNeed {
    Required,
    Reconcile,
    Cached,
}

fn detail_need(
    index: &ApiPullRequestIndex,
    cached: Option<&CachedPullRequest>,
    reconcile_allowed: bool,
) -> DetailNeed {
    let Some(cached) = cached else {
        return DetailNeed::Required;
    };
    if cached.updated_at != index.updated_at {
        return DetailNeed::Required;
    }
    if reconcile_allowed && cached.details_refreshed_at.elapsed() >= DETAIL_RECONCILE_TTL {
        return DetailNeed::Reconcile;
    }
    DetailNeed::Cached
}

fn should_replace_cached_entry(existing: &CachedPullRequest, incoming: &CachedPullRequest) -> bool {
    incoming.updated_at > existing.updated_at
        || (incoming.updated_at == existing.updated_at
            && (incoming.details_refreshed_at > existing.details_refreshed_at
                || (incoming.details_refreshed_at == existing.details_refreshed_at
                    && incoming.last_seen_at >= existing.last_seen_at)))
}

#[derive(Debug, Clone)]
struct RateLimitSnapshot {
    remaining: u32,
    reset_at: DateTime<Utc>,
}

#[derive(Debug, Default)]
struct ApiState {
    viewer: Option<String>,
    pull_requests: HashMap<String, CachedPullRequest>,
    rate_limit: Option<RateLimitSnapshot>,
    dashboard: Option<DashboardData>,
}

static API_STATE: OnceLock<Mutex<ApiState>> = OnceLock::new();

fn api_state() -> &'static Mutex<ApiState> {
    API_STATE.get_or_init(|| Mutex::new(ApiState::default()))
}

fn deferred_rate_limit() -> Option<RateLimitSnapshot> {
    api_state()
        .lock()
        .ok()
        .and_then(|state| state.rate_limit.clone())
        .filter(|limit| rate_limit_requires_deferral(limit, Utc::now()))
}

fn rate_limit_requires_deferral(limit: &RateLimitSnapshot, now: DateTime<Utc>) -> bool {
    limit.remaining < REFRESH_RATE_FLOOR && limit.reset_at > now
}

fn cached_dashboard_when_rate_limited() -> Option<DashboardData> {
    let limit = deferred_rate_limit()?;
    let dashboard = api_state().lock().ok()?.dashboard.clone()?;
    Some(paused_dashboard(dashboard, &limit))
}

fn paused_dashboard(mut dashboard: DashboardData, limit: &RateLimitSnapshot) -> DashboardData {
    dashboard
        .warnings
        .retain(|warning| !warning.starts_with("GitHub GraphQL budget is low"));
    dashboard.warnings.push(format!(
        "GitHub GraphQL budget is low ({} points; resets {}); refresh is paused and the last complete dashboard remains visible",
        limit.remaining,
        reset_time_label(limit.reset_at)
    ));
    dashboard
}

fn hydrate_pull_requests(
    viewer: &str,
    indexes: &[ApiPullRequestIndex],
    warnings: &mut Vec<String>,
) -> Result<HashMap<String, PullRequest>> {
    // Search membership stays live on every refresh, but rich PR details are
    // reused until updatedAt changes. A separate one-commit status query keeps
    // CI/merge state fresh without reloading comments and reviews.
    if indexes.is_empty() {
        return Ok(HashMap::new());
    }

    let now = Instant::now();
    let (mut cached, reconcile_allowed) = {
        let state = api_state()
            .lock()
            .map_err(|_| anyhow!("pull-request cache is unavailable"))?;
        let cached = indexes
            .iter()
            .filter_map(|index| {
                state
                    .pull_requests
                    .get(&index.id)
                    .cloned()
                    .map(|pull_request| (index.id.clone(), pull_request))
            })
            .collect::<HashMap<_, _>>();
        let reconcile_allowed = state
            .rate_limit
            .as_ref()
            .is_none_or(|limit| limit.remaining >= RECONCILE_RATE_FLOOR);
        (cached, reconcile_allowed)
    };

    let mut full_refresh_ids = indexes
        .iter()
        .filter(|index| {
            detail_need(index, cached.get(&index.id), reconcile_allowed) == DetailNeed::Required
        })
        .map(|index| index.id.clone())
        .collect::<Vec<_>>();
    let mut mandatory_refresh_ids = full_refresh_ids.iter().cloned().collect::<HashSet<_>>();
    let current_ids = indexes
        .iter()
        .filter(|index| {
            detail_need(index, cached.get(&index.id), reconcile_allowed) != DetailNeed::Required
        })
        .map(|index| index.id.clone())
        .collect::<Vec<_>>();

    if !current_ids.is_empty() {
        match fetch_pull_request_statuses_by_ids(&current_ids) {
            Ok(statuses) => {
                let statuses = statuses
                    .into_iter()
                    .map(|status| (status.id.clone(), status))
                    .collect::<HashMap<_, _>>();
                for id in &current_ids {
                    let Some(entry) = cached.get_mut(id) else {
                        continue;
                    };
                    let Some(status) = statuses.get(id) else {
                        continue;
                    };
                    if status.state != "OPEN" {
                        cached.remove(id);
                        continue;
                    }
                    if status.updated_at != entry.updated_at {
                        mandatory_refresh_ids.insert(id.clone());
                        full_refresh_ids.push(id.clone());
                        continue;
                    }
                    apply_status(&mut entry.pull_request, status);
                    entry.last_seen_at = now;
                    let index = indexes.iter().find(|index| index.id == *id);
                    if index.is_some_and(|index| {
                        detail_need(index, Some(entry), reconcile_allowed) == DetailNeed::Reconcile
                    }) {
                        full_refresh_ids.push(id.clone());
                    }
                }
            }
            Err(error) => warnings.push(format!(
                "Live CI and merge status refresh unavailable; cached status retained ({error:#})"
            )),
        }
    }

    deduplicate_strings(&mut full_refresh_ids);
    if !reconcile_allowed {
        full_refresh_ids.retain(|id| mandatory_refresh_ids.contains(id));
    }

    let refreshed = fetch_pull_requests_by_ids(viewer, &full_refresh_ids)?;
    for id in &full_refresh_ids {
        cached.remove(id);
    }
    for (id, pull_request) in refreshed {
        cached.insert(
            id,
            CachedPullRequest {
                updated_at: pull_request.updated_at,
                details_refreshed_at: now,
                last_seen_at: now,
                pull_request,
            },
        );
    }

    let active_ids = indexes
        .iter()
        .map(|index| index.id.as_str())
        .collect::<HashSet<_>>();
    cached.retain(|id, _| active_ids.contains(id.as_str()));

    let hydrated = cached
        .iter()
        .map(|(id, entry)| (id.clone(), entry.pull_request.clone()))
        .collect::<HashMap<_, _>>();
    let active_cache_ids = hydrated.keys().cloned().collect::<HashSet<_>>();

    let mut state = api_state()
        .lock()
        .map_err(|_| anyhow!("pull-request cache is unavailable"))?;
    if state
        .viewer
        .as_deref()
        .is_some_and(|cached_viewer| !cached_viewer.eq_ignore_ascii_case(viewer))
    {
        state.pull_requests.clear();
    }
    state.viewer = Some(viewer.into());
    for (id, entry) in cached {
        if state
            .pull_requests
            .get(&id)
            .is_none_or(|existing| should_replace_cached_entry(existing, &entry))
        {
            state.pull_requests.insert(id, entry);
        }
    }
    state.pull_requests.retain(|id, entry| {
        entry.last_seen_at.elapsed() < CACHE_ENTRY_TTL || active_cache_ids.contains(id)
    });

    Ok(hydrated)
}

fn apply_status(pull_request: &mut PullRequest, status: &ApiPullRequestStatus) {
    pull_request.mergeable = MergeableState::from_api(&status.mergeable);
    pull_request.review_decision = status
        .review_decision
        .as_deref()
        .map(ReviewDecision::from_api);
    pull_request.checks = status
        .commits
        .nodes
        .last()
        .and_then(|node| node.commit.status_check_rollup.as_ref())
        .map(|rollup| CheckState::from_api(&rollup.state));
}

fn deduplicate_strings(values: &mut Vec<String>) {
    let mut seen = HashSet::new();
    values.retain(|value| seen.insert(value.clone()));
}

fn record_rate_limit(rate_limit: Option<ApiRateLimit>) {
    let Some(rate_limit) = rate_limit else {
        return;
    };
    if let Ok(mut state) = api_state().lock() {
        state.rate_limit = Some(RateLimitSnapshot {
            remaining: rate_limit.remaining,
            reset_at: rate_limit.reset_at,
        });
    }
}

fn append_rate_warning(warnings: &mut Vec<String>) {
    let limit = api_state()
        .lock()
        .ok()
        .and_then(|state| state.rate_limit.clone());
    if let Some(limit) = limit.filter(|limit| limit.remaining < LOW_RATE_WARNING) {
        warnings.push(format!(
            "GitHub GraphQL budget is low ({} points; resets {}); unchanged details are being served from cache",
            limit.remaining,
            reset_time_label(limit.reset_at)
        ));
    }
}

fn ensure_rate_limit_snapshot() {
    if api_state()
        .lock()
        .ok()
        .is_some_and(|state| state.rate_limit.is_some())
    {
        return;
    }
    let Ok(output) = run_gh(&["api", "rate_limit"], None) else {
        return;
    };
    let Ok(response) = serde_json::from_slice::<RestRateLimitResponse>(&output) else {
        return;
    };
    let Some(reset_at) = DateTime::from_timestamp(response.resources.graphql.reset, 0) else {
        return;
    };
    if let Ok(mut state) = api_state().lock() {
        state.rate_limit = Some(RateLimitSnapshot {
            remaining: response.resources.graphql.remaining,
            reset_at,
        });
    }
}

fn reset_time_label(reset_at: DateTime<Utc>) -> String {
    reset_at
        .with_timezone(&Local)
        .format("%H:%M:%S %Z")
        .to_string()
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
    full_refreshed_at: Instant,
    commit_ids: HashSet<String>,
    discovery: CommitDiscovery,
}

static COMMIT_DISCOVERY_CACHE: OnceLock<Mutex<Option<CommitDiscoveryCache>>> = OnceLock::new();

fn discover_committed_pull_request_ids(viewer: &str) -> Result<CommitDiscovery> {
    let cache = COMMIT_DISCOVERY_CACHE.get_or_init(|| Mutex::new(None));
    let cached = cache
        .lock()
        .map_err(|_| anyhow!("commit discovery cache is unavailable"))?
        .as_ref()
        .filter(|cached| cached.viewer.eq_ignore_ascii_case(viewer))
        .cloned();
    if let Some(cached) = cached
        .as_ref()
        .filter(|cached| cached.refreshed_at.elapsed() < COMMIT_DISCOVERY_TTL)
    {
        return Ok(cached.discovery.clone());
    }

    // The first pass preserves the historical 1,000-commit coverage. Normal
    // refreshes examine only the newest page and resolve associations for IDs
    // that were not present in the previous scan.
    let full_scan = cached
        .as_ref()
        .is_none_or(|cached| cached.full_refreshed_at.elapsed() >= COMMIT_FULL_RECONCILE_TTL);
    let mut commit_ids = fetch_commit_ids(viewer, full_scan)?;
    if let Some(cached) = &cached
        && !full_scan
    {
        commit_ids.retain(|id| !cached.commit_ids.contains(id));
    }

    let associated_ids = fetch_associated_pull_request_ids(&commit_ids)?;
    let mut pull_request_ids = if full_scan {
        associated_ids
    } else {
        let mut ids = cached
            .as_ref()
            .map(|cached| cached.discovery.pull_request_ids.clone())
            .unwrap_or_default();
        ids.extend(associated_ids);
        ids
    };
    deduplicate_strings(&mut pull_request_ids);

    let mut known_commit_ids = if full_scan {
        HashSet::new()
    } else {
        cached
            .as_ref()
            .map(|cached| cached.commit_ids.clone())
            .unwrap_or_default()
    };
    known_commit_ids.extend(commit_ids);
    let now = Instant::now();
    let discovery = CommitDiscovery { pull_request_ids };
    *cache
        .lock()
        .map_err(|_| anyhow!("commit discovery cache is unavailable"))? =
        Some(CommitDiscoveryCache {
            viewer: viewer.into(),
            refreshed_at: now,
            full_refreshed_at: if full_scan {
                now
            } else {
                cached
                    .as_ref()
                    .map_or(now, |cached| cached.full_refreshed_at)
            },
            commit_ids: known_commit_ids,
            discovery: discovery.clone(),
        });
    Ok(discovery)
}

fn fetch_commit_ids(viewer: &str, full_scan: bool) -> Result<Vec<String>> {
    let query = format!("author:{viewer}");
    let query_field = format!("q={query}");
    let full_args = [
        "api",
        "--paginate",
        "--slurp",
        "-X",
        "GET",
        "search/commits",
        "-f",
        query_field.as_str(),
        "-f",
        "per_page=100",
        "-f",
        "sort=author-date",
        "-f",
        "order=desc",
    ];
    let incremental_args = [
        "api",
        "-X",
        "GET",
        "search/commits",
        "-f",
        query_field.as_str(),
        "-f",
        "per_page=100",
        "-f",
        "sort=author-date",
        "-f",
        "order=desc",
    ];
    let output = run_gh(
        if full_scan {
            &full_args
        } else {
            &incremental_args
        },
        None,
    )
    .context("could not search commits authored by you")?;
    let pages = if full_scan {
        serde_json::from_slice::<Vec<CommitSearchPage>>(&output)
            .context("invalid paginated GitHub commit-search response")?
    } else {
        vec![
            serde_json::from_slice::<CommitSearchPage>(&output)
                .context("invalid GitHub commit-search response")?,
        ]
    };
    let mut seen_commits = HashSet::new();
    Ok(pages
        .into_iter()
        .flat_map(|page| page.items)
        .map(|item| item.node_id)
        .filter(|id| seen_commits.insert(id.clone()))
        .take(GITHUB_SEARCH_LIMIT)
        .collect())
}

fn fetch_associated_pull_request_ids(commit_ids: &[String]) -> Result<Vec<String>> {
    let mut pull_request_ids = Vec::new();
    for chunk in commit_ids.chunks(100) {
        let payload = NodeGraphQlRequest {
            query: ASSOCIATED_PULL_REQUESTS_QUERY,
            variables: NodeVariables { ids: chunk },
        };
        let body = serde_json::to_vec(&payload)?;
        let output = run_graphql(&body)?;
        let response: AssociationEnvelope =
            serde_json::from_slice(&output).context("invalid commit-to-pull-request response")?;
        if !response.errors.is_empty() {
            bail!(
                "GitHub GraphQL error: {}",
                graphql_error_text(&response.errors)
            );
        }
        let data = response
            .data
            .ok_or_else(|| anyhow!("GitHub returned no commit-association data"))?;
        record_rate_limit(data.rate_limit);
        for association in data.nodes {
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
    deduplicate_strings(&mut pull_request_ids);
    Ok(pull_request_ids)
}

fn fetch_pull_request_indexes_by_ids(ids: &[String]) -> Result<Vec<ApiPullRequestIndex>> {
    let mut indexes = Vec::new();
    for chunk in ids.chunks(100) {
        let payload = NodeGraphQlRequest {
            query: NODE_INDEX_QUERY,
            variables: NodeVariables { ids: chunk },
        };
        let body = serde_json::to_vec(&payload)?;
        let output = run_graphql(&body)?;
        let response: PullRequestIndexEnvelope =
            serde_json::from_slice(&output).context("invalid pull-request index response")?;
        if !response.errors.is_empty() {
            bail!(
                "GitHub GraphQL error: {}",
                graphql_error_text(&response.errors)
            );
        }
        let data = response
            .data
            .ok_or_else(|| anyhow!("GitHub returned no pull-request index data"))?;
        record_rate_limit(data.rate_limit);
        indexes.extend(
            data.nodes
                .into_iter()
                .flatten()
                .filter(|pull_request| pull_request.state.as_deref() == Some("OPEN")),
        );
    }
    Ok(indexes)
}

fn fetch_pull_request_statuses_by_ids(ids: &[String]) -> Result<Vec<ApiPullRequestStatus>> {
    let mut statuses = Vec::new();
    for chunk in ids.chunks(100) {
        let payload = NodeGraphQlRequest {
            query: STATUS_QUERY,
            variables: NodeVariables { ids: chunk },
        };
        let body = serde_json::to_vec(&payload)?;
        let output = run_graphql(&body)?;
        let response: PullRequestStatusEnvelope =
            serde_json::from_slice(&output).context("invalid pull-request status response")?;
        if !response.errors.is_empty() {
            bail!(
                "GitHub GraphQL error: {}",
                graphql_error_text(&response.errors)
            );
        }
        let data = response
            .data
            .ok_or_else(|| anyhow!("GitHub returned no pull-request status data"))?;
        record_rate_limit(data.rate_limit);
        statuses.extend(data.nodes.into_iter().flatten());
    }
    Ok(statuses)
}

fn fetch_pull_requests_by_ids(viewer: &str, ids: &[String]) -> Result<Vec<(String, PullRequest)>> {
    let mut pull_requests = Vec::new();
    let query = format!("{NODE_QUERY}\n{PULL_REQUEST_FRAGMENT}");
    for chunk in ids.chunks(DETAIL_BATCH_SIZE) {
        let payload = NodeGraphQlRequest {
            query: &query,
            variables: NodeVariables { ids: chunk },
        };
        let body = serde_json::to_vec(&payload)?;
        let output = run_graphql(&body)?;
        let response: PullRequestNodesEnvelope =
            serde_json::from_slice(&output).context("invalid pull-request node response")?;
        if !response.errors.is_empty() {
            bail!(
                "GitHub GraphQL error: {}",
                graphql_error_text(&response.errors)
            );
        }
        let data = response
            .data
            .ok_or_else(|| anyhow!("GitHub returned no pull-request detail data"))?;
        record_rate_limit(data.rate_limit);
        pull_requests.extend(
            data.nodes
                .into_iter()
                .flatten()
                .filter_map(|api_pull_request| {
                    if api_pull_request.state != "OPEN" {
                        return None;
                    }
                    let id = api_pull_request.id.clone();
                    let reasons = participation_reasons(&api_pull_request, viewer);
                    let mut pull_request = PullRequest::from(api_pull_request);
                    for reason in reasons {
                        pull_request.add_involvement(reason);
                    }
                    Some((id, pull_request))
                }),
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

fn run_graphql(input: &[u8]) -> Result<Vec<u8>> {
    // Keep enough budget for a later changed PR and avoid turning a fast poll
    // interval into an hour-long hard failure. The REST preflight seeds this
    // state before the first GraphQL request in a fresh process.
    if let Some(limit) = deferred_rate_limit() {
        bail!(
            "GitHub GraphQL budget is protected at {} points; requests resume after {}",
            limit.remaining,
            reset_time_label(limit.reset_at)
        );
    }
    run_gh(&["api", "graphql", "--input", "-"], Some(input))
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
struct RestRateLimitResponse {
    resources: RestRateLimitResources,
}

#[derive(Debug, Deserialize)]
struct RestRateLimitResources {
    graphql: RestRateLimitResource,
}

#[derive(Debug, Deserialize)]
struct RestRateLimitResource {
    remaining: u32,
    reset: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct ApiTeam {
    slug: String,
    organization: ApiOrganization,
}

#[derive(Debug, Clone, Deserialize)]
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
struct SearchGraphQlEnvelope {
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
    #[serde(default, rename = "rateLimit")]
    rate_limit: Option<ApiRateLimit>,
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
    #[serde(default, rename = "rateLimit")]
    rate_limit: Option<ApiRateLimit>,
}

#[derive(Debug, Deserialize)]
struct SearchData {
    search: SearchConnection,
    #[serde(default, rename = "rateLimit")]
    rate_limit: Option<ApiRateLimit>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchConnection {
    issue_count: usize,
    page_info: PageInfo,
    nodes: Vec<Option<ApiPullRequestIndex>>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct ApiPullRequestIndex {
    id: String,
    updated_at: DateTime<Utc>,
    #[serde(default)]
    state: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PullRequestIndexEnvelope {
    data: Option<PullRequestIndexData>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
}

#[derive(Debug, Deserialize)]
struct PullRequestIndexData {
    nodes: Vec<Option<ApiPullRequestIndex>>,
    #[serde(default, rename = "rateLimit")]
    rate_limit: Option<ApiRateLimit>,
}

#[derive(Debug, Deserialize)]
struct PullRequestStatusEnvelope {
    data: Option<PullRequestStatusData>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
}

#[derive(Debug, Deserialize)]
struct PullRequestStatusData {
    nodes: Vec<Option<ApiPullRequestStatus>>,
    #[serde(default, rename = "rateLimit")]
    rate_limit: Option<ApiRateLimit>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiPullRequestStatus {
    id: String,
    state: String,
    updated_at: DateTime<Utc>,
    mergeable: String,
    review_decision: Option<String>,
    commits: StatusCommitConnection,
}

#[derive(Debug, Deserialize)]
struct StatusCommitConnection {
    nodes: Vec<StatusCommitNode>,
}

#[derive(Debug, Deserialize)]
struct StatusCommitNode {
    commit: StatusCommit,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StatusCommit {
    status_check_rollup: Option<ApiStatusRollup>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiRateLimit {
    remaining: u32,
    reset_at: DateTime<Utc>,
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
    id: String,
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
    use chrono::TimeZone;

    use super::*;

    fn timestamp(minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 3, 12, minute, 0).unwrap()
    }

    fn index(id: &str, minute: u32) -> ApiPullRequestIndex {
        ApiPullRequestIndex {
            id: id.into(),
            updated_at: timestamp(minute),
            state: None,
        }
    }

    fn cached_pull_request(id: &str, minute: u32, age: Duration) -> CachedPullRequest {
        CachedPullRequest {
            updated_at: timestamp(minute),
            details_refreshed_at: Instant::now() - age,
            last_seen_at: Instant::now(),
            pull_request: PullRequest {
                number: 1,
                title: id.into(),
                url: format!("https://github.com/acme/app/pull/{id}"),
                repository: "acme/app".into(),
                author: "owner".into(),
                is_draft: false,
                created_at: timestamp(0),
                updated_at: timestamp(minute),
                additions: 1,
                deletions: 1,
                changed_files: 1,
                base_ref: "main".into(),
                head_ref: "feature".into(),
                mergeable: MergeableState::Unknown,
                review_decision: None,
                reviews: vec![],
                total_review_events: 0,
                review_requests: vec![],
                total_review_requests: 0,
                comments: 0,
                labels: vec![],
                checks: None,
                requested_via: vec![],
                involvement: vec![],
            },
        }
    }

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
    fn frequent_searches_only_request_scalar_index_fields() {
        assert!(SEARCH_INDEX_QUERY.contains("id updatedAt"));
        for expensive_field in [
            "comments(",
            "reviews(",
            "reviewRequests(",
            "authors(",
            "statusCheckRollup",
        ] {
            assert!(!SEARCH_INDEX_QUERY.contains(expensive_field));
        }
        assert!(STATUS_QUERY.contains("commits(last: 1)"));
        assert!(!STATUS_QUERY.contains("reviews("));
        assert!(PULL_REQUEST_FRAGMENT.contains("reviews(last: 100)"));
    }

    #[test]
    fn every_graphql_operation_reports_its_rate_budget() {
        for query in [
            SEARCH_INDEX_QUERY,
            NODE_QUERY,
            STATUS_QUERY,
            NODE_INDEX_QUERY,
            ASSOCIATED_PULL_REQUESTS_QUERY,
        ] {
            assert!(query.contains("rateLimit { cost remaining resetAt }"));
        }
    }

    #[test]
    fn duplicate_search_hits_are_hydrated_once_but_keep_every_review_source() {
        let mut hits = vec![index("PR_1", 0), index("PR_1", 1), index("PR_2", 1)];
        let mut sources = HashMap::new();
        record_review_sources(&mut sources, &hits[..1], "@viewer".into());
        record_review_sources(&mut sources, &hits[1..2], "acme/core".into());
        deduplicate_indexes(&mut hits);

        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].updated_at, timestamp(1));
        assert_eq!(
            sources["PR_1"],
            vec!["@viewer".to_string(), "acme/core".to_string()]
        );
    }

    #[test]
    fn detail_planner_reuses_unchanged_data_and_refreshes_new_changed_or_old_data() {
        let fresh = cached_pull_request("PR_1", 0, Duration::from_secs(60));
        let old = cached_pull_request("PR_1", 0, DETAIL_RECONCILE_TTL + Duration::from_secs(1));

        assert_eq!(
            detail_need(&index("PR_1", 0), None, true),
            DetailNeed::Required
        );
        assert_eq!(
            detail_need(&index("PR_1", 1), Some(&fresh), true),
            DetailNeed::Required
        );
        assert_eq!(
            detail_need(&index("PR_1", 0), Some(&fresh), true),
            DetailNeed::Cached
        );
        assert_eq!(
            detail_need(&index("PR_1", 0), Some(&old), true),
            DetailNeed::Reconcile
        );
        assert_eq!(
            detail_need(&index("PR_1", 0), Some(&old), false),
            DetailNeed::Cached
        );
    }

    #[test]
    fn concurrent_refreshes_cannot_overwrite_newer_cached_data() {
        let existing = cached_pull_request("PR_1", 1, Duration::from_secs(60));
        let older_update = cached_pull_request("PR_1", 0, Duration::ZERO);
        let fresher_same_update = cached_pull_request("PR_1", 1, Duration::ZERO);
        let mut stale_status = existing.clone();
        stale_status.last_seen_at = existing.last_seen_at - Duration::from_secs(1);

        assert!(!should_replace_cached_entry(&existing, &older_update));
        assert!(should_replace_cached_entry(&existing, &fresher_same_update));
        assert!(!should_replace_cached_entry(&existing, &stale_status));
    }

    #[test]
    fn compact_status_updates_ci_mergeability_and_review_decision() {
        let mut pull_request = cached_pull_request("PR_1", 0, Duration::ZERO).pull_request;
        let status: ApiPullRequestStatus = serde_json::from_str(
            r#"{
              "id":"PR_1", "state":"OPEN", "updatedAt":"2026-08-03T12:00:00Z",
              "mergeable":"CONFLICTING", "reviewDecision":"CHANGES_REQUESTED",
              "commits":{"nodes":[{"commit":{"authors":{"nodes":[]},"statusCheckRollup":{"state":"FAILURE"}}}]}
            }"#,
        )
        .unwrap();

        apply_status(&mut pull_request, &status);

        assert_eq!(pull_request.mergeable, MergeableState::Conflicting);
        assert_eq!(
            pull_request.review_decision,
            Some(ReviewDecision::ChangesRequested)
        );
        assert_eq!(pull_request.checks, Some(CheckState::Failure));
    }

    #[test]
    fn historical_commit_discovery_is_not_repeated_on_every_ui_refresh() {
        assert_eq!(COMMIT_DISCOVERY_TTL, Duration::from_secs(30 * 60));
        assert!(COMMIT_DISCOVERY_TTL > Duration::from_secs(30));
        assert_eq!(COMMIT_FULL_RECONCILE_TTL, Duration::from_secs(6 * 60 * 60));
    }

    #[test]
    fn exhausted_budget_pauses_polling_only_until_githubs_reset() {
        let now = timestamp(0);
        let exhausted = RateLimitSnapshot {
            remaining: REFRESH_RATE_FLOOR - 1,
            reset_at: timestamp(1),
        };
        let replenished = RateLimitSnapshot {
            remaining: REFRESH_RATE_FLOOR,
            reset_at: timestamp(1),
        };

        assert!(rate_limit_requires_deferral(&exhausted, now));
        assert!(!rate_limit_requires_deferral(&replenished, now));
        assert!(!rate_limit_requires_deferral(&exhausted, timestamp(1)));
    }

    #[test]
    fn rest_rate_limit_response_can_seed_graphql_budget_before_the_first_query() {
        let response: RestRateLimitResponse = serde_json::from_str(
            r#"{"resources":{"graphql":{"limit":5000,"remaining":42,"reset":1785803764}}}"#,
        )
        .unwrap();

        assert_eq!(response.resources.graphql.remaining, 42);
        assert_eq!(response.resources.graphql.reset, 1_785_803_764);
    }

    #[test]
    fn paused_refresh_keeps_the_complete_dashboard_and_original_sync_time() {
        let fetched_at = timestamp(0);
        let dashboard = DashboardData {
            viewer: "viewer".into(),
            review_queue: vec![cached_pull_request("PR_1", 0, Duration::ZERO).pull_request],
            involved: vec![],
            owned: vec![],
            warnings: vec!["GitHub GraphQL budget is low (old warning)".into()],
            fetched_at,
            teams: vec!["acme/core".into()],
        };
        let paused = paused_dashboard(
            dashboard,
            &RateLimitSnapshot {
                remaining: 12,
                reset_at: timestamp(1),
            },
        );

        assert_eq!(paused.fetched_at, fetched_at);
        assert_eq!(paused.review_queue.len(), 1);
        assert_eq!(paused.teams, vec!["acme/core"]);
        assert_eq!(paused.warnings.len(), 1);
        assert!(paused.warnings[0].contains("12 points"));
        assert!(paused.warnings[0].contains("last complete dashboard remains visible"));
    }

    #[test]
    fn parses_every_review_state_and_requested_reviewer_kind() {
        let json = r#"{
          "data": {"search": {
            "issueCount": 1,
            "pageInfo": {"hasNextPage": false, "endCursor": null},
            "nodes": [{
              "id": "PR_kwDO_test_42",
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

        let response: serde_json::Value = serde_json::from_str(json).unwrap();
        let api_pr: ApiPullRequest =
            serde_json::from_value(response["data"]["search"]["nodes"][0].clone()).unwrap();
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
          "id": "PR_kwDO_test_7",
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
            participation_reasons(&pull_request, "viewer"),
            vec![
                InvolvementReason::Assigned,
                InvolvementReason::Commented,
                InvolvementReason::Reviewed,
                InvolvementReason::Committed,
            ]
        );
    }
}
