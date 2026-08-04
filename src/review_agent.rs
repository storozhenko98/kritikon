use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::model::PullRequest;

const RECORD_VERSION: u8 = 1;
const OPENCODE_PERMISSION_CONFIG: &str = r#"{"permission":"allow"}"#;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewTarget {
    pub url: String,
    pub repository: String,
    pub number: u64,
    pub title: String,
    pub base_ref: String,
    pub head_ref: String,
}

impl From<&PullRequest> for ReviewTarget {
    fn from(pull_request: &PullRequest) -> Self {
        Self {
            url: pull_request.url.clone(),
            repository: pull_request.repository.clone(),
            number: pull_request.number,
            title: pull_request.title.clone(),
            base_ref: pull_request.base_ref.clone(),
            head_ref: pull_request.head_ref.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchMode {
    Review,
    Chat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewKind {
    Approve,
    Comment,
    RequestChanges,
}

impl ReviewKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Approve => "APPROVE",
            Self::Comment => "COMMENT",
            Self::RequestChanges => "REQUEST CHANGES",
        }
    }

    fn gh_flag(self) -> &'static str {
        match self {
            Self::Approve => "--approve",
            Self::Comment => "--comment",
            Self::RequestChanges => "--request-changes",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewSnapshot {
    pub target: ReviewTarget,
    pub session_id: Option<String>,
    pub draft: Option<String>,
    pub draft_path: PathBuf,
    pub workspace: PathBuf,
    pub warning: Option<String>,
}

impl ReviewSnapshot {
    pub fn has_session(&self) -> bool {
        self.session_id.is_some()
    }

    pub fn has_draft(&self) -> bool {
        self.draft
            .as_ref()
            .is_some_and(|draft| !draft.trim().is_empty())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewRecord {
    version: u8,
    target: ReviewTarget,
    session_id: String,
    updated_at: u64,
}

#[derive(Debug, Clone)]
pub struct ReviewStore {
    root: PathBuf,
    workspace_root: PathBuf,
}

impl ReviewStore {
    pub fn system() -> Result<Self> {
        Ok(Self {
            root: default_data_root()?,
            workspace_root: env::temp_dir().join("kritikon").join("review-workspaces"),
        })
    }

    #[cfg(test)]
    pub fn at(root: impl Into<PathBuf>, workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            workspace_root: workspace_root.into(),
        }
    }

    pub fn inspect(&self, target: ReviewTarget) -> Result<ReviewSnapshot> {
        let paths = self.paths(&target);
        let record = self.load_record(&paths.record)?;
        let session_id = record
            .filter(|record| record.target.url == target.url)
            .map(|record| record.session_id);
        let draft = read_optional_nonempty(&paths.draft)?;
        Ok(ReviewSnapshot {
            target,
            session_id,
            draft,
            draft_path: paths.draft,
            workspace: paths.workspace,
            warning: None,
        })
    }

    fn paths(&self, target: &ReviewTarget) -> ReviewPaths {
        let slug = target_slug(target);
        let directory = self.root.join(&slug);
        ReviewPaths {
            record: directory.join("session.json"),
            draft: directory.join("review.md"),
            workspace: self.workspace_root.join(slug),
        }
    }

    fn load_record(&self, path: &Path) -> Result<Option<ReviewRecord>> {
        let source = match fs::read_to_string(path) {
            Ok(source) => source,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("could not read {}", path.display()));
            }
        };
        let record: ReviewRecord = serde_json::from_str(&source)
            .with_context(|| format!("invalid review session record at {}", path.display()))?;
        if record.version != RECORD_VERSION {
            bail!(
                "unsupported review session version {} in {}",
                record.version,
                path.display()
            );
        }
        Ok(Some(record))
    }

    fn save_record(&self, path: &Path, record: &ReviewRecord) -> Result<()> {
        let contents = serde_json::to_vec_pretty(record)
            .context("could not serialize OpenCode review session")?;
        atomic_write_private(path, &contents)
    }

    fn save_draft(&self, source: &Path, destination: &Path) -> Result<Option<String>> {
        let draft = match read_optional_nonempty(source)? {
            Some(draft) => draft,
            None => return read_optional_nonempty(destination),
        };
        atomic_write_private(destination, draft.as_bytes())?;
        Ok(Some(draft))
    }
}

struct ReviewPaths {
    record: PathBuf,
    draft: PathBuf,
    workspace: PathBuf,
}

pub fn inspect(target: ReviewTarget) -> Result<ReviewSnapshot> {
    ReviewStore::system()?.inspect(target)
}

pub fn launch(
    target: ReviewTarget,
    mode: LaunchMode,
    focus: Option<&str>,
) -> Result<ReviewSnapshot> {
    let opencode = env::var_os("KRITIKON_OPENCODE_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("opencode"));
    let gh = env::var_os("KRITIKON_GH_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("gh"));
    let model = env::var("KRITIKON_OPENCODE_MODEL").ok();
    launch_with_dependencies(
        &ReviewStore::system()?,
        target,
        mode,
        focus,
        &gh,
        &opencode,
        model.as_deref(),
    )
}

fn launch_with_dependencies(
    store: &ReviewStore,
    target: ReviewTarget,
    mode: LaunchMode,
    focus: Option<&str>,
    gh: &Path,
    opencode: &Path,
    model: Option<&str>,
) -> Result<ReviewSnapshot> {
    let mut snapshot = store.inspect(target.clone())?;
    if mode == LaunchMode::Chat && snapshot.session_id.is_none() {
        bail!("start a review before opening its OpenCode chat session");
    }

    println!("Preparing isolated review workspace for {}…", target.url);
    prepare_workspace(&snapshot, gh)?;
    restore_saved_draft(&snapshot)?;

    let capabilities = verify_opencode(opencode)?;
    let permission_config = opencode_permission_config()?;
    if snapshot.session_id.is_none() {
        snapshot.session_id = newest_workspace_session(opencode, &snapshot.workspace)?;
    }

    let mut command = Command::new(opencode);
    command.current_dir(&snapshot.workspace);
    if capabilities.auto_flag {
        command.arg("--auto");
    }
    command.arg("--agent").arg("build");
    if let Some(session_id) = &snapshot.session_id {
        command.arg("--session").arg(session_id);
    }
    if let Some(model) = model.filter(|model| !model.trim().is_empty()) {
        command.arg("--model").arg(model);
    }
    if mode == LaunchMode::Review {
        command
            .arg("--prompt")
            .arg(review_prompt(&target, focus.unwrap_or_default()));
    }
    command.env("OPENCODE_CONFIG_CONTENT", permission_config);

    println!(
        "Opening OpenCode with permission auto-approval in {}…",
        snapshot.workspace.display()
    );
    let status = command
        .status()
        .with_context(|| format!("could not launch {}", opencode.display()))?;

    let session_id = match snapshot.session_id.take() {
        Some(session_id) => session_id,
        None => newest_workspace_session(opencode, &snapshot.workspace)?
            .context("OpenCode exited before Kritikon could identify a resumable session")?,
    };
    let paths = store.paths(&target);
    let workspace_draft = workspace_draft_path(&snapshot.workspace);
    let draft = store.save_draft(&workspace_draft, &paths.draft)?;
    store.save_record(
        &paths.record,
        &ReviewRecord {
            version: RECORD_VERSION,
            target: target.clone(),
            session_id: session_id.clone(),
            updated_at: now_epoch_seconds(),
        },
    )?;

    Ok(ReviewSnapshot {
        target,
        session_id: Some(session_id),
        draft,
        draft_path: paths.draft,
        workspace: paths.workspace,
        warning: (!status.success()).then(|| {
            format!(
                "OpenCode exited with {}; the session and any completed draft were preserved",
                status
            )
        }),
    })
}

pub fn post_review(snapshot: &ReviewSnapshot, kind: ReviewKind) -> Result<()> {
    let gh = env::var_os("KRITIKON_GH_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("gh"));
    post_review_with_binary(snapshot, kind, &gh)
}

fn post_review_with_binary(snapshot: &ReviewSnapshot, kind: ReviewKind, gh: &Path) -> Result<()> {
    let body = fs::read_to_string(&snapshot.draft_path).with_context(|| {
        format!(
            "could not read review draft at {}",
            snapshot.draft_path.display()
        )
    })?;
    if body.trim().is_empty() {
        bail!("the review draft is empty");
    }
    let output = Command::new(gh)
        .args(["pr", "review"])
        .arg(&snapshot.target.url)
        .arg(kind.gh_flag())
        .args(["--body-file"])
        .arg(&snapshot.draft_path)
        .output()
        .with_context(|| format!("could not run {} pr review", gh.display()))?;
    checked_output(output, "GitHub rejected the review submission").map(|_| ())
}

fn prepare_workspace(snapshot: &ReviewSnapshot, gh: &Path) -> Result<()> {
    let workspace = &snapshot.workspace;
    let parent = workspace
        .parent()
        .context("review workspace has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("could not create {}", parent.display()))?;

    if !workspace.join(".git").exists() {
        let output = Command::new(gh)
            .args(["repo", "clone"])
            .arg(&snapshot.target.repository)
            .arg(workspace)
            .args(["--", "--filter=blob:none"])
            .output()
            .with_context(|| format!("could not clone {}", snapshot.target.repository))?;
        checked_output(output, "could not create the review checkout")?;
    } else {
        run_git(workspace, &["reset", "--hard", "HEAD"])?;
        run_git(workspace, &["clean", "-fd", "-e", ".kritikon/"])?;
    }

    let output = Command::new(gh)
        .current_dir(workspace)
        .args(["pr", "checkout"])
        .arg(&snapshot.target.url)
        .args(["--detach", "--force"])
        .output()
        .with_context(|| format!("could not check out {}", snapshot.target.url))?;
    checked_output(output, "could not update the review checkout")?;

    fs::create_dir_all(workspace.join(".kritikon")).with_context(|| {
        format!(
            "could not create review output directory in {}",
            workspace.display()
        )
    })?;
    Ok(())
}

fn restore_saved_draft(snapshot: &ReviewSnapshot) -> Result<()> {
    let Some(draft) = &snapshot.draft else {
        return Ok(());
    };
    let path = workspace_draft_path(&snapshot.workspace);
    atomic_write_private(&path, draft.as_bytes())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OpencodeCapabilities {
    auto_flag: bool,
}

fn verify_opencode(opencode: &Path) -> Result<OpencodeCapabilities> {
    let output = Command::new(opencode)
        .arg("--version")
        .output()
        .with_context(|| {
            format!(
                "OpenCode is required for agent reviews but `{}` was not found",
                opencode.display()
            )
        })?;
    checked_output(output, "OpenCode is installed but could not start")?;

    let help = Command::new(opencode)
        .arg("--help")
        .output()
        .context("could not inspect OpenCode CLI capabilities")?;
    let auto_flag = help.status.success()
        && (contains_auto_flag(&help.stdout) || contains_auto_flag(&help.stderr));
    Ok(OpencodeCapabilities { auto_flag })
}

fn contains_auto_flag(output: &[u8]) -> bool {
    String::from_utf8_lossy(output)
        .lines()
        .any(|line| line.split_whitespace().any(|word| word == "--auto"))
}

fn opencode_permission_config() -> Result<String> {
    let existing = env::var_os("OPENCODE_CONFIG_CONTENT")
        .map(|value| {
            value
                .into_string()
                .map_err(|_| anyhow::anyhow!("OPENCODE_CONFIG_CONTENT is not valid UTF-8"))
        })
        .transpose()?;
    merge_opencode_permission_config(existing.as_deref())
}

fn merge_opencode_permission_config(existing: Option<&str>) -> Result<String> {
    let Some(existing) = existing else {
        return Ok(OPENCODE_PERMISSION_CONFIG.into());
    };
    let mut config: Map<String, Value> =
        serde_json::from_str(existing).context("OPENCODE_CONFIG_CONTENT must be a JSON object")?;
    config.insert("permission".into(), Value::String("allow".into()));
    serde_json::to_string(&config).context("could not enable OpenCode permission auto-approval")
}

fn newest_workspace_session(opencode: &Path, workspace: &Path) -> Result<Option<String>> {
    match newest_workspace_session_from_cli(opencode, workspace) {
        Ok(session_id) => Ok(session_id),
        Err(cli_error) => {
            let sqlite = env::var_os("KRITIKON_SQLITE_BIN")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("sqlite3"));
            let database = opencode_database_path()?;
            newest_workspace_session_from_database(&sqlite, &database, workspace).with_context(
                || {
                    format!(
                        "OpenCode CLI session discovery failed ({cli_error:#}); the read-only database fallback also failed"
                    )
                },
            )
        }
    }
}

fn newest_workspace_session_from_cli(opencode: &Path, workspace: &Path) -> Result<Option<String>> {
    let output = Command::new(opencode)
        .current_dir(workspace)
        .args(["session", "list", "--format", "json", "--max-count", "50"])
        .output()
        .context("could not list OpenCode sessions")?;
    let stdout = checked_output(output, "could not list OpenCode sessions")?;
    let sessions: Vec<SessionListItem> =
        serde_json::from_slice(&stdout).context("OpenCode returned an invalid session list")?;
    let expected = canonicalish(workspace);
    Ok(sessions
        .into_iter()
        .filter(|session| {
            session.parent_id.is_none() && canonicalish(Path::new(&session.directory)) == expected
        })
        .max_by_key(|session| session.updated)
        .map(|session| session.id))
}

fn newest_workspace_session_from_database(
    sqlite: &Path,
    database: &Path,
    workspace: &Path,
) -> Result<Option<String>> {
    let output = Command::new(sqlite)
        .args(["-readonly", "-separator", "\t"])
        .arg(database)
        .arg("SELECT id, time_updated, directory, COALESCE(parent_id, '') FROM session ORDER BY time_updated DESC LIMIT 100;")
        .output()
        .with_context(|| format!("could not run {} in read-only mode", sqlite.display()))?;
    let stdout = checked_output(output, "could not read OpenCode's session database")?;
    let expected = canonicalish(workspace);
    let mut newest: Option<(u64, String)> = None;
    for line in String::from_utf8_lossy(&stdout).lines() {
        let mut fields = line.splitn(4, '\t');
        let (Some(id), Some(updated), Some(directory), Some(parent_id)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if !parent_id.is_empty() || canonicalish(Path::new(directory)) != expected {
            continue;
        }
        let updated = updated.parse::<u64>().with_context(|| {
            format!("invalid OpenCode session timestamp returned by sqlite: {updated}")
        })?;
        if newest
            .as_ref()
            .is_none_or(|(newest_updated, _)| updated > *newest_updated)
        {
            newest = Some((updated, id.into()));
        }
    }
    Ok(newest.map(|(_, id)| id))
}

fn opencode_database_path() -> Result<PathBuf> {
    if let Some(path) = env::var_os("KRITIKON_OPENCODE_DB") {
        return Ok(PathBuf::from(path));
    }
    let base = match env::var_os("XDG_DATA_HOME") {
        Some(path) if Path::new(&path).is_absolute() => PathBuf::from(path),
        _ => env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is not set; cannot locate OpenCode's session database")?
            .join(".local")
            .join("share"),
    };
    Ok(base.join("opencode").join("opencode.db"))
}

#[derive(Debug, Deserialize)]
struct SessionListItem {
    id: String,
    updated: u64,
    directory: String,
    #[serde(default, rename = "parentID", alias = "parent_id", alias = "parentId")]
    parent_id: Option<String>,
}

fn canonicalish(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn run_git(workspace: &Path, arguments: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(arguments)
        .output()
        .with_context(|| format!("could not run git {}", arguments.join(" ")))?;
    checked_output(output, &format!("git {} failed", arguments.join(" "))).map(|_| ())
}

fn checked_output(output: Output, context: &str) -> Result<Vec<u8>> {
    if output.status.success() {
        return Ok(output.stdout);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let details = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    bail!("{context}: {details}")
}

fn workspace_draft_path(workspace: &Path) -> PathBuf {
    workspace.join(".kritikon").join("review.md")
}

fn review_prompt(target: &ReviewTarget, focus: &str) -> String {
    let focus = if focus.trim().is_empty() {
        "Apply a thorough, evidence-based review. Prioritize correctness, regressions, security, data loss, concurrency, and missing tests.".into()
    } else {
        format!("Additional reviewer focus:\n{}", focus.trim())
    };
    format!(
        r#"Review GitHub pull request {url}: {title}

You are in a managed detached checkout of the PR head ({head}) for {repository}. Compare it to origin/{base}. Read repository instructions and inspect the actual diff, relevant surrounding code, and tests. Run focused checks when useful.

{focus}

This is a review-only workspace. Do not edit product files, commit, push, submit a GitHub review, or call `gh pr review`. You may use read-only tools and test commands. Kritikon will handle submission only after the user previews and confirms the draft.

Write the complete proposed GitHub review body to `.kritikon/review.md`, replacing any prior draft. Use concise Markdown with:

# Summary
A short assessment.

# Findings
For every actionable issue, include severity (`BLOCKING`, `IMPORTANT`, or `SUGGESTION`), an exact file path and line, the concrete failure mode, and a practical fix. Do not invent findings. If there are none, say so explicitly.

# Validation
What you inspected or ran and any limitations.

# Recommended decision
Exactly one of `APPROVE`, `COMMENT`, or `REQUEST_CHANGES`, followed by one sentence explaining why.

After writing the file, summarize what is ready for the user to preview. Continue to answer follow-up questions in this same session."#,
        url = target.url,
        title = target.title,
        head = target.head_ref,
        repository = target.repository,
        base = target.base_ref,
    )
}

fn target_slug(target: &ReviewTarget) -> String {
    let host = target
        .url
        .split_once("://")
        .map(|(_, remainder)| remainder)
        .unwrap_or(&target.url)
        .split('/')
        .next()
        .unwrap_or("github");
    let raw = format!("{host}-{}-pr-{}", target.repository, target.number);
    let mut slug = String::with_capacity(raw.len());
    for character in raw.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_matches('-').to_string()
}

fn read_optional_nonempty(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(contents) if contents.trim().is_empty() => Ok(None),
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("could not read {}", path.display())),
    }
}

fn now_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn default_data_root() -> Result<PathBuf> {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set; cannot determine review session directory")?;

    #[cfg(target_os = "macos")]
    {
        Ok(data_root_for(
            Platform::MacOs,
            &home,
            env::var_os("XDG_DATA_HOME").as_deref().map(Path::new),
        ))
    }

    #[cfg(target_os = "linux")]
    {
        Ok(data_root_for(
            Platform::Linux,
            &home,
            env::var_os("XDG_DATA_HOME").as_deref().map(Path::new),
        ))
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = home;
        bail!("Kritikon supports OpenCode review sessions on macOS and Linux only")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
enum Platform {
    MacOs,
    Linux,
}

fn data_root_for(platform: Platform, home: &Path, xdg: Option<&Path>) -> PathBuf {
    let base = match platform {
        Platform::MacOs => home.join("Library").join("Application Support"),
        Platform::Linux => xdg
            .filter(|path| path.is_absolute())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| home.join(".local").join("share")),
    };
    base.join("kritikon").join("review-sessions")
}

#[cfg(unix)]
fn atomic_write_private(path: &Path, contents: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    let parent = path.parent().context("review data path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("could not create {}", parent.display()))?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)
            .with_context(|| format!("could not write {}", temporary.display()))?;
        file.write_all(contents)
            .with_context(|| format!("could not write {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("could not sync {}", temporary.display()))?;
        fs::rename(&temporary, path)
            .with_context(|| format!("could not atomically replace {}", path.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

#[cfg(not(unix))]
fn atomic_write_private(_path: &Path, _contents: &[u8]) -> Result<()> {
    bail!("Kritikon supports OpenCode review sessions on macOS and Linux only")
}

#[cfg(debug_assertions)]
pub fn development_snapshot(target: ReviewTarget) -> ReviewSnapshot {
    let slug = target_slug(&target);
    ReviewSnapshot {
        target,
        session_id: Some(format!("dev-session-{slug}")),
        draft: Some(
            "# Summary\n\nThe change is focused and the main path looks sound.\n\n# Findings\n\n- **IMPORTANT** `src/example.rs:42` — Demonstration finding for the review-agent development scenario.\n\n# Validation\n\nReviewed the diff and focused tests.\n\n# Recommended decision\n\nCOMMENT — The draft is ready for human review.\n"
                .into(),
        ),
        draft_path: PathBuf::from(format!("/tmp/kritikon-dev/{slug}/review.md")),
        workspace: PathBuf::from(format!("/tmp/kritikon-dev/{slug}/workspace")),
        warning: None,
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use tempfile::tempdir;

    use super::*;

    fn target() -> ReviewTarget {
        ReviewTarget {
            url: "https://github.com/Acme/Widgets/pull/42".into(),
            repository: "Acme/Widgets".into(),
            number: 42,
            title: "Keep session context".into(),
            base_ref: "main".into(),
            head_ref: "agent-review".into(),
        }
    }

    #[cfg(unix)]
    fn write_executable(path: &Path, contents: &str) {
        fs::write(path, contents).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn uses_native_data_directories_and_deterministic_workspace_slugs() {
        assert_eq!(
            data_root_for(Platform::MacOs, Path::new("/Users/alice"), None),
            Path::new("/Users/alice/Library/Application Support/kritikon/review-sessions")
        );
        assert_eq!(
            data_root_for(Platform::Linux, Path::new("/home/alice"), None),
            Path::new("/home/alice/.local/share/kritikon/review-sessions")
        );
        assert_eq!(
            data_root_for(
                Platform::Linux,
                Path::new("/home/alice"),
                Some(Path::new("/data")),
            ),
            Path::new("/data/kritikon/review-sessions")
        );
        assert_eq!(target_slug(&target()), "github-com-acme-widgets-pr-42");
    }

    #[test]
    fn prompt_requires_a_saved_draft_and_forbids_direct_submission() {
        let prompt = review_prompt(&target(), "Focus on race conditions.");
        assert!(prompt.contains(".kritikon/review.md"));
        assert!(prompt.contains("Focus on race conditions."));
        assert!(prompt.contains("Do not edit product files"));
        assert!(prompt.contains("Do not") && prompt.contains("gh pr review"));
        assert!(prompt.contains("APPROVE"));
        assert!(prompt.contains("COMMENT"));
        assert!(prompt.contains("REQUEST_CHANGES"));
    }

    #[test]
    fn store_round_trips_session_and_draft_per_pull_request() {
        let data = tempdir().unwrap();
        let workspaces = tempdir().unwrap();
        let store = ReviewStore::at(data.path(), workspaces.path());
        let target = target();
        let paths = store.paths(&target);
        atomic_write_private(&paths.draft, b"# Review\nLooks good.\n").unwrap();
        store
            .save_record(
                &paths.record,
                &ReviewRecord {
                    version: RECORD_VERSION,
                    target: target.clone(),
                    session_id: "ses_test".into(),
                    updated_at: 1,
                },
            )
            .unwrap();

        let snapshot = store.inspect(target).unwrap();
        assert_eq!(snapshot.session_id.as_deref(), Some("ses_test"));
        assert_eq!(snapshot.draft.as_deref(), Some("# Review\nLooks good.\n"));
        assert_eq!(
            snapshot.workspace,
            workspaces.path().join("github-com-acme-widgets-pr-42")
        );
    }

    #[test]
    fn invalid_or_cross_pr_records_are_not_resumed() {
        let data = tempdir().unwrap();
        let workspaces = tempdir().unwrap();
        let store = ReviewStore::at(data.path(), workspaces.path());
        let target = target();
        let paths = store.paths(&target);
        let mut other = target.clone();
        other.url = "https://github.com/acme/widgets/pull/99".into();
        store
            .save_record(
                &paths.record,
                &ReviewRecord {
                    version: RECORD_VERSION,
                    target: other,
                    session_id: "ses_wrong".into(),
                    updated_at: 1,
                },
            )
            .unwrap();

        assert!(!store.inspect(target).unwrap().has_session());
    }

    #[test]
    fn review_kinds_map_to_explicit_github_actions() {
        assert_eq!(ReviewKind::Approve.gh_flag(), "--approve");
        assert_eq!(ReviewKind::Comment.gh_flag(), "--comment");
        assert_eq!(ReviewKind::RequestChanges.gh_flag(), "--request-changes");
    }

    #[cfg(unix)]
    #[test]
    fn full_launch_chain_persists_draft_and_resumes_the_same_session() {
        let data = tempdir().unwrap();
        let workspaces = tempdir().unwrap();
        let binaries = tempdir().unwrap();
        let store = ReviewStore::at(data.path(), workspaces.path());
        let gh = binaries.path().join("gh");
        let opencode = binaries.path().join("opencode");
        let opencode_log = binaries.path().join("opencode.args");
        let permission_log = binaries.path().join("opencode.permission.json");

        write_executable(
            &gh,
            r#"#!/bin/sh
set -eu
if [ "$1 $2" = "repo clone" ]; then
  workspace="$4"
  git init -q "$workspace"
  git -C "$workspace" config user.email test@example.com
  git -C "$workspace" config user.name Kritikon-Test
  git -C "$workspace" commit -q --allow-empty -m initial
  exit 0
fi
if [ "$1 $2" = "pr checkout" ]; then
  exit 0
fi
exit 2
"#,
        );
        write_executable(
            &opencode,
            &format!(
                r#"#!/bin/sh
set -eu
if [ "$1" = "--version" ]; then
  echo 1.2.15
  exit 0
fi
if [ "$1" = "--help" ]; then
  echo '      --auto  Auto-approve permissions'
  exit 0
fi
if [ "$1" = "session" ]; then
  printf '[{{"id":"ses_child","updated":3,"directory":"%s","parentID":"ses_mock"}},{{"id":"ses_mock","updated":2,"directory":"%s"}}]\n' "$(pwd)" "$(pwd)"
  exit 0
fi
printf '%s\n' "$@" > '{}'
printf '%s' "$OPENCODE_CONFIG_CONTENT" > '{}'
mkdir -p .kritikon
printf '# Summary\n\nMock review complete.\n' > .kritikon/review.md
"#,
                opencode_log.display(),
                permission_log.display()
            ),
        );

        let first = launch_with_dependencies(
            &store,
            target(),
            LaunchMode::Review,
            Some("Check cancellation safety."),
            &gh,
            &opencode,
            Some("opencode/gpt-5.4"),
        )
        .unwrap();
        assert_eq!(first.session_id.as_deref(), Some("ses_mock"));
        assert_eq!(
            first.draft.as_deref(),
            Some("# Summary\n\nMock review complete.\n")
        );
        let first_arguments = fs::read_to_string(&opencode_log).unwrap();
        assert!(first_arguments.contains("--auto\n"));
        assert!(first_arguments.contains("--agent\nbuild\n"));
        assert!(first_arguments.contains("--prompt\n"));
        assert!(first_arguments.contains("--model\nopencode/gpt-5.4\n"));
        assert!(first_arguments.contains("--session\nses_mock\n"));
        assert!(first_arguments.contains("Check cancellation safety."));
        let permission: Value =
            serde_json::from_str(&fs::read_to_string(&permission_log).unwrap()).unwrap();
        assert_eq!(permission["permission"], "allow");

        let second = launch_with_dependencies(
            &store,
            target(),
            LaunchMode::Chat,
            None,
            &gh,
            &opencode,
            Some("opencode/gpt-5.4"),
        )
        .unwrap();
        assert_eq!(second.session_id, first.session_id);
        let second_arguments = fs::read_to_string(&opencode_log).unwrap();
        assert!(second_arguments.contains("--session\nses_mock\n"));
        assert!(!second_arguments.contains("--prompt\n"));
    }

    #[test]
    fn detects_only_an_explicit_auto_option() {
        assert!(contains_auto_flag(
            b"Options:\n      --auto  Auto-approve permissions\n"
        ));
        assert!(!contains_auto_flag(
            b"Description: automatic permission handling\n"
        ));
    }

    #[test]
    fn default_permission_config_enables_auto_approval() {
        let config: Value = serde_json::from_str(OPENCODE_PERMISSION_CONFIG).unwrap();
        assert_eq!(config["permission"], "allow");

        let merged: Value = serde_json::from_str(
            &merge_opencode_permission_config(Some(
                r#"{"permission":"deny","model":"opencode/gpt-5.4"}"#,
            ))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(merged["permission"], "allow");
        assert_eq!(merged["model"], "opencode/gpt-5.4");
    }

    #[cfg(unix)]
    #[test]
    fn session_discovery_has_a_read_only_database_fallback() {
        let directory = tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let sqlite = directory.path().join("sqlite3");
        let arguments = directory.path().join("sqlite.args");
        write_executable(
            &sqlite,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf 'ses_other\\t22\\t/tmp/other\\t\\nses_child\\t21\\t{}\\tses_expected\\nses_expected\\t19\\t{}\\t\\n'\n",
                arguments.display(),
                workspace.display(),
                workspace.display()
            ),
        );

        let session = newest_workspace_session_from_database(
            &sqlite,
            &directory.path().join("opencode.db"),
            &workspace,
        )
        .unwrap();
        assert_eq!(session.as_deref(), Some("ses_expected"));
        let arguments = fs::read_to_string(arguments).unwrap();
        assert!(arguments.lines().any(|argument| argument == "-readonly"));
        assert!(
            arguments
                .lines()
                .any(|argument| argument.contains("COALESCE(parent_id"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn posting_uses_the_confirmed_action_url_and_saved_body_file() {
        let directory = tempdir().unwrap();
        let gh = directory.path().join("gh");
        let arguments = directory.path().join("gh.args");
        let draft = directory.path().join("review.md");
        fs::write(&draft, "# Summary\n\nPlease address the race.\n").unwrap();
        write_executable(
            &gh,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n",
                arguments.display()
            ),
        );
        let mut snapshot = ReviewStore::at(directory.path(), directory.path())
            .inspect(target())
            .unwrap();
        snapshot.draft_path = draft;

        post_review_with_binary(&snapshot, ReviewKind::RequestChanges, &gh).unwrap();
        let arguments = fs::read_to_string(arguments).unwrap();
        assert_eq!(
            arguments.lines().collect::<Vec<_>>(),
            vec![
                "pr",
                "review",
                "https://github.com/Acme/Widgets/pull/42",
                "--request-changes",
                "--body-file",
                snapshot.draft_path.to_str().unwrap(),
            ]
        );
    }
}
