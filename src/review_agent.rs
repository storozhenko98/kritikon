use std::{
    collections::HashMap,
    env, fs,
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Output, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewEvent {
    SessionReady(ReviewSnapshot),
    Completed(ReviewSnapshot),
    Failed {
        snapshot: ReviewSnapshot,
        error: String,
    },
}

impl ReviewEvent {
    fn target_url(&self) -> &str {
        match self {
            Self::SessionReady(snapshot) | Self::Completed(snapshot) => &snapshot.target.url,
            Self::Failed { snapshot, .. } => &snapshot.target.url,
        }
    }

    fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed(_) | Self::Failed { .. })
    }
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

pub struct ReviewCoordinator {
    store: ReviewStore,
    dependencies: ReviewDependencies,
    sender: Sender<ReviewEvent>,
    receiver: Receiver<ReviewEvent>,
    active: HashMap<String, Arc<JobControl>>,
}

impl ReviewCoordinator {
    pub fn system() -> Result<Self> {
        let (sender, receiver) = mpsc::channel();
        Ok(Self {
            store: ReviewStore::system()?,
            dependencies: ReviewDependencies {
                gh: env::var_os("KRITIKON_GH_BIN")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("gh")),
                opencode: env::var_os("KRITIKON_OPENCODE_BIN")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("opencode")),
                model: env::var("KRITIKON_OPENCODE_MODEL").ok(),
            },
            sender,
            receiver,
            active: HashMap::new(),
        })
    }

    #[cfg(test)]
    fn at(store: ReviewStore, gh: PathBuf, opencode: PathBuf, model: Option<String>) -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            store,
            dependencies: ReviewDependencies {
                gh,
                opencode,
                model,
            },
            sender,
            receiver,
            active: HashMap::new(),
        }
    }

    pub fn start_review(
        &mut self,
        target: ReviewTarget,
        focus: Option<String>,
    ) -> Result<ReviewSnapshot> {
        if self.active.contains_key(&target.url) {
            bail!("an OpenCode review is already running for this pull request");
        }

        let initial = self.store.inspect(target.clone())?;
        let control = Arc::new(JobControl::default());
        self.active.insert(target.url.clone(), control.clone());

        let store = self.store.clone();
        let dependencies = self.dependencies.clone();
        let sender = self.sender.clone();
        let initial_for_error = initial.clone();
        thread::spawn(move || {
            let result = run_review_job(ReviewJob {
                store: &store,
                target: target.clone(),
                focus: focus.as_deref(),
                dependencies: &dependencies,
                control: &control,
                sender: &sender,
            });
            match result {
                Ok(snapshot) => {
                    let _ = sender.send(ReviewEvent::Completed(snapshot));
                }
                Err(error) => {
                    let snapshot = store.inspect(target).unwrap_or(initial_for_error);
                    let _ = sender.send(ReviewEvent::Failed {
                        snapshot,
                        error: format!("{error:#}"),
                    });
                }
            }
        });

        Ok(initial)
    }

    pub fn drain_events(&mut self) -> Vec<ReviewEvent> {
        let events = self.receiver.try_iter().collect::<Vec<_>>();
        for event in &events {
            if event.is_terminal() {
                self.active.remove(event.target_url());
            }
        }
        events
    }

    pub fn open_chat(&self, snapshot: &ReviewSnapshot) -> Result<ReviewSnapshot> {
        let attachment_guard = self
            .active
            .get(&snapshot.target.url)
            .cloned()
            .map(AttachmentGuard::new);
        let attachment = attachment_guard
            .as_ref()
            .and_then(|guard| guard.control.connection());
        if attachment_guard.is_some() && attachment.is_none() {
            bail!("the review workspace is still preparing; attach when the session is ready");
        }
        open_chat_with_dependencies(
            &self.store,
            snapshot,
            &self.dependencies.opencode,
            self.dependencies.model.as_deref(),
            attachment.as_ref(),
        )
    }
}

#[derive(Debug, Clone)]
struct ReviewDependencies {
    gh: PathBuf,
    opencode: PathBuf,
    model: Option<String>,
}

impl Drop for ReviewCoordinator {
    fn drop(&mut self) {
        for control in self.active.values() {
            control.shutdown.store(true, Ordering::Release);
            control.terminate_processes();
        }
    }
}

#[derive(Debug, Clone)]
struct ServerConnection {
    endpoint: String,
    password: String,
}

