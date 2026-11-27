//! `cleanup::sweep`: what gets removed, what is kept, and when.

use std::{path::Path, process::Command, time::Duration};

use sqlx::PgPool;
use wh_core::{
    Actor, DomainEvent, EventMeta, NewEvent, SessionId, TaskId,
    session::SessionEvent,
    task::{TaskEvent, TaskSource, task_branch},
};
use wh_eventstore::{EventStore, ExpectedVersion};
use wh_foreman::cleanup::{CleanupConfig, sweep};

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
