//! Housekeeping so a long-running factory does not fill the disk.
//!
//! [`sweep`] removes, in `$WH_DATA_DIR`:
//! - `worktrees/<session>` of finished (or unknown) sessions older than the grace
//!   period. The foreman's own sessions remove theirs right away; this catches
//!   crashes and `wh run` worktrees kept for inspection;
//! - `sandbox-home/<session>` of finished sessions, right away (it may hold a copy
//!   of the user's credentials);
//! - neither of these while the session's stage is paused or resumed;
//! - `scorer-work/*` and `bench/<run>` (finished runs) left by a crash, after the
//!   grace period;
//!
//! in the factory repo, task branches `wh/task/<id>`:
//! - of a merged PR: deleted right away (the work is on the base branch);
//! - of a cancelled task: deleted after the grace period;
//! - with `github:`, the same branches on the remote, plus (after the grace period)
//!   the remote branch of a PR closed without merging. Its local branch is kept,
//!   because the task is blocked and can be resumed. Stale remote-tracking refs
//!   are pruned afterwards;
//! - branches of tasks completed locally, blocked or still open are never touched;
//!
//! on the factory's `task_database` server:
//! - `wh_task_*` databases created by this event store whose task is done or merged,
//!   cancelled (after the grace period) or no longer exists;
//!
//! and build caches (`CARGO_TARGET_DIR` shared by the factory's agents) that grew
//! past their size limit, wiped only while no session is running.
//!
//! Transcripts are kept: they are small and the UI, scorers and improver read them.
//! Everything here is idempotent and only touches finished work.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    process::{Output, Stdio},
    sync::Arc,
    time::{Duration, SystemTime},
};

use anyhow::Context;
use sqlx::{Connection, PgConnection};
use tokio::{process::Command, sync::Notify};
use wh_core::{
    BenchmarkRunId, RecordedEvent, SessionId, TaskId,
    session::SessionEvent,
    task::{TaskEvent, TaskState, TaskStatus, task_branch},
};
use wh_eventstore::EventStore;
use wh_factory::Factory;

use crate::task_db;

/// The server holding the per-task databases.
#[derive(Clone)]
pub struct TaskDbAdmin {
    /// Variable the URL comes from.
    pub env: String,
    /// `None` when that variable is not set.
    pub url: Option<String>,
}

impl std::fmt::Debug for TaskDbAdmin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The URL holds a password.
        f.debug_struct("TaskDbAdmin").field("env", &self.env).field("set", &self.url.is_some()).finish()
    }
}

#[derive(Debug, Clone)]
pub struct CleanupConfig {
    pub data_dir: PathBuf,
    /// Repository holding the task branches (the factory's repo).
    pub repo: PathBuf,
    /// Remote of `repo` that task branches are pushed to (`github.remote`);
    /// None = never touch a remote.
    pub remote: Option<String>,
    /// Finished work younger than this is kept so it can still be looked at.
    pub grace: Duration,
    /// Directories (build caches) wiped when larger than the byte limit.
    pub caches: Vec<(PathBuf, u64)>,
    /// Report what would be removed without removing anything.
    pub dry_run: bool,
    /// Set when the factory has `task_database:`.
    pub task_db: Option<TaskDbAdmin>,
}

impl CleanupConfig {
    pub const DEFAULT_GRACE: Duration = Duration::from_secs(24 * 3600);
    pub const DEFAULT_CACHE_MAX_GB: u64 = 30;