#[derive(Debug, Default)]
struct JobControl {
    shutdown: AtomicBool,
    attachments: AtomicUsize,
    connection: Mutex<Option<ServerConnection>>,
    processes: Mutex<Vec<u32>>,
}

impl JobControl {
    fn connection(&self) -> Option<ServerConnection> {
        self.connection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn set_connection(&self, connection: ServerConnection) {
        *self
            .connection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(connection);
    }

    fn register_process(&self, process_id: u32) {
        self.processes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(process_id);
    }

    fn unregister_process(&self, process_id: u32) {
        self.processes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|registered| *registered != process_id);
    }

    fn terminate_processes(&self) {
        let process_ids = self
            .processes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        for process_id in process_ids {
            let _ = Command::new("kill")
                .args(["-TERM", &process_id.to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

struct AttachmentGuard {
    control: Arc<JobControl>,
}

impl AttachmentGuard {
    fn new(control: Arc<JobControl>) -> Self {
        control.attachments.fetch_add(1, Ordering::AcqRel);
        Self { control }
    }
}

impl Drop for AttachmentGuard {
    fn drop(&mut self) {
        self.control.attachments.fetch_sub(1, Ordering::AcqRel);
    }
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
            log: directory.join("agent.log"),
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
    log: PathBuf,
    workspace: PathBuf,
}

pub fn inspect(target: ReviewTarget) -> Result<ReviewSnapshot> {
    ReviewStore::system()?.inspect(target)
}

struct ReviewJob<'a> {
    store: &'a ReviewStore,
    target: ReviewTarget,
    focus: Option<&'a str>,
    dependencies: &'a ReviewDependencies,
    control: &'a Arc<JobControl>,
    sender: &'a Sender<ReviewEvent>,
}

fn run_review_job(job: ReviewJob<'_>) -> Result<ReviewSnapshot> {
    let ReviewJob {
        store,
        target,
        focus,
        dependencies,
        control,
        sender,
    } = job;
    let mut snapshot = store.inspect(target.clone())?;
    if control.shutdown.load(Ordering::Acquire) {
        bail!("review stopped before workspace preparation began");
    }
    prepare_workspace(&snapshot, &dependencies.gh)?;
    restore_saved_draft(&snapshot)?;

    let capabilities = verify_opencode(&dependencies.opencode)?;
    let permission_config = opencode_permission_config()?;
    let paths = store.paths(&target);
    let server = start_server(
        &dependencies.opencode,
        &snapshot.workspace,
        &paths.log,
        &permission_config,
        control,
    )?;
    control.set_connection(server.connection.clone());

    let session_id = match snapshot.session_id.clone() {
        Some(session_id) => session_id,
        None => create_server_session(&server.connection, &snapshot.workspace, &target)?,
    };
    snapshot.session_id = Some(session_id.clone());
    store.save_record(
        &paths.record,
        &ReviewRecord {
            version: RECORD_VERSION,
            target: target.clone(),
            session_id: session_id.clone(),
            updated_at: now_epoch_seconds(),
        },
    )?;
    sender
        .send(ReviewEvent::SessionReady(snapshot.clone()))
        .context("Kritikon closed before the OpenCode session became ready")?;

    let status = run_headless_review(
        HeadlessReview {
            opencode: &dependencies.opencode,
            snapshot: &snapshot,
            connection: &server.connection,
            log_path: &paths.log,
            permission_config: &permission_config,
            capabilities: &capabilities,
            model: dependencies.model.as_deref(),
            focus: focus.unwrap_or_default(),
        },
        control,
    )?;

    wait_for_attachments(control)?;

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
                "The background OpenCode run exited with {status}; its session and any completed draft were preserved. Log: {}",
                paths.log.display()
            )
        }),
    })
}

fn wait_for_attachments(control: &JobControl) -> Result<()> {
    while control.attachments.load(Ordering::Acquire) > 0
        && !control.shutdown.load(Ordering::Acquire)
    {
        thread::sleep(Duration::from_millis(50));
    }
    if control.shutdown.load(Ordering::Acquire) {
        bail!("review stopped because Kritikon exited");
    }
    Ok(())
}

struct HeadlessReview<'a> {
    opencode: &'a Path,
    snapshot: &'a ReviewSnapshot,
    connection: &'a ServerConnection,
    log_path: &'a Path,
    permission_config: &'a str,
    capabilities: &'a OpencodeCapabilities,
    model: Option<&'a str>,
    focus: &'a str,
}

