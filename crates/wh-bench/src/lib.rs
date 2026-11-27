//! Benchmarks: run a fixed suite of tasks against a factory variant and
//! measure it objectively before a change is rolled out.
//!
//! - A **suite** ([`suite`]) lists cases: a task (title/body), where the agents
//!   work (the factory repo, another repo, or a fixture directory copied into a
//!   fresh git repo per case run) and an optional **check** command run on the
//!   finished task branch (exit 0 = pass).
//! - A **run** (`bench-<id>` stream) evaluates one variant: the suite's factory
//!   file at a git ref (e.g. a proposal's commit and its parent). Every case
//!   becomes a real task flagged with [`BenchmarkContext`], driven inline by a
//!   benchmark-scoped [`Foreman`] (gates auto-approved). Benchmark tasks and
//!   sessions are excluded from production metrics, scorers and the improver.
//! - Runs are requested by `BenchmarkRequested` and claimed with
//!   `BenchmarkStarted` at `Exact(1)` (at most once). `wh bench run` appends
//!   both at once and runs inline, so a daemon never races it.

pub mod commands;
pub mod suite;

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use tokio::{io::AsyncReadExt, sync::Semaphore};
use wh_core::{
    Actor, BenchmarkRunId, EventMeta, NewEvent, RecordedEvent, TaskId,
    benchmark::{BenchmarkContext, BenchmarkEvent, CheckResult},
    task::{TaskEvent, TaskSource, TaskStatus},
};
use wh_eventstore::{EventHandler, EventStore, ExpectedVersion, StoreError};
use wh_factory::Factory;
use wh_foreman::Foreman;
use wh_runner::{AgentRuntime, Runner, RunnerConfig, worktree::Worktree};

pub use commands::Request;
pub use suite::{Case, CaseRepo, Suite};

const CHECK_OUTPUT_MAX: usize = 4000;

#[derive(Clone)]
pub enum Dispatch {
    /// Run inside the caller (tests, `wh bench run`).
    Inline,
    /// Run in the background, at most `n` at a time.
    Spawn(Arc<Semaphore>),
}

#[derive(Debug, Clone)]
pub struct BenchConfig {
    /// Run files under `<data_dir>/bench/<run>`; worktrees and transcripts as for the runner.
    pub data_dir: PathBuf,
}

/// What one benchmark run produced.
#[derive(Debug, Clone, PartialEq)]
pub enum RunResult {
    Completed { cases: u32, passed: u32, cost_usd: f64, aborted: Option<String> },
    Failed { message: String },
    /// Not a pending request (already claimed).
    NotPending,
}

/// Progress callback for case runs (used by the CLI to print as it goes).
pub type Progress = Arc<dyn Fn(&BenchmarkEvent) + Send + Sync>;

pub struct BenchService<R> {
    inner: Arc<Inner<R>>,
    dispatch: Dispatch,
}

impl<R> Clone for BenchService<R> {
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone(), dispatch: self.dispatch.clone() }
    }
}

struct Inner<R> {
    store: EventStore,
    make_runtime: Box<dyn Fn() -> R + Send + Sync>,
    config: BenchConfig,
    progress: Option<Progress>,
    task_db_admin: Option<String>,
}

/// Writer position on a run's stream (the runner is its only writer once claimed).
struct RunLog {
    id: BenchmarkRunId,
    version: i64,
    cause: Option<uuid::Uuid>,
}

/// Everything resolved before a run is claimed.
struct Prepared {
    suite: Suite,
    factory: Factory,
    factory_commit: Option<String>,
    /// Detached worktree holding the factory at `git_ref` (removed after the run).
    factory_worktree: Option<(PathBuf, PathBuf)>,
    plan: Vec<(Case, u32)>,
}