    /// The factory's repo, plus every `CARGO_TARGET_DIR` its agents use as a shared cache.
    pub fn for_factory(f: &Factory, data_dir: PathBuf, grace: Duration, cache_max_bytes: u64) -> Self {
        let mut caches: Vec<PathBuf> =
            f.agents.values().filter_map(|a| a.env.get("CARGO_TARGET_DIR")).map(PathBuf::from).collect();
        caches.sort();
        caches.dedup();
        Self {
            data_dir,
            repo: f.repo.clone(),
            remote: f.github.as_ref().map(|g| g.remote.clone()),
            grace,
            caches: caches.into_iter().map(|p| (p, cache_max_bytes)).collect(),
            dry_run: false,
            task_db: f.task_database.as_ref().map(|db| TaskDbAdmin {
                env: db.admin_url_env.clone(),
                url: std::env::var(&db.admin_url_env).ok().filter(|v| !v.is_empty()),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Removed {
    /// `worktree`, `sandbox-home`, `scorer-work`, `bench`, `branch`, `remote-branch`,
    /// `database` or `cache`.
    pub kind: &'static str,
    pub what: String,
    pub bytes: u64,
}

#[derive(Debug, Default)]
pub struct CleanupReport {
    pub removed: Vec<Removed>,
    pub errors: Vec<String>,
}

impl CleanupReport {
    pub fn bytes(&self) -> u64 {
        self.removed.iter().map(|r| r.bytes).sum()
    }
}

/// `1.5 GB`, `320 MB`, `12 KB`.
pub fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    if v < 1024.0 {
        return format!("{b} B");
    }
    let mut unit = "";
    for u in UNITS {
        v /= 1024.0;
        unit = u;
        if v < 1024.0 {
            break;
        }
    }
    if v < 10.0 { format!("{v:.1} {unit}") } else { format!("{v:.0} {unit}") }
}

fn session_finished(head: &str) -> bool {
    matches!(head, "AgentSessionCompleted" | "AgentSessionFailed")
}

const REMOTE_TIMEOUT: Duration = Duration::from_secs(60);

/// The task can no longer have a session running for it. Not `PrClosed`: a PR can
/// be closed while a rework stage for it is still running.
fn task_over(head: &str) -> bool {
    matches!(head, "TaskCancelled" | "TaskCompleted" | "PrMerged" | "TaskBlocked")
}

/// One pass over everything described in the module docs.
pub async fn sweep(store: &EventStore, cfg: &CleanupConfig) -> anyhow::Result<CleanupReport> {
    let mut r = CleanupReport::default();
    let task_heads: HashMap<String, String> = store
        .stream_heads(&format!("{}-", TaskId::STREAM_PREFIX))
        .await?
        .into_iter()
        .map(|(stream, _, head)| (stream, head))
        .collect();
    let mut sessions: HashMap<SessionId, bool> = HashMap::new();
    for (stream, _, head) in store.stream_heads(&format!("{}-", SessionId::STREAM_PREFIX)).await? {
        let Some(id) = SessionId::from_stream_id(&stream) else { continue };
        let mut done = session_finished(&head);
        if !done {
            // Never finished, but its task is over (a killed `wh run`, a crash): an orphan.
            let first = store.read_stream(&stream).await?.into_iter().next();
            if let Some(Ok(SessionEvent::AgentDispatched { task_id, .. })) = first.map(|e| e.decode::<SessionEvent>()) {
                done = task_heads.get(&task_id.stream_id()).is_some_and(|h| task_over(h));
            }
        }
        sessions.insert(id, done);
    }
    // A directory named after a session nobody recorded is an orphan, i.e. finished.
    let finished = |name: &str| name.parse::<SessionId>().ok().map(|id| sessions.get(&id).copied().unwrap_or(true));

    let mut running = 0;
    for kind in ["worktree", "sandbox-home"] {
        let root = if kind == "worktree" { "worktrees" } else { kind };
        for (path, name) in entries(&cfg.data_dir.join(root)).await {
            match finished(&name) {
                Some(true) => {}
                Some(false) => {
                    running += usize::from(kind == "worktree");
                    continue;
                }
                None => continue,
            }
            // A finished session may still belong to a paused stage.
            let held = match name.parse::<SessionId>() {
                Ok(id) => held_by_pause(store, id).await,
                Err(_) => Ok(None),
            };
            match held {
                Ok(Some(task_running)) => running += usize::from(task_running),
                // A sandbox home may hold credentials: no grace period for it.
                Ok(None) if kind == "sandbox-home" || age(&path) >= cfg.grace => {
                    remove_tree(&path, kind, cfg, &mut r).await
                }
                Ok(None) => {}
                Err(e) => r.errors.push(format!("keep {}: {e:#}", path.display())),
            }
        }
    }
    // A grading takes minutes; never race one, whatever the grace period.
    let scorer_grace = cfg.grace.max(Duration::from_secs(3600));
    for (path, _) in entries(&cfg.data_dir.join("scorer-work")).await {
        if age(&path) >= scorer_grace {
            remove_tree(&path, "scorer-work", cfg, &mut r).await;
        }
    }
    let bench_dirs = entries(&cfg.data_dir.join("bench")).await;
    if !bench_dirs.is_empty() {
        let runs: HashMap<BenchmarkRunId, String> = store
            .stream_heads(&format!("{}-", BenchmarkRunId::STREAM_PREFIX))
            .await?
            .into_iter()
            .filter_map(|(stream, _, head)| Some((BenchmarkRunId::from_stream_id(&stream)?, head)))
            .collect();
        for (path, name) in bench_dirs {
            let Ok(run) = name.parse::<BenchmarkRunId>() else { continue };
            let done = runs.get(&run).is_none_or(|h| matches!(h.as_str(), "BenchmarkCompleted" | "BenchmarkFailed"));
            if done && age(&path) >= cfg.grace {
                remove_tree(&path, "bench", cfg, &mut r).await;
            }
        }
    }

    delete_task_branches(store, cfg, &mut r).await?;

    if let Some(admin) = &cfg.task_db {
        match &admin.url {
            None => r.errors.push(format!("task databases not cleaned up: {} is not set", admin.env)),
            Some(url) => {
                if let Err(e) = drop_task_databases(store, url, cfg, &mut r).await {
                    r.errors.push(format!("clean up task databases: {e:#}"));
                }
            }
        }
    }

    if running == 0 {
        for (path, max) in &cfg.caches {
            let size = dir_size(path.clone()).await;
            if size > *max {
                tracing::info!(cache = %path.display(), size = %human_bytes(size), max = %human_bytes(*max), "cache over its limit");
                remove_tree(path, "cache", cfg, &mut r).await;
            }
        }
    }
    Ok(r)
}

/// Whether the directories of `session` belong to a stage that paused and has not
/// ended: `Some(true)` while it is running, `Some(false)` while it waits for answers.
async fn held_by_pause(store: &EventStore, session: SessionId) -> anyhow::Result<Option<bool>> {
    let events = store.read_stream(&session.stream_id()).await?;
    let Some(first) = events.first() else { return Ok(None) };
    let SessionEvent::AgentDispatched { task_id, .. } = first.decode()? else { return Ok(None) };
    // `AgentAsked` is appended after this, so the task may not know of the pause yet.
    let ended_paused = match events.last() {
        Some(e) => matches!(e.decode()?, SessionEvent::AgentSessionCompleted { paused: true, .. }),
        None => false,
    };
    let state = task_state(&store.read_stream(&task_id.stream_id()).await?)?;
    let running = match state.status {
        TaskStatus::Running { .. } => true,
        TaskStatus::AwaitingInput { .. } => false,
        _ => return Ok(None),
    };
    let is_home = state.paused.and_then(|p| p.resume).is_some_and(|h| h.home_session == session);
    Ok((is_home || (running && ended_paused)).then_some(running))
}

fn task_state(events: &[RecordedEvent]) -> anyhow::Result<TaskState> {
    let mut state = TaskState::default();
    for e in events {
        state.apply(e.stream_version, e.decode::<TaskEvent>()?);
    }
    Ok(state)
}

async fn delete_task_branches(store: &EventStore, cfg: &CleanupConfig, r: &mut CleanupReport) -> anyhow::Result<()> {
    let local = match git(&cfg.repo, &["for-each-ref", "--format=%(refname:short)", "refs/heads/wh/task/"]).await {
        Ok(out) => out,
        Err(e) => {
            r.errors.push(format!("list task branches in {}: {e:#}", cfg.repo.display()));
            return Ok(());
        }
    };
    let local: HashSet<&str> = local.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    // Asked of the remote itself, not derived from the local branches: a failed
    // delete is retried by the next sweep even when the local branch is gone.
    let mut remote = None;
    if let Some(name) = &cfg.remote {
        match remote_task_branches(&cfg.repo, name).await {
            Ok(branches) => remote = Some((name.as_str(), branches)),
            Err(e) => r.errors.push(format!("list task branches on {name}: {e:#}")),
        }
    }

    if !local.is_empty() || remote.as_ref().is_some_and(|(_, b)| !b.is_empty()) {
        for (stream, _, head) in store.stream_heads(&format!("{}-", TaskId::STREAM_PREFIX)).await? {
            let Some(task) = TaskId::from_stream_id(&stream) else { continue };
            let branch = task_branch(task);
            let has_local = local.contains(branch.as_str());
            // (remote name, commit the branch points at there)
            let on_remote = remote.as_ref().and_then(|(name, b)| Some((*name, b.get(&branch)?)));
            if !has_local && on_remote.is_none() {
                continue;
            }
            let due = match branches_due(store, &stream, &head, cfg.grace).await {
                Ok(due) => due,
                Err(e) => {
                    r.errors.push(format!("read {stream}: {e:#}"));
                    continue;
                }
            };
            if due.local && has_local {
                // Fails e.g. when checked out somewhere: leave it and say so.
                let res = match cfg.dry_run {
                    true => Ok(()),
                    false => git(&cfg.repo, &["branch", "-D", &branch]).await.map(drop),
                };
                match res {
                    Ok(()) => r.removed.push(Removed { kind: "branch", what: branch.clone(), bytes: 0 }),
                    Err(e) => r.errors.push(format!("delete branch {branch}: {e:#}")),
                }
            }
            if due.remote
                && let Some((name, oid)) = on_remote
            {
                let res = match cfg.dry_run {
                    true => Ok(true),
                    false => delete_remote_branch(&cfg.repo, name, &branch, oid).await,
                };
                match res {
                    Ok(true) => r.removed.push(Removed { kind: "remote-branch", what: branch, bytes: 0 }),
                    Ok(false) => {}
                    Err(e) => r.errors.push(format!("delete branch {branch} on {name}: {e:#}")),
                }
            }
        }
    }

    // Also drops the tracking refs of branches GitHub deleted by itself.
    if let Some((name, _)) = &remote
        && !cfg.dry_run
        && let Err(e) = remote_git(&cfg.repo, &["remote", "prune", name]).await
    {
        r.errors.push(format!("prune {name}: {e:#}"));
    }
    Ok(())
}

/// Which copies of a task's branch may be deleted now.
#[derive(Default)]
struct Due {
    local: bool,
    remote: bool,
}

async fn branches_due(store: &EventStore, stream: &str, head: &str, grace: Duration) -> anyhow::Result<Due> {
    if head == "PrMerged" {
        return Ok(Due { local: true, remote: true });
    }
    let (status, aged) = task_status(store, stream, grace).await?;
    Ok(match status {
        // From the state, not the head: PR events can still arrive after the cancel.
        TaskStatus::Cancelled => Due { local: aged, remote: aged },
        // Resume continues on the local branch and ships (pushes) it again.
        TaskStatus::Blocked { .. } if head == "PrClosed" => Due { local: false, remote: aged },
        _ => Due::default(),
    })
}

/// The folded status of a task, and whether its last event is older than `grace`.
async fn task_status(store: &EventStore, stream: &str, grace: Duration) -> anyhow::Result<(TaskStatus, bool)> {
    let events = store.read_stream(stream).await?;
    let state = task_state(&events)?;
    let aged = events.last().is_some_and(|e| (chrono::Utc::now() - e.recorded_at).to_std().unwrap_or_default() >= grace);
    Ok((state.status, aged))
}

/// Task branches (`wh/task/<id>`) on `remote`, with the commit each points at.
async fn remote_task_branches(repo: &Path, remote: &str) -> anyhow::Result<HashMap<String, String>> {
    let out = remote_git(repo, &["ls-remote", "--heads", remote, "refs/heads/wh/task/*"]).await?;
    Ok(out
        .lines()
        .filter_map(|l| {
            let (oid, refname) = l.split_once('\t')?;
            let branch = refname.strip_prefix("refs/heads/").filter(|b| b.starts_with("wh/task/"))?;
            Some((branch.to_owned(), oid.to_owned()))
        })
        .collect())
}

/// Delete `branch`, last seen at `oid`, on `remote`. `false` = it was already gone
/// (GitHub can delete head branches by itself).
async fn delete_remote_branch(repo: &Path, remote: &str, branch: &str, oid: &str) -> anyhow::Result<bool> {
    anyhow::ensure!(branch.starts_with("wh/task/"), "refusing to delete {branch}: not a task branch");
    let refname = format!("refs/heads/{branch}");
    // The lease makes the push fail when the ref is gone; a plain delete of a
    // missing ref "succeeds". It also refuses if the branch moved since listing.
    let lease = format!("--force-with-lease={refname}:{oid}");
    let delete = format!(":{refname}");
    match remote_git(repo, &["push", "--quiet", &lease, remote, &delete]).await {
        Ok(_) => Ok(true),
        // Ask the remote why, rather than match git's error text.
        Err(e) => match remote_git(repo, &["ls-remote", remote, &refname]).await {
            Ok(out) if out.trim().is_empty() => Ok(false),
            _ => Err(e),
        },
    }
}

/// Whether a task's database may be dropped now.
async fn database_due(store: &EventStore, stream: &str, head: &str, grace: Duration) -> anyhow::Result<bool> {
    if head == "PrMerged" {
        return Ok(true);
    }
    let (status, aged) = task_status(store, stream, grace).await?;
    Ok(match status {
        TaskStatus::Done => true,
        // From the state, not the head: PR events can still arrive after the cancel.
        TaskStatus::Cancelled => aged,
        _ => false,
    })
}

/// Drop the task databases that are due. Only names [`task_db::db_name`] produces
/// and only databases this event store created (by their comment).
async fn drop_task_databases(
    store: &EventStore,
    admin_url: &str,
    cfg: &CleanupConfig,
    r: &mut CleanupReport,
) -> anyhow::Result<()> {
    let owner = task_db::owner_comment(store.instance_id().await?);
    let ready = format!("{owner} ready");
    let mut admin = PgConnection::connect(admin_url).await.context("connect to the task database server")?;
    // List the databases before the tasks: a task exists before its database does,
    // so one created meanwhile is never taken for an orphan.
    let databases: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT datname::text, shobj_description(oid, 'pg_database') FROM pg_database ORDER BY datname")
            .fetch_all(&mut admin)
            .await?;
    let mut tasks: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for (stream, _, head) in store.stream_heads(&format!("{}-", TaskId::STREAM_PREFIX)).await? {
        if let Some(task) = TaskId::from_stream_id(&stream) {
            tasks.entry(task_db::db_name(task)).or_default().push((stream, head));
        }
    }
    for (name, comment) in databases {
        let ours = comment.is_some_and(|c| c == owner || c == ready);
        if !ours || !task_db::is_task_db_name(&name) {
            continue;
        }
        // Without a task it is due; otherwise every task of that name must be over.
        let mut due = true;
        for (stream, head) in tasks.get(&name).into_iter().flatten() {
            due &= database_due(store, stream, head, cfg.grace).await?;
        }
        if !due {
            continue;
        }
        // `name` passed `is_task_db_name`, so it is safe to splice into DDL.
        if !cfg.dry_run
            && let Err(e) = sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)")).execute(&mut admin).await
        {
            r.errors.push(format!("drop database {name}: {e}"));
            continue;
        }
        r.removed.push(Removed { kind: "database", what: name, bytes: 0 });
    }
    admin.close().await?;
    Ok(())
}

/// Sweep now, then again whenever `wake` is notified (a task finished) or
/// `interval` elapses. Runs until the task is dropped/aborted.
pub async fn run_janitor(store: EventStore, cfg: CleanupConfig, wake: Arc<Notify>, interval: Duration) {
    loop {
        match sweep(&store, &cfg).await {
            Ok(r) => {
                if !r.removed.is_empty() {
                    tracing::info!(items = r.removed.len(), freed = %human_bytes(r.bytes()), "cleanup");
                }
                for e in &r.errors {
                    tracing::warn!("cleanup: {e}");
                }
            }
            Err(e) => tracing::warn!("cleanup failed: {e:#}"),
        }
        tokio::select! {
            _ = wake.notified() => {}
            _ = tokio::time::sleep(interval) => {}
        }
    }
}

/// Subdirectories of `dir` with their names (empty if `dir` does not exist).
async fn entries(dir: &Path) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else { return out };
    while let Ok(Some(e)) = rd.next_entry().await {
        if e.file_type().await.is_ok_and(|t| t.is_dir()) {
            out.push((e.path(), e.file_name().to_string_lossy().into_owned()));
        }
    }
    out
}

fn age(path: &Path) -> Duration {
    std::fs::symlink_metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .unwrap_or_default()
}

/// Total size of the files under `path` (symlinks are not followed).
async fn dir_size(path: PathBuf) -> u64 {
    tokio::task::spawn_blocking(move || {
        let mut total = 0;
        let mut stack = vec![path];
        while let Some(p) = stack.pop() {
            let Ok(m) = std::fs::symlink_metadata(&p) else { continue };
            if m.is_dir() {
                if let Ok(rd) = std::fs::read_dir(&p) {
                    stack.extend(rd.flatten().map(|e| e.path()));
                }
            } else {
                total += m.len();
            }
        }
        total
    })
    .await
    .unwrap_or(0)
}

/// Common git dir of the repo a linked worktree at `dir` belongs to
/// (`dir/.git` is a file `gitdir: <common>/worktrees/<name>`).
fn worktree_repo(dir: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(dir.join(".git")).ok()?;
    let gitdir = PathBuf::from(text.trim().strip_prefix("gitdir:")?.trim());
    let parent = gitdir.parent()?;
    (parent.file_name()? == "worktrees").then(|| parent.parent().map(Path::to_owned)).flatten()
}

/// Delete `path`; linked worktrees in it (itself or a direct child) are pruned
/// from their repos afterwards so git forgets them too.
async fn remove_tree(path: &Path, kind: &'static str, cfg: &CleanupConfig, r: &mut CleanupReport) {
    let mut repos: Vec<PathBuf> = worktree_repo(path).into_iter().collect();
    for (child, _) in entries(path).await {
        repos.extend(worktree_repo(&child));
    }
    let bytes = dir_size(path.to_owned()).await;
    if !cfg.dry_run {
        if let Err(e) = tokio::fs::remove_dir_all(path).await {
            r.errors.push(format!("remove {}: {e}", path.display()));
            return;
        }
        for repo in repos {
            if let Err(e) = Command::new("git").arg("--git-dir").arg(&repo).args(["worktree", "prune"]).output().await {
                r.errors.push(format!("git worktree prune in {}: {e}", repo.display()));
            }
        }
    }
    r.removed.push(Removed { kind, what: path.display().to_string(), bytes });
}

async fn git(repo: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git").arg("-C").arg(repo).args(args).output().await?;
    stdout_of(args, out)
}

/// [`git`] for commands that talk to a remote: never prompts and never hangs
/// the janitor.
async fn remote_git(repo: &Path, args: &[&str]) -> anyhow::Result<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo).args(args).env("GIT_TERMINAL_PROMPT", "0").env("LC_ALL", "C");
    cmd.stdin(Stdio::null()).kill_on_drop(true);
    let out = tokio::time::timeout(REMOTE_TIMEOUT, cmd.output())
        .await
        .with_context(|| format!("git {}: timed out after {}s", args.join(" "), REMOTE_TIMEOUT.as_secs()))??;
    stdout_of(args, out)
}

