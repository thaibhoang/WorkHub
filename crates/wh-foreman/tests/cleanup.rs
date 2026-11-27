//! `cleanup::sweep`: what gets removed, what is kept, and when.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use sqlx::PgPool;
use wh_core::{
    Actor, DomainEvent, EventMeta, NewEvent, SessionId, TaskId,
    session::SessionEvent,
    task::{TaskEvent, TaskSource, task_branch},
};
use wh_eventstore::{EventStore, ExpectedVersion};
use wh_foreman::cleanup::{CleanupConfig, CleanupReport, sweep};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

async fn append<E: DomainEvent>(store: &EventStore, stream: &str, task: TaskId, ev: E) {
    let meta = EventMeta::new(task.0, Actor::System { component: "test".into() });
    store.append(stream, ExpectedVersion::Any, vec![NewEvent::from_domain(&ev, meta).unwrap()]).await.unwrap();
}

async fn task(store: &EventStore, last: Option<TaskEvent>) -> TaskId {
    let t = TaskId::new();
    let created = TaskEvent::TaskCreated {
        title: "t".into(),
        body: None,
        source: TaskSource::Cli,
        factory: Some("test".into()),
        benchmark: None,
        external_ref: None,
    };
    append(store, &t.stream_id(), t, created).await;
    if let Some(ev) = last {
        append(store, &t.stream_id(), t, ev).await;
    }
    t
}

async fn session(store: &EventStore, task: TaskId, finished: bool) -> SessionId {
    let s = SessionId::new();
    let dispatched = SessionEvent::AgentDispatched {
        task_id: task,
        agent: "a".into(),
        stage: "implement".into(),
        runtime: "fake".into(),
        model: None,
        prompt: "p".into(),
        budget_usd: None,
        timeout_secs: 60,
        attempt: 1,
        retry_of: None,
        benchmark: None,
    };
    append(store, &s.stream_id(), task, dispatched).await;
    if finished {
        let done = SessionEvent::AgentSessionCompleted {
            num_turns: 1,
            duration_ms: 1,
            summary: None,
            transcript: None,
            commit: None,
        };
        append(store, &s.stream_id(), task, done).await;
    }
    s
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn sweeps_finished_work_and_keeps_the_rest(pool: PgPool) {
    let store = EventStore::new(pool);
    let dir = tempfile::tempdir().unwrap();
    let (repo, data) = (dir.path().join("repo"), dir.path().join("data"));
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hi").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"]);

    // Tasks: merged PR, cancelled, completed locally (its branch is the only copy of the work).
    let merged = task(&store, Some(TaskEvent::PrMerged { merged_at: None, merged_by: None })).await;
    let cancelled = task(&store, Some(TaskEvent::TaskCancelled { reason: "no".into() })).await;
    let local = task(&store, Some(TaskEvent::TaskCompleted { branch: None })).await;
    for t in [merged, cancelled, local] {
        git(&repo, &["branch", &task_branch(t), "main"]);
    }

    // Sessions: finished, still running, and a directory nobody recorded.
    let done = session(&store, local, true).await;
    let open = task(&store, None).await;
    let running = session(&store, open, false).await;
    let orphan = SessionId::new();
    // Never finished, but its task was cancelled (a killed `wh run`): an orphan too.
    let stale = session(&store, cancelled, false).await;
    let wt = |s: SessionId| data.join("worktrees").join(s.to_string());
    for (i, s) in [done, running].into_iter().enumerate() {
        git(&repo, &["worktree", "add", "-q", "-b", &format!("wh/x/{i}"), wt(s).to_str().unwrap(), "main"]);
    }
    std::fs::create_dir_all(wt(orphan)).unwrap();
    std::fs::create_dir_all(wt(stale)).unwrap();
    for s in [done, running] {
        std::fs::create_dir_all(data.join("sandbox-home").join(s.to_string())).unwrap();
    }
    let cache = dir.path().join("cargo-target");
    std::fs::create_dir_all(cache.join("debug")).unwrap();
    std::fs::write(cache.join("debug/big"), vec![0u8; 4096]).unwrap();

    let mut cfg = CleanupConfig {
        data_dir: data.clone(),
        repo: repo.clone(),
        remote: None,
        grace: Duration::from_secs(3600),
        caches: vec![(cache.clone(), 1024)],
        dry_run: true,
        task_db: None,
    };
    let kinds = |r: &wh_foreman::cleanup::CleanupReport| {
        let mut v: Vec<String> = r.removed.iter().map(|x| format!("{}:{}", x.kind, x.what.rsplit('/').next().unwrap())).collect();
        v.sort();
        v
    };

    // Within the grace period only the finished session's home and the merged
    // branch go; a dry run removes nothing.
    let r = sweep(&store, &cfg).await.unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let short = |t: TaskId| task_branch(t).rsplit('/').next().unwrap().to_owned();
    assert_eq!(kinds(&r), vec![format!("branch:{}", short(merged)), format!("sandbox-home:{done}")]);
    assert!(data.join("sandbox-home").join(done.to_string()).exists());

    // Grace over: finished and orphaned worktrees, the cancelled branch too. The
    // running session keeps its worktree and home, and the cache survives while
    // a session runs.
    cfg.grace = Duration::ZERO;
    cfg.dry_run = false;
    let r = sweep(&store, &cfg).await.unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let mut want = vec![
        format!("branch:{}", short(merged)),
        format!("branch:{}", short(cancelled)),
        format!("sandbox-home:{done}"),
        format!("worktree:{done}"),
        format!("worktree:{orphan}"),
        format!("worktree:{stale}"),
    ];
    want.sort();
    assert_eq!(kinds(&r), want);
    assert!(!wt(done).exists() && !wt(orphan).exists() && wt(running).exists());
    assert!(cache.exists());
    let branches = git(&repo, &["branch", "--list", "wh/task/*"]);
    assert!(branches.contains(&task_branch(local)) && !branches.contains(&task_branch(merged)), "{branches}");
    let worktrees = git(&repo, &["worktree", "list"]);
    assert!(!worktrees.contains(&done.to_string()), "git forgot the removed worktree: {worktrees}");

    // Once nothing runs, the oversized cache is wiped.
    let s = running.stream_id();
    let ev = SessionEvent::AgentSessionCompleted { num_turns: 1, duration_ms: 1, summary: None, transcript: None, commit: None };
    append(&store, &s, open, ev).await;
    let r = sweep(&store, &cfg).await.unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert!(kinds(&r).contains(&"cache:cargo-target".to_string()), "{:?}", kinds(&r));
    assert!(!cache.exists() && !wt(running).exists());

    // Idempotent.
    assert!(sweep(&store, &cfg).await.unwrap().removed.is_empty());
}