impl<R: AgentRuntime + 'static> BenchService<R> {
    pub fn new(
        store: EventStore,
        make_runtime: impl Fn() -> R + Send + Sync + 'static,
        config: BenchConfig,
        dispatch: Dispatch,
    ) -> Self {
        let inner = Inner { store, make_runtime: Box::new(make_runtime), config, progress: None, task_db_admin: None };
        Self { inner: Arc::new(inner), dispatch }
    }

    /// Passed to [`Foreman::with_task_db_admin`] (tests).
    pub fn with_task_db_admin(mut self, url: impl Into<String>) -> Self {
        Arc::get_mut(&mut self.inner).expect("set the admin URL before cloning").task_db_admin = Some(url.into());
        self
    }

    pub fn with_progress(mut self, progress: Progress) -> Self {
        Arc::get_mut(&mut self.inner).expect("set progress before cloning").progress = Some(progress);
        self
    }

    /// Request a run and execute it now, in this process.
    pub async fn run_now(&self, actor: Actor, req: &Request) -> anyhow::Result<(BenchmarkRunId, RunResult)> {
        let (_, requested) = req.to_event().map_err(|e| anyhow::anyhow!("{e}"))?;
        let id = BenchmarkRunId::new();
        let p = self.inner.prepare(id, &requested).await?;
        let started = p.started_event();
        let meta = EventMeta::new(id.0, actor);
        let rec = self
            .inner
            .store
            .append(
                &id.stream_id(),
                ExpectedVersion::NoStream,
                vec![NewEvent::from_domain(&requested, meta.clone())?, NewEvent::from_domain(&started, meta)?],
            )
            .await?;
        let r = self.inner.execute(id, p, rec.last().map(|e| e.event_id)).await;
        wh_metrics::catch_up_all(&self.inner.store).await?;
        Ok((id, r?))
    }

    /// Execute a pending request now.
    pub async fn run(&self, id: BenchmarkRunId) -> anyhow::Result<RunResult> {
        self.inner.run(id).await
    }

    /// Run `id` according to the dispatch mode.
    pub async fn dispatch(&self, id: BenchmarkRunId) {
        let inner = self.inner.clone();
        let job = async move {
            match inner.run(id).await {
                Ok(RunResult::NotPending) => {}
                Ok(r) => tracing::info!(run = %id, result = ?r, "benchmark finished"),
                Err(e) => tracing::error!(run = %id, "benchmark failed: {e:#}"),
            }
            if let Err(e) = wh_metrics::catch_up_all(&inner.store).await {
                tracing::warn!("projection catch-up after benchmark: {e:#}");
            }
        };
        match &self.dispatch {
            Dispatch::Inline => job.await,
            Dispatch::Spawn(limit) => {
                let limit = limit.clone();
                tokio::spawn(async move {
                    let _permit = limit.acquire_owned().await;
                    job.await
                });
            }
        }
    }

    /// Call once at startup. Runs whose process is gone are marked failed (their
    /// running case tasks blocked); returns (that count, requests still waiting).
    pub async fn recover(&self) -> anyhow::Result<(usize, Vec<BenchmarkRunId>)> {
        let store = &self.inner.store;
        let (mut failed, mut pending) = (0, Vec::new());
        for (stream, version, head) in store.stream_heads(&format!("{}-", BenchmarkRunId::STREAM_PREFIX)).await? {
            let Some(id) = BenchmarkRunId::from_stream_id(&stream) else { continue };
            match head.as_str() {
                "BenchmarkCompleted" | "BenchmarkFailed" => continue,
                "BenchmarkRequested" => {
                    pending.push(id);
                    continue;
                }
                _ => {}
            }
            let events = store.read_stream(&stream).await?;
            let pid = events.iter().find_map(|e| match e.decode::<BenchmarkEvent>() {
                Ok(BenchmarkEvent::BenchmarkStarted { pid, .. }) => pid,
                _ => None,
            });
            if pid.is_some_and(|p| p != std::process::id() && process_alive(p)) {
                continue; // still running in another process (e.g. `wh bench run`)
            }
            const REASON: &str = "interrupted: the benchmark runner stopped while this run was in progress";
            let ev = BenchmarkEvent::BenchmarkFailed { message: REASON.into() };
            let meta = self.inner.meta(id, events.last().map(|e| e.event_id));
            if store.append(&stream, ExpectedVersion::Exact(version), vec![NewEvent::from_domain(&ev, meta)?]).await.is_ok() {
                failed += 1;
                wh_foreman::recover_interrupted(store, REASON, None, |s| {
                    s.benchmark.as_ref().is_some_and(|b| b.run == id)
                })
                .await?;
            }
        }
        Ok((failed, pending))
    }
}