fn run_headless_review(request: HeadlessReview<'_>, control: &JobControl) -> Result<ExitStatus> {
    let HeadlessReview {
        opencode,
        snapshot,
        connection,
        log_path,
        permission_config,
        capabilities,
        model,
        focus,
    } = request;
    let log = open_private_log(log_path, true)?;
    let mut command = Command::new(opencode);
    command
        .current_dir(&snapshot.workspace)
        .arg("run")
        .args(["--attach", &connection.endpoint])
        .arg("--dir")
        .arg(&snapshot.workspace)
        .args([
            "--session",
            snapshot.session_id.as_deref().unwrap_or_default(),
        ])
        .args(["--agent", "build"])
        .args(["--format", "json"]);
    if capabilities.run_auto_flag {
        command.arg("--auto");
    }
    if let Some(model) = model.filter(|model| !model.trim().is_empty()) {
        command.args(["--model", model]);
    }
    command
        .arg(review_prompt(&snapshot.target, focus))
        .env("OPENCODE_SERVER_PASSWORD", &connection.password)
        .env("OPENCODE_CONFIG_CONTENT", permission_config)
        .stdin(Stdio::null())
        .stdout(
            log.try_clone()
                .context("could not clone OpenCode log file")?,
        )
        .stderr(log);

    let mut child = command
        .spawn()
        .with_context(|| format!("could not start background {} run", opencode.display()))?;
    let process_id = child.id();
    control.register_process(process_id);
    let result = wait_for_child(&mut child, control);
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    control.unregister_process(process_id);
    result
}

fn open_chat_with_dependencies(
    store: &ReviewStore,
    snapshot: &ReviewSnapshot,
    opencode: &Path,
    model: Option<&str>,
    connection: Option<&ServerConnection>,
) -> Result<ReviewSnapshot> {
    let session_id = snapshot
        .session_id
        .as_deref()
        .context("start a review before opening its OpenCode chat session")?;
    let capabilities = verify_opencode(opencode)?;
    let permission_config = opencode_permission_config()?;
    let mut command = Command::new(opencode);
    command.current_dir(&snapshot.workspace);
    if let Some(connection) = connection {
        command
            .arg("attach")
            .arg(&connection.endpoint)
            .arg("--dir")
            .arg(&snapshot.workspace)
            .args(["--session", session_id])
            .env("OPENCODE_SERVER_PASSWORD", &connection.password);
    } else {
        if capabilities.tui_auto_flag {
            command.arg("--auto");
        }
        command
            .arg(&snapshot.workspace)
            .args(["--session", session_id])
            .args(["--agent", "build"]);
        if let Some(model) = model.filter(|model| !model.trim().is_empty()) {
            command.args(["--model", model]);
        }
    }
    let status = command
        .env("OPENCODE_CONFIG_CONTENT", permission_config)
        .status()
        .with_context(|| format!("could not open {} chat", opencode.display()))?;

    let paths = store.paths(&snapshot.target);
    let draft = store.save_draft(&workspace_draft_path(&snapshot.workspace), &paths.draft)?;
    let mut refreshed = store.inspect(snapshot.target.clone())?;
    refreshed.draft = draft;
    if !status.success() {
        refreshed.warning = Some(if connection.is_some() {
            format!("OpenCode chat exited with {status}; the background review was not stopped")
        } else {
            format!("OpenCode chat exited with {status}; the saved session was preserved")
        });
    }
    Ok(refreshed)
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

struct ServerProcess {
    child: Child,
    connection: ServerConnection,
    control: Arc<JobControl>,
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let process_id = self.child.id();
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.control.unregister_process(process_id);
    }
}