/// Closing a PR does not stop a rework that is running: its session is no orphan.
#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn keeps_a_running_session_when_its_pr_is_closed(pool: PgPool) {
    let store = EventStore::new(pool);
    let dir = tempfile::tempdir().unwrap();
    let (repo, _) = repo_with_remote(dir.path());
    let data = dir.path().join("data");

    let rework = TaskEvent::StageEntered { stage: "implement".into(), feedback: Some("fix CI".into()) };
    let t = task(&store, Some(rework)).await;
    let running = session(&store, t, false).await;
    append(&store, &t.stream_id(), t, TaskEvent::PrClosed { closed_at: None }).await;

    let worktree = data.join("worktrees").join(running.to_string());
    let home = data.join("sandbox-home").join(running.to_string());
    let cache = dir.path().join("cargo-target");
    for d in [&worktree, &home, &cache] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(cache.join("big"), vec![0u8; 4096]).unwrap();

    let cfg = CleanupConfig {
        data_dir: data,
        repo,
        remote: Some("origin".into()),
        grace: Duration::ZERO,
        caches: vec![(cache.clone(), 1024)],
        dry_run: false,
        task_db: None,
    };
    let r = sweep(&store, &cfg).await.unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert!(r.removed.is_empty(), "{:?}", r.removed);
    assert!(worktree.exists() && home.exists() && cache.exists());
}

/// A repo with one commit whose `origin` is a local bare repo.
fn repo_with_remote(dir: &Path) -> (PathBuf, PathBuf) {
    let (repo, bare) = (dir.join("repo"), dir.join("origin.git"));
    for d in [&repo, &bare] {
        std::fs::create_dir_all(d).unwrap();
    }
    git(&bare, &["init", "-q", "--bare", "-b", "main"]);
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hi").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"]);
    git(&repo, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git(&repo, &["push", "-q", "origin", "main"]);
    (repo, bare)
}

/// Create `branch` locally and push it.
fn publish(repo: &Path, branch: &str) {
    git(repo, &["branch", branch, "main"]);
    git(repo, &["push", "-q", "origin", branch]);
}

/// Ref names under `prefix`, without it.
fn refs(dir: &Path, prefix: &str) -> BTreeSet<String> {
    git(dir, &["for-each-ref", "--format=%(refname)", prefix])
        .lines()
        .map(|l| l.strip_prefix(prefix).unwrap().to_owned())
        .collect()
}