fn stdout_of(args: &[&str], out: Output) -> anyhow::Result<String> {
    anyhow::ensure!(out.status.success(), "git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_sizes() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(30 * 1024 * 1024 * 1024), "30 GB");
    }

    #[test]
    fn finds_the_repo_of_a_linked_worktree() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".git"), "gitdir: /src/repo/.git/worktrees/abc\n").unwrap();
        assert_eq!(worktree_repo(dir.path()), Some(PathBuf::from("/src/repo/.git")));
        std::fs::write(dir.path().join(".git"), "gitdir: /elsewhere/x\n").unwrap();
        assert_eq!(worktree_repo(dir.path()), None);
    }

    /// The race with GitHub's auto-delete: listed by `ls-remote`, gone before the push.
    #[tokio::test]
    async fn deleting_a_missing_remote_branch_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, bare) = (dir.path().join("repo"), dir.path().join("origin.git"));
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&bare).unwrap();
        git(&bare, &["init", "-q", "--bare"]).await.unwrap();
        git(&repo, &["init", "-q"]).await.unwrap();
        git(&repo, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "init"])
            .await
            .unwrap();
        git(&repo, &["remote", "add", "origin", bare.to_str().unwrap()]).await.unwrap();
        git(&repo, &["push", "-q", "origin", "HEAD:refs/heads/wh/task/x"]).await.unwrap();

        let oid = remote_task_branches(&repo, "origin").await.unwrap().remove("wh/task/x").unwrap();
        assert!(delete_remote_branch(&repo, "origin", "wh/task/x", &oid).await.unwrap());
        assert!(!delete_remote_branch(&repo, "origin", "wh/task/x", &oid).await.unwrap());
        assert!(delete_remote_branch(&repo, "origin", "main", &oid).await.is_err());
    }
}