fn start_server(
    opencode: &Path,
    workspace: &Path,
    log_path: &Path,
    permission_config: &str,
    control: &Arc<JobControl>,
) -> Result<ServerProcess> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .context("could not reserve a loopback port for OpenCode")?;
    let port = listener
        .local_addr()
        .context("could not read the reserved OpenCode port")?
        .port();
    drop(listener);

    let password = random_server_password()?;
    let connection = ServerConnection {
        endpoint: format!("http://127.0.0.1:{port}"),
        password,
    };
    let log = open_private_log(log_path, false)?;
    let child = Command::new(opencode)
        .current_dir(workspace)
        .arg("serve")
        .args(["--hostname", "127.0.0.1"])
        .args(["--port", &port.to_string()])
        .env("OPENCODE_SERVER_PASSWORD", &connection.password)
        .env("OPENCODE_CONFIG_CONTENT", permission_config)
        .stdin(Stdio::null())
        .stdout(
            log.try_clone()
                .context("could not clone OpenCode log file")?,
        )
        .stderr(log)
        .spawn()
        .with_context(|| format!("could not start {} serve", opencode.display()))?;
    control.register_process(child.id());
    let mut server = ServerProcess {
        child,
        connection,
        control: control.clone(),
    };

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if control.shutdown.load(Ordering::Acquire) {
            bail!("review stopped while the OpenCode server was starting");
        }
        if let Some(status) = server
            .child
            .try_wait()
            .context("could not inspect the OpenCode server")?
        {
            bail!(
                "OpenCode server exited with {status} before becoming ready. Log: {}",
                log_path.display()
            );
        }
        if server_is_healthy(&server.connection) {
            return Ok(server);
        }
        if Instant::now() >= deadline {
            bail!(
                "OpenCode server did not become ready within 10 seconds. Log: {}",
                log_path.display()
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn server_is_healthy(connection: &ServerConnection) -> bool {
    let url = format!("{}/global/health", connection.endpoint);
    let agent = local_http_agent(Duration::from_millis(250));
    let Ok(mut response) = agent
        .get(&url)
        .header("Authorization", &basic_auth_header(&connection.password))
        .call()
    else {
        return false;
    };
    response
        .body_mut()
        .read_json::<Value>()
        .ok()
        .and_then(|value| value.get("healthy").and_then(Value::as_bool))
        .unwrap_or(false)
}

#[derive(Debug, Deserialize)]
struct CreatedSession {
    id: String,
}

fn create_server_session(
    connection: &ServerConnection,
    workspace: &Path,
    target: &ReviewTarget,
) -> Result<String> {
    let url = format!("{}/session", connection.endpoint);
    let title = format!("Kritikon: {}#{}", target.repository, target.number);
    let agent = local_http_agent(Duration::from_secs(5));
    let mut response = agent
        .post(&url)
        .query("directory", workspace.to_string_lossy())
        .header("Authorization", &basic_auth_header(&connection.password))
        .send_json(serde_json::json!({ "title": title }))
        .context("could not create an OpenCode review session")?;
    let session: CreatedSession = response
        .body_mut()
        .read_json()
        .context("OpenCode returned an invalid session response")?;
    if session.id.trim().is_empty() {
        bail!("OpenCode created a review session without an ID");
    }
    Ok(session.id)
}

fn local_http_agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .build()
        .into()
}

fn basic_auth_header(password: &str) -> String {
    format!("Basic {}", BASE64.encode(format!("opencode:{password}")))
}

fn random_server_password() -> Result<String> {
    let mut bytes = [0_u8; 32];
    fs::File::open("/dev/urandom")
        .context("could not open the operating system random source")?
        .read_exact(&mut bytes)
        .context("could not generate an OpenCode server password")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn wait_for_child(child: &mut Child, control: &JobControl) -> Result<ExitStatus> {
    loop {
        if control.shutdown.load(Ordering::Acquire) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("review stopped because Kritikon exited");
        }
        if let Some(status) = child
            .try_wait()
            .context("could not inspect the background OpenCode review")?
        {
            return Ok(status);
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(unix)]
fn open_private_log(path: &Path, append: bool) -> Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    let parent = path.parent().context("OpenCode log path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("could not create {}", parent.display()))?;
    fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("could not open {}", path.display()))
}

#[cfg(not(unix))]
fn open_private_log(_path: &Path, _append: bool) -> Result<fs::File> {
    bail!("Kritikon supports OpenCode review sessions on macOS and Linux only")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OpencodeCapabilities {
    tui_auto_flag: bool,
    run_auto_flag: bool,
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
    let root_help = [help.stdout.as_slice(), help.stderr.as_slice()].concat();
    let tui_auto_flag = help.status.success() && contains_auto_flag(&root_help);
    let root_help = String::from_utf8_lossy(&root_help);
    if !root_help.contains("opencode serve") || !root_help.contains("opencode attach") {
        bail!(
            "this OpenCode version does not support detached reviews; upgrade to a version with `serve`, `run --attach`, and `attach`"
        );
    }

    let run_help = Command::new(opencode)
        .args(["run", "--help"])
        .output()
        .context("could not inspect OpenCode background-run capabilities")?;
    let run_help_text = [run_help.stdout.as_slice(), run_help.stderr.as_slice()].concat();
    let run_help_string = String::from_utf8_lossy(&run_help_text);
    if !run_help.status.success()
        || !run_help_string.contains("--attach")
        || !run_help_string.contains("--dir")
        || !run_help_string.contains("--session")
    {
        bail!(
            "this OpenCode version does not support detached reviews; upgrade to a version with `run --attach`, `--dir`, and `--session`"
        );
    }
    let run_auto_flag = run_help.status.success() && contains_auto_flag(&run_help_text);
    Ok(OpencodeCapabilities {
        tui_auto_flag,
        run_auto_flag,
    })
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
    use std::{
        io::{BufRead, BufReader},
        net::TcpListener,
        sync::mpsc,
    };

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

    #[test]
    fn server_session_creation_is_loopback_authenticated_and_directory_scoped() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let (request_sender, request_receiver) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                request.push_str(&line);
            }
            request_sender.send(request).unwrap();
            let body = r#"{"id":"ses_background"}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        let connection = ServerConnection {
            endpoint: format!("http://{address}"),
            password: "secret".into(),
        };
        let session =
            create_server_session(&connection, Path::new("/tmp/review workspace"), &target())
                .unwrap();
        assert_eq!(session, "ses_background");
        let request = request_receiver.recv().unwrap();
        assert!(
            request.starts_with("POST /session?directory=%2Ftmp%2Freview%20workspace HTTP/1.1")
        );
        assert!(request.to_ascii_lowercase().contains(&format!(
            "authorization: {}",
            basic_auth_header("secret").to_ascii_lowercase()
        )));
        server.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn headless_review_uses_run_and_never_takes_over_the_terminal() {
        let directory = tempdir().unwrap();
        let opencode = directory.path().join("opencode");
        let arguments = directory.path().join("run.args");
        let permissions = directory.path().join("permissions.json");
        let workspace = directory.path().join("workspace");
        let log = directory.path().join("agent.log");
        fs::create_dir_all(workspace.join(".kritikon")).unwrap();
        write_executable(
            &opencode,
            &format!(
                "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$@\" > '{}'\nprintf '%s' \"$OPENCODE_CONFIG_CONTENT\" > '{}'\nprintf '# Summary\\n\\nBackground complete.\\n' > .kritikon/review.md\n",
                arguments.display(),
                permissions.display()
            ),
        );
        let snapshot = ReviewSnapshot {
            target: target(),
            session_id: Some("ses_background".into()),
            draft: None,
            draft_path: directory.path().join("saved.md"),
            workspace: workspace.clone(),
            warning: None,
        };
        let connection = ServerConnection {
            endpoint: "http://127.0.0.1:43177".into(),
            password: "secret".into(),
        };
        let status = run_headless_review(
            HeadlessReview {
                opencode: &opencode,
                snapshot: &snapshot,
                connection: &connection,
                log_path: &log,
                permission_config: OPENCODE_PERMISSION_CONFIG,
                capabilities: &OpencodeCapabilities {
                    tui_auto_flag: true,
                    run_auto_flag: true,
                },
                model: Some("opencode/gpt-5.4"),
                focus: "Check detachment.",
            },
            &JobControl::default(),
        )
        .unwrap();
        assert!(status.success());
        let arguments = fs::read_to_string(arguments).unwrap();
        let arguments = arguments.lines().collect::<Vec<_>>();
        assert_eq!(arguments[0], "run");
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--attach", connection.endpoint.as_str()])
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--dir", workspace.to_str().unwrap()])
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--session", "ses_background"])
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--agent", "build"])
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--format", "json"])
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--model", "opencode/gpt-5.4"])
        );
        assert!(arguments.contains(&"--auto"));
        assert!(!arguments.contains(&"--prompt"));
        assert!(
            arguments
                .iter()
                .any(|argument| argument.contains("Check detachment."))
        );
        let permission: Value =
            serde_json::from_str(&fs::read_to_string(permissions).unwrap()).unwrap();
        assert_eq!(permission["permission"], "allow");
        assert_eq!(
            fs::metadata(log).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn live_chat_uses_attach_and_returns_to_the_same_saved_session() {
        let data = tempdir().unwrap();
        let workspaces = tempdir().unwrap();
        let binaries = tempdir().unwrap();
        let store = ReviewStore::at(data.path(), workspaces.path());
        let target = target();
        let paths = store.paths(&target);
        fs::create_dir_all(paths.workspace.join(".kritikon")).unwrap();
        fs::write(
            workspace_draft_path(&paths.workspace),
            "# Summary\n\nUpdated while attached.\n",
        )
        .unwrap();
        store
            .save_record(
                &paths.record,
                &ReviewRecord {
                    version: RECORD_VERSION,
                    target: target.clone(),
                    session_id: "ses_live".into(),
                    updated_at: 1,
                },
            )
            .unwrap();

        let opencode = binaries.path().join("opencode");
        let arguments = binaries.path().join("attach.args");
        let password = binaries.path().join("attach.password");
        write_executable(
            &opencode,
            &format!(
                r#"#!/bin/sh
set -eu
if [ "$1" = "--version" ]; then echo 1.18.13; exit 0; fi
if [ "$1" = "--help" ]; then
  printf 'opencode serve\nopencode attach\n      --auto\n'
  exit 0
fi
if [ "$1 $2" = "run --help" ]; then
  printf '%s\n' '--attach --dir --session --auto'
  exit 0
fi
printf '%s\n' "$@" > '{}'
printf '%s' "$OPENCODE_SERVER_PASSWORD" > '{}'
"#,
                arguments.display(),
                password.display()
            ),
        );
        let snapshot = store.inspect(target).unwrap();
        let connection = ServerConnection {
            endpoint: "http://127.0.0.1:43177".into(),
            password: "secret".into(),
        };
        let refreshed = open_chat_with_dependencies(
            &store,
            &snapshot,
            &opencode,
            Some("opencode/gpt-5.4"),
            Some(&connection),
        )
        .unwrap();

        assert_eq!(refreshed.session_id.as_deref(), Some("ses_live"));
        assert_eq!(
            refreshed.draft.as_deref(),
            Some("# Summary\n\nUpdated while attached.\n")
        );
        let arguments = fs::read_to_string(arguments).unwrap();
        assert_eq!(
            arguments.lines().collect::<Vec<_>>(),
            vec![
                "attach",
                connection.endpoint.as_str(),
                "--dir",
                paths.workspace.to_str().unwrap(),
                "--session",
                "ses_live",
            ]
        );
        assert_eq!(fs::read_to_string(password).unwrap(), "secret");
    }

    #[test]
    fn worker_keeps_its_server_until_the_attached_tui_detaches() {
        let control = Arc::new(JobControl::default());
        let attachment = AttachmentGuard::new(control.clone());
        let (sender, receiver) = mpsc::channel();
        let waiter = thread::spawn(move || {
            wait_for_attachments(&control).unwrap();
            sender.send(()).unwrap();
        });

        assert!(receiver.recv_timeout(Duration::from_millis(150)).is_err());
        drop(attachment);
        receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        waiter.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn starting_a_review_returns_before_workspace_preparation_finishes() {
        let data = tempdir().unwrap();
        let workspaces = tempdir().unwrap();
        let binaries = tempdir().unwrap();
        let gh = binaries.path().join("gh");
        write_executable(
            &gh,
            "#!/bin/sh\nsleep 1\necho delayed failure >&2\nexit 7\n",
        );
        let mut coordinator = ReviewCoordinator::at(
            ReviewStore::at(data.path(), workspaces.path()),
            gh,
            binaries.path().join("opencode"),
            None,
        );

        let started = Instant::now();
        let snapshot = coordinator.start_review(target(), None).unwrap();
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(!snapshot.has_session());

        let deadline = Instant::now() + Duration::from_secs(3);
        let event = loop {
            if let Some(event) = coordinator.drain_events().into_iter().next() {
                break event;
            }
            assert!(
                Instant::now() < deadline,
                "background failure was not reported"
            );
            thread::sleep(Duration::from_millis(20));
        };
        assert!(matches!(event, ReviewEvent::Failed { .. }));
    }

    #[test]
    fn coordinator_shutdown_terminates_registered_background_processes() {
        let data = tempdir().unwrap();
        let workspaces = tempdir().unwrap();
        let mut coordinator = ReviewCoordinator::at(
            ReviewStore::at(data.path(), workspaces.path()),
            PathBuf::from("gh"),
            PathBuf::from("opencode"),
            None,
        );
        let control = Arc::new(JobControl::default());
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        control.register_process(child.id());
        coordinator.active.insert(target().url, control);

        drop(coordinator);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            assert!(Instant::now() < deadline, "background child was orphaned");
            thread::sleep(Duration::from_millis(20));
        }
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