/// `kind:wh/task/<id>` of everything removed, sorted.
fn removed(r: &CleanupReport) -> Vec<String> {
    sorted(r.removed.iter().map(|x| format!("{}:{}", x.kind, x.what)).collect())
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn deletes_remote_task_branches(pool: PgPool) {
    let store = EventStore::new(pool);
    let dir = tempfile::tempdir().unwrap();
    let (repo, bare) = repo_with_remote(dir.path());

    let pr_merged = || TaskEvent::PrMerged { merged_at: None, merged_by: None };
    let pr_closed = || TaskEvent::PrClosed { closed_at: None };
    let cancel = || TaskEvent::TaskCancelled { reason: "no".into() };

    let merged = task(&store, Some(pr_merged())).await;
    let closed = task(&store, Some(pr_closed())).await;
    let cancelled = task(&store, Some(cancel())).await;
    // The poller saw the PR close after the task was cancelled.
    let cancelled_closed = task(&store, Some(cancel())).await;
    append(&store, &cancelled_closed.stream_id(), cancelled_closed, pr_closed()).await;
    let completed = task(&store, Some(TaskEvent::TaskCompleted { branch: None })).await;
    let blocked = task(&store, Some(TaskEvent::TaskBlocked { stage: None, reason: "x".into() })).await;
    let open = task(&store, None).await;
    let resumed = task(&store, Some(pr_closed())).await;
    let resume = TaskEvent::TaskResumed { stage: None, comment: None };
    append(&store, &resumed.stream_id(), resumed, resume).await;

    for t in [merged, closed, cancelled, cancelled_closed, completed, blocked, open, resumed] {
        publish(&repo, &task_branch(t));
    }
    publish(&repo, "wh/task/unknown");
    publish(&repo, "feature");

    let mut cfg = CleanupConfig {
        data_dir: dir.path().join("data"),
        repo: repo.clone(),
        remote: Some("origin".into()),
        grace: Duration::from_secs(3600),
        caches: vec![],
        dry_run: true,
        task_db: None,
    };
    let local = |t: TaskId| format!("branch:{}", task_branch(t));
    let remote = |t: TaskId| format!("remote-branch:{}", task_branch(t));
    let before = refs(&bare, "refs/heads/");
    let tracking = refs(&repo, "refs/remotes/origin/");

    // Dry run: lists the merged task's branches, changes nothing.
    let r = sweep(&store, &cfg).await.unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(removed(&r), sorted(vec![local(merged), remote(merged)]));
    assert_eq!(refs(&bare, "refs/heads/"), before);
    assert_eq!(refs(&repo, "refs/remotes/origin/"), tracking);

    // Within the grace period only the merged task's branches go.
    cfg.dry_run = false;
    let r = sweep(&store, &cfg).await.unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(removed(&r), sorted(vec![local(merged), remote(merged)]));
    let mut want = before.clone();
    want.remove(&task_branch(merged));
    assert_eq!(refs(&bare, "refs/heads/"), want);
    assert!(!refs(&repo, "refs/remotes/origin/").contains(&task_branch(merged)));

    // Grace over: closed and cancelled go too. The closed task keeps its local branch.
    cfg.grace = Duration::ZERO;
    let r = sweep(&store, &cfg).await.unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let gone = [remote(closed), local(cancelled), remote(cancelled), local(cancelled_closed), remote(cancelled_closed)];
    assert_eq!(removed(&r), sorted(gone.to_vec()));
    for t in [closed, cancelled, cancelled_closed] {
        want.remove(&task_branch(t));
    }
    assert_eq!(refs(&bare, "refs/heads/"), want);
    assert!(want.contains("wh/task/unknown") && want.contains("feature"));
    let kept = refs(&repo, "refs/heads/");
    for t in [closed, completed, blocked, open, resumed] {
        assert!(kept.contains(&task_branch(t)));
    }
    for t in [completed, blocked, open, resumed] {
        assert!(want.contains(&task_branch(t)));
    }

    // Idempotent.
    let r = sweep(&store, &cfg).await.unwrap();
    assert!(r.removed.is_empty() && r.errors.is_empty(), "{:?} {:?}", r.removed, r.errors);

    // GitHub deleted the head branch itself: no error, the stale tracking ref is pruned.
    let auto = task(&store, Some(pr_merged())).await;
    publish(&repo, &task_branch(auto));
    git(&bare, &["branch", "-D", &task_branch(auto)]);
    assert!(refs(&repo, "refs/remotes/origin/").contains(&task_branch(auto)));
    let r = sweep(&store, &cfg).await.unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(removed(&r), vec![local(auto)]);
    assert!(!refs(&repo, "refs/remotes/origin/").contains(&task_branch(auto)));

    // A rejected delete is reported and retried, although the local branch is gone by then.
    let refused = task(&store, Some(pr_merged())).await;
    publish(&repo, &task_branch(refused));
    git(&bare, &["config", "receive.denyDeletes", "true"]);
    let r = sweep(&store, &cfg).await.unwrap();
    assert_eq!(r.errors.len(), 1, "{:?}", r.errors);
    assert_eq!(removed(&r), vec![local(refused)]);
    git(&bare, &["config", "receive.denyDeletes", "false"]);
    let r = sweep(&store, &cfg).await.unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(removed(&r), vec![remote(refused)]);

    // Unreachable remote: reported, local cleanup still happens.
    let offline = task(&store, Some(pr_merged())).await;
    publish(&repo, &task_branch(offline));
    git(&repo, &["remote", "set-url", "origin", dir.path().join("nowhere").to_str().unwrap()]);
    let r = sweep(&store, &cfg).await.unwrap();
    assert_eq!(r.errors.len(), 1, "{:?}", r.errors);
    assert_eq!(removed(&r), vec![local(offline)]);
}