fn process_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

impl Prepared {
    fn started_event(&self) -> BenchmarkEvent {
        BenchmarkEvent::BenchmarkStarted {
            factory: self.factory.name.clone(),
            factory_version: self.factory.version.clone(),
            factory_commit: self.factory_commit.clone(),
            suite_version: self.suite.version.clone(),
            planned: self.plan.len() as u32,
            pid: Some(std::process::id()),
        }
    }
}

impl<R: AgentRuntime + 'static> Inner<R> {
    fn meta(&self, run: BenchmarkRunId, cause: Option<uuid::Uuid>) -> EventMeta {
        let mut meta = EventMeta::new(run.0, Actor::System { component: "bench".into() });
        meta.causation_id = cause;
        meta
    }

    fn run_dir(&self, run: BenchmarkRunId) -> PathBuf {
        self.config.data_dir.join("bench").join(run.to_string())
    }

    async fn run(&self, id: BenchmarkRunId) -> anyhow::Result<RunResult> {
        let events = self.store.read_stream(&id.stream_id()).await?;
        let [first] = events.as_slice() else { return Ok(RunResult::NotPending) };
        let requested: BenchmarkEvent = first.decode()?;
        if !matches!(requested, BenchmarkEvent::BenchmarkRequested { .. }) {
            return Ok(RunResult::NotPending);
        }
        let (claim, prepared) = match self.prepare(id, &requested).await {
            Ok(p) => (p.started_event(), Ok(p)),
            Err(e) => (BenchmarkEvent::BenchmarkFailed { message: format!("{e:#}") }, Err(e)),
        };
        let meta = self.meta(id, Some(first.event_id));
        let rec = match self.store.append(&id.stream_id(), ExpectedVersion::Exact(1), vec![NewEvent::from_domain(&claim, meta)?]).await {
            Ok(r) => r,
            Err(StoreError::Conflict { .. }) => {
                if let Ok(p) = prepared {
                    p.cleanup().await;
                }
                return Ok(RunResult::NotPending);
            }
            Err(e) => return Err(e.into()),
        };
        match prepared {
            Ok(p) => self.execute(id, p, rec.last().map(|e| e.event_id)).await,
            Err(e) => Ok(RunResult::Failed { message: format!("{e:#}") }),
        }
    }

    /// Load the suite and the factory variant (checking it out at `git_ref`).
    async fn prepare(&self, id: BenchmarkRunId, requested: &BenchmarkEvent) -> anyhow::Result<Prepared> {
        let BenchmarkEvent::BenchmarkRequested { suite_file, git_ref, repeat, cases, .. } = requested else {
            bail!("not a BenchmarkRequested event");
        };
        let suite = Suite::load(Path::new(suite_file))?;
        let plan: Vec<(Case, u32)> = suite.plan(cases, *repeat)?.into_iter().map(|(c, r)| (c.clone(), r)).collect();
        let Some(git_ref) = git_ref else {
            let factory = Factory::load(&suite.factory_file)?;
            return Ok(Prepared { suite, factory, factory_commit: None, factory_worktree: None, plan });
        };
        let dir = suite.factory_file.parent().context("factory file has no parent")?;
        let root = PathBuf::from(git(dir, &["rev-parse", "--show-toplevel"]).await.context("factory is not in a git repo")?);
        let root = std::fs::canonicalize(&root)?;
        let rel = suite.factory_file.strip_prefix(&root).context("factory file is outside its git repo")?.to_owned();
        let commit = git(&root, &["rev-parse", "--verify", "--quiet", &format!("{git_ref}^{{commit}}")])
            .await
            .with_context(|| format!("unknown git ref '{git_ref}' in {}", root.display()))?;
        let wt = self.run_dir(id).join("factory");
        if let Some(parent) = wt.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let wt_str = wt.to_str().context("non-UTF-8 path")?;
        git(&root, &["worktree", "prune"]).await?;
        git(&root, &["worktree", "add", "--detach", wt_str, &commit]).await?;
        let loaded = Factory::load(&wt.join(&rel));
        let prepared = Prepared {
            suite,
            factory: match loaded {
                Ok(f) => f,
                Err(e) => {
                    let _ = git(&root, &["worktree", "remove", "--force", wt_str]).await;
                    return Err(e.context(format!("factory at {git_ref} ({})", &commit[..12.min(commit.len())])));
                }
            },
            factory_commit: Some(commit),
            factory_worktree: Some((root, wt)),
            plan,
        };
        Ok(prepared)
    }

    async fn execute(&self, id: BenchmarkRunId, p: Prepared, cause: Option<uuid::Uuid>) -> anyhow::Result<RunResult> {
        let t0 = Instant::now();
        let mut log = RunLog { id, version: 2, cause }; // Requested + Started
        let runner = Runner::new(self.store.clone(), (self.make_runtime)(), RunnerConfig { data_dir: self.config.data_dir.clone() });
        let mut foreman = Foreman::for_benchmark(self.store.clone(), p.factory.clone(), runner, id);
        if let Some(url) = &self.task_db_admin {
            foreman = foreman.with_task_db_admin(url.clone());
        }
        let (mut cases, mut passed, mut cost_usd, mut aborted) = (0u32, 0u32, 0.0f64, None);

        for (case, repeat) in &p.plan {
            if let Some(limit) = p.suite.max_cost_usd.filter(|l| cost_usd >= *l) {
                aborted = Some(format!("max_cost_usd ${limit} reached after {cases} case run(s) (${cost_usd:.4})"));
                break;
            }
            let task = TaskId::new();
            let finished = self.run_case(&mut foreman, &p, case, *repeat, task, &mut log).await;
            let finished = match finished {
                Ok(ev) => ev,
                Err(e) if e.downcast_ref::<StoreError>().is_some() => return Err(e), // run record conflict: stop
                Err(e) => BenchmarkEvent::BenchmarkCaseFinished {
                    case: case.id.clone(),
                    repeat: *repeat,
                    task_id: task,
                    task_status: "error".into(),
                    passed: false,
                    check: None,
                    cost_usd: self.task_cost(task).await.map(|c| c.1).unwrap_or(0.0),
                    duration_ms: 0,
                    sessions: 0,
                    rework_loops: 0,
                    message: Some(format!("{e:#}")),
                },
            };
            if let BenchmarkEvent::BenchmarkCaseFinished { passed: ok, cost_usd: c, .. } = &finished {
                cases += 1;
                passed += u32::from(*ok);
                cost_usd += c;
            }
            self.emit(&mut log, finished).await?;
        }

        let done = BenchmarkEvent::BenchmarkCompleted {
            cases,
            passed,
            cost_usd,
            duration_ms: t0.elapsed().as_millis() as u64,
            aborted: aborted.clone(),
        };
        self.emit(&mut log, done).await?;
        p.cleanup().await;
        Ok(RunResult::Completed { cases, passed, cost_usd, aborted })
    }

    async fn emit(&self, log: &mut RunLog, ev: BenchmarkEvent) -> anyhow::Result<()> {
        if let Some(cb) = &self.progress {
            cb(&ev);
        }
        let meta = self.meta(log.id, log.cause);
        let rec = self
            .store
            .append(&log.id.stream_id(), ExpectedVersion::Exact(log.version), vec![NewEvent::from_domain(&ev, meta)?])
            .await?;
        log.version += 1;
        log.cause = rec.last().map(|e| e.event_id);
        Ok(())
    }

    async fn run_case(
        &self,
        foreman: &mut Foreman<R>,
        p: &Prepared,
        case: &Case,
        repeat: u32,
        task: TaskId,
        log: &mut RunLog,
    ) -> anyhow::Result<BenchmarkEvent> {
        let id = log.id;
        let t0 = Instant::now();
        let slot = format!("{}-{repeat}", case.id);
        let (repo, base_ref) = match &case.repo {
            CaseRepo::Factory => (None, case.base_ref.clone()),
            CaseRepo::Repo(r) => (Some(r.clone()), case.base_ref.clone()),
            CaseRepo::Fixture(dir) => {
                let repo = self.run_dir(id).join("repos").join(&slot);
                fixture_repo(dir, &repo).await.with_context(|| format!("fixture {}", dir.display()))?;
                (Some(repo), Some("main".to_owned()))
            }
        };
        let ctx = BenchmarkContext {
            run: id,
            case: case.id.clone(),
            repo: repo.as_ref().map(|r| r.display().to_string()),
            base_ref,
        };
        let created = TaskEvent::TaskCreated {
            title: case.title.clone(),
            body: case.body.clone(),
            source: TaskSource::Benchmark,
            factory: Some(p.factory.name.clone()),
            benchmark: Some(ctx),
            external_ref: None,
        };
        let mut meta = EventMeta::new(task.0, Actor::System { component: "bench".into() });
        meta.causation_id = log.cause;
        meta.factory_version = Some(p.factory.version.clone());
        self.store.append(&task.stream_id(), ExpectedVersion::NoStream, vec![NewEvent::from_domain(&created, meta)?]).await?;
        self.emit(log, BenchmarkEvent::BenchmarkCaseStarted { case: case.id.clone(), repeat, task_id: task }).await?;

        let state = foreman.drive(task).await?;
        let (task_status, mut message) = match &state.status {
            TaskStatus::Done => ("done", None),
            TaskStatus::Blocked { reason } => ("blocked", Some(reason.clone())),
            TaskStatus::Cancelled => ("cancelled", None),
            other => ("error", Some(format!("pipeline stopped in state {other:?}"))),
        };
        let check = match (&case.check, task_status) {
            (Some(cmd), "done") => {
                let repo = repo.clone().unwrap_or_else(|| p.factory.repo.clone());
                let dir = self.run_dir(id).join("check").join(&slot);
                let suite_dir = p.suite.path.parent().unwrap_or(Path::new("/"));
                let mut env = vec![("WH_BENCH_SUITE_DIR", suite_dir.as_os_str()), ("WH_BENCH_CASE", case.id.as_ref())];
                // The check runs agent-written code: it gets the case task's database, never ours.
                let database = foreman.task_database_url(task).await.unwrap_or_else(|e| {
                    tracing::warn!(%task, "task database for the check: {e:#}");
                    None
                });
                if let Some(url) = &database {
                    env.push(("DATABASE_URL", url.as_ref()));
                }
                let c = run_check(&repo, &Foreman::<R>::task_branch(task), &dir, cmd, &env, case.check_timeout).await;
                if !c.passed {
                    message = Some(match c.exit_code {
                        Some(code) => format!("check failed (exit {code})"),
                        None => "check did not finish (timeout or could not start)".into(),
                    });
                }
                Some(c)
            }
            _ => None,
        };
        let passed = task_status == "done" && check.as_ref().is_none_or(|c| c.passed);
        let (sessions, cost_usd) = self.task_cost(task).await?;
        Ok(BenchmarkEvent::BenchmarkCaseFinished {
            case: case.id.clone(),
            repeat,
            task_id: task,
            task_status: task_status.into(),
            passed,
            check,
            cost_usd,
            duration_ms: t0.elapsed().as_millis() as u64,
            sessions,
            rework_loops: state.rework_loops,
            message,
        })
    }

    /// (sessions, total cost) of a task, straight from the log.
    async fn task_cost(&self, task: TaskId) -> anyhow::Result<(u32, f64)> {
        let (n, cost): (i64, f64) = sqlx::query_as(
            "SELECT count(*) FILTER (WHERE event_type = 'AgentDispatched'), \
               COALESCE(sum((payload->>'cost_usd')::float8) FILTER (WHERE event_type = 'AgentUsageReported'), 0) \
             FROM events WHERE meta->>'correlation_id' = $1 AND stream_id LIKE 'session-%'",
        )
        .bind(task.0.to_string())
        .fetch_one(self.store.pool())
        .await?;
        Ok((n as u32, cost))
    }
}

