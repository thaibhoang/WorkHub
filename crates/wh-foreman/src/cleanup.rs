//! Housekeeping so a long-running factory does not fill the disk.
//!
//! [`sweep`] removes, in `$WH_DATA_DIR`:
//! - `worktrees/<session>` of finished (or unknown) sessions older than the grace
//!   period. The foreman's own sessions remove theirs right away; this catches
//!   crashes and `wh run` worktrees kept for inspection;
//! - `sandbox-home/<session>` of finished sessions, right away (it may hold a copy
//!   of the user's credentials);
//! - `scorer-work/*` and `bench/<run>` (finished runs) left by a crash, after the
//!   grace period;
//!
//! in the factory repo:
//! - task branches `wh/task/<id>` whose PR was merged (right away: the work is on
//!   the remote) or whose task was cancelled (after the grace period). Branches of
//!   tasks completed locally, blocked or still open are never touched;
//!
//! and build caches (`CARGO_TARGET_DIR` shared by the factory's agents) that grew
//! past their size limit, wiped only while no session is running.
//!
//! Transcripts are kept: they are small and the UI, scorers and improver read them.
//! Everything here is idempotent and only touches finished work.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use tokio::{process::Command, sync::Notify};
use wh_core::{BenchmarkRunId, SessionId, TaskId, session::SessionEvent, task::task_branch};
use wh_eventstore::EventStore;
use wh_factory::Factory;

#[derive(Debug, Clone)]
pub struct CleanupConfig {
    pub data_dir: PathBuf,
    /// Repository holding the task branches (the factory's repo).
    pub repo: PathBuf,
    /// Finished work younger than this is kept so it can still be looked at.
    pub grace: Duration,
    /// Directories (build caches) wiped when larger than the byte limit.
    pub caches: Vec<(PathBuf, u64)>,
    /// Report what would be removed without removing anything.
    pub dry_run: bool,
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
            grace,
            caches: caches.into_iter().map(|p| (p, cache_max_bytes)).collect(),
            dry_run: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Removed {
    /// `worktree`, `sandbox-home`, `scorer-work`, `bench`, `branch` or `cache`.
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

/// The task can no longer have a session running for it.
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
    for (path, name) in entries(&cfg.data_dir.join("worktrees")).await {
        match finished(&name) {
            Some(true) if age(&path) >= cfg.grace => remove_tree(&path, "worktree", cfg, &mut r).await,
            Some(false) => running += 1,
            _ => {}
        }
    }
    for (path, name) in entries(&cfg.data_dir.join("sandbox-home")).await {
        if finished(&name) == Some(true) {
            remove_tree(&path, "sandbox-home", cfg, &mut r).await;
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

async fn delete_task_branches(store: &EventStore, cfg: &CleanupConfig, r: &mut CleanupReport) -> anyhow::Result<()> {
    let refs = match git(&cfg.repo, &["for-each-ref", "--format=%(refname:short)", "refs/heads/wh/task/"]).await {
        Ok(out) => out,
        Err(e) => {
            r.errors.push(format!("list task branches in {}: {e:#}", cfg.repo.display()));
            return Ok(());
        }
    };
    let branches: HashSet<&str> = refs.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if branches.is_empty() {
        return Ok(());
    }
    for (stream, _, head) in store.stream_heads(&format!("{}-", TaskId::STREAM_PREFIX)).await? {
        let Some(task) = TaskId::from_stream_id(&stream) else { continue };
        let branch = task_branch(task);
        if !branches.contains(branch.as_str()) {
            continue;
        }
        let due = match head.as_str() {
            "PrMerged" => true,
            "TaskCancelled" => {
                let last = store.read_stream(&stream).await?.pop();
                last.is_some_and(|e| (chrono::Utc::now() - e.recorded_at).to_std().unwrap_or_default() >= cfg.grace)
            }
            _ => false,
        };
        if !due {
            continue;
        }
        if !cfg.dry_run
            && let Err(e) = git(&cfg.repo, &["branch", "-D", &branch]).await
        {
            // e.g. checked out somewhere: leave it and say so.
            r.errors.push(format!("delete branch {branch}: {e:#}"));
            continue;
        }
        r.removed.push(Removed { kind: "branch", what: branch, bytes: 0 });
    }
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
}