impl Prepared {
    async fn cleanup(&self) {
        if let Some((root, wt)) = &self.factory_worktree
            && let Some(wt) = wt.to_str()
            && let Err(e) = git(root, &["worktree", "remove", "--force", wt]).await
        {
            tracing::warn!("failed to remove factory worktree {wt}: {e:#}");
        }
    }
}

async fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = tokio::process::Command::new("git").arg("-C").arg(dir).args(args).output().await.context("failed to run git")?;
    if !out.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Copy `fixture` into `repo` and make it a single-commit git repo on `main`.
async fn fixture_repo(fixture: &Path, repo: &Path) -> anyhow::Result<()> {
    if repo.exists() {
        tokio::fs::remove_dir_all(repo).await?;
    }
    copy_dir(fixture, repo)?;
    git(repo, &["init", "-q", "-b", "main"]).await?;
    git(repo, &["add", "-A"]).await?;
    git(repo, &["-c", "user.name=WorkHub", "-c", "user.email=workhub@localhost", "commit", "-q", "--allow-empty", "-m", "fixture"])
        .await?;
    Ok(())
}

fn copy_dir(from: &Path, to: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let (src, dst) = (e.path(), to.join(e.file_name()));
        let ty = e.file_type()?;
        if ty.is_dir() {
            if e.file_name() != ".git" {
                copy_dir(&src, &dst)?;
            }
        } else if ty.is_file() {
            std::fs::copy(&src, &dst)?; // keeps permissions (executable scripts)
        }
    }
    Ok(())
}

/// Check out `branch` at `dir` and run `cmd` there with a timeout.
async fn run_check(
    repo: &Path,
    branch: &str,
    dir: &Path,
    cmd: &str,
    env: &[(&str, &std::ffi::OsStr)],
    timeout: Duration,
) -> CheckResult {
    let t0 = Instant::now();
    let fail = |output: String| CheckResult { passed: false, exit_code: None, output, duration_ms: t0.elapsed().as_millis() as u64 };
    let wt = match Worktree::create(repo, "HEAD", dir, branch).await {
        Ok(w) => w,
        Err(e) => return fail(format!("cannot check out {branch}: {e:#}")),
    };
    let result = async {
        let mut sh = tokio::process::Command::new("sh");
        wh_runner::strip_live_env(&mut sh);
        let mut child = match sh
            .arg("-c")
            .arg(cmd)
            .envs(env.iter().copied())
            .current_dir(&wt.path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => return fail(format!("cannot start check: {e}")),
        };
        let (mut out, mut err) = (child.stdout.take().expect("piped"), child.stderr.take().expect("piped"));
        let collect = async {
            let (mut a, mut b) = (Vec::new(), Vec::new());
            let _ = tokio::join!(out.read_to_end(&mut a), err.read_to_end(&mut b));
            let status = child.wait().await;
            (status, a, b)
        };
        match tokio::time::timeout(timeout, collect).await {
            Err(_) => fail(format!("check timed out after {}s", timeout.as_secs())),
            Ok((status, a, b)) => {
                let mut output = String::from_utf8_lossy(&a).into_owned();
                output.push_str(&String::from_utf8_lossy(&b));
                let code = status.ok().and_then(|s| s.code());
                CheckResult {
                    passed: code == Some(0),
                    exit_code: code,
                    output: tail(&output, CHECK_OUTPUT_MAX),
                    duration_ms: t0.elapsed().as_millis() as u64,
                }
            }
        }
    }
    .await;
    if let Err(e) = wt.remove().await {
        tracing::warn!("failed to remove check worktree: {e:#}");
    }
    result
}

fn tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &s[start..])
}

impl<R: AgentRuntime + 'static> EventHandler for BenchService<R> {
    fn name(&self) -> &str {
        "bench-runner"
    }

    async fn handle(&mut self, ev: &RecordedEvent) -> anyhow::Result<()> {
        if ev.event_type != "BenchmarkRequested" || ev.stream_version != 1 {
            return Ok(());
        }
        let Some(id) = BenchmarkRunId::from_stream_id(&ev.stream_id) else { return Ok(()) };
        self.dispatch(id).await;
        Ok(())
    }
}
