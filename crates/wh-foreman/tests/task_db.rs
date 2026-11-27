//! Per-task databases: provisioned before a task's stages, dropped by `sweep`.
//! The "admin" server is the one the tests run on (`DATABASE_URL`); every test
//! uses fresh task ids and drops what it creates.

use std::{
    path::Path,
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};

use sqlx::{Connection, PgConnection, PgPool};
use tokio::sync::mpsc::UnboundedSender;
use wh_core::{
    Actor, EventMeta, NewEvent, TaskId,
    session::TokenUsage,
    task::{TaskEvent, TaskSource, TaskState, TaskStatus},
};
use wh_eventstore::{EventStore, ExpectedVersion};
use wh_factory::Factory;
use wh_foreman::{
    Dispatch, Foreman,
    cleanup::{CleanupConfig, TaskDbAdmin, sweep},
    task_db,
};
use wh_runner::{AgentRequest, AgentRuntime, Outcome, OutcomeStatus, Runner, RunnerConfig, Signal};

fn admin_url() -> String {
    std::env::var("DATABASE_URL").expect("DATABASE_URL")
}

/// `DATABASE_URL` and `WH_DATA_DIR` of a session.
type SessionEnv = (Option<String>, Option<String>);

/// Records the environment of each session.
#[derive(Default, Clone)]
struct Fake(Arc<Mutex<Vec<SessionEnv>>>);

impl AgentRuntime for Fake {
    fn name(&self) -> &str {
        "fake"
    }
    async fn run(&self, req: &AgentRequest, _tx: UnboundedSender<Signal>) -> Outcome {
        let url = req.env.get("DATABASE_URL").cloned();
        let first = self.0.lock().unwrap().is_empty();
        self.0.lock().unwrap().push((url.clone(), req.env.get("WH_DATA_DIR").cloned()));
        // Only a task database is ours to write to.
        if let Some(url) = url.filter(|u| u.contains("/wh_task_")) {
            let mut db = PgConnection::connect(&url).await.unwrap();
            if first {
                // Stands for a migration the task branch added.
                sqlx::query("CREATE TABLE marker (x int)").execute(&mut db).await.unwrap();
                sqlx::query(
                    "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
                     VALUES (999999, 'branch', true, ''::bytea, 0)",
                )
                .execute(&mut db)
                .await
                .unwrap();
            }
            db.close().await.unwrap();
        }
        Outcome {
            status: OutcomeStatus::Succeeded,
            usage: TokenUsage::default(),
            cost_usd: 0.0,
            num_turns: 1,
            duration_ms: 1,
            summary: Some("ok".into()),
        }
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn setup() -> (tempfile::TempDir, Factory) {
    setup_agent("{ prompt: a.md }")
}

fn setup_agent(agent: &str) -> (tempfile::TempDir, Factory) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hi").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"]);
    std::fs::write(dir.path().join("a.md"), "agent").unwrap();
    let yaml = format!(
        "
name: test
repo: repo
base_ref: main
task_database: {{ admin_url_env: WH_AGENT_DB_TEST_UNSET }}
stages: [{{ id: one, agent: a }}, {{ id: two, agent: a }}]
agents: {{ a: {agent} }}
"
    );
    std::fs::write(dir.path().join("factory.yaml"), yaml).unwrap();
    let factory = Factory::load(&dir.path().join("factory.yaml")).unwrap();
    (dir, factory)
}

async fn append(store: &EventStore, task: TaskId, ev: TaskEvent) {
    let meta = EventMeta::new(task.0, Actor::System { component: "test".into() });
    let events = vec![NewEvent::from_domain(&ev, meta).unwrap()];
    store.append(&task.stream_id(), ExpectedVersion::Any, events).await.unwrap();
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
    append(store, t, created).await;
    if let Some(ev) = last {
        append(store, t, ev).await;
    }
    t
}

fn foreman(store: &EventStore, dir: &Path, factory: Factory, fake: Fake) -> Foreman<Fake> {
    let runner = Runner::new(store.clone(), fake, RunnerConfig { data_dir: dir.join("data") });
    Foreman::new(store.clone(), factory, runner, Dispatch::Inline)
}

async fn exists(pool: &PgPool, name: &str) -> bool {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn drop_database(pool: &PgPool, name: &str) {
    sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)")).execute(pool).await.unwrap();
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn stages_of_a_task_share_a_database_and_tasks_do_not(pool: PgPool) {
    let store = EventStore::new(pool.clone());
    let (dir, factory) = setup();
    let fake = Fake::default();
    let mut foreman = foreman(&store, dir.path(), factory, fake.clone()).with_task_db_admin(admin_url());
    let (a, b) = (task(&store, None).await, task(&store, None).await);
    for t in [a, b] {
        let state = foreman.drive(t).await.unwrap();
        assert_eq!(state.status, TaskStatus::Done);
    }

    let envs = fake.0.lock().unwrap().clone();
    let urls: Vec<String> = envs.iter().map(|(u, _)| u.clone().expect("DATABASE_URL set")).collect();
    assert_eq!(urls.len(), 4);
    assert_eq!(urls[0], urls[1], "two stages of a task");
    assert_ne!(urls[1], urls[2], "two tasks");
    assert_eq!(urls[2], urls[3]);
    assert!(urls[0].contains(&task_db::db_name(a)));
    for ((_, data), t) in envs.iter().zip([a, a, b, b]) {
        let data = data.as_deref().expect("WH_DATA_DIR set");
        assert!(data.ends_with(&format!("wh-agent-{}", task_db::db_name(t))), "{data}");
    }

    let mut db = PgConnection::connect(&urls[0]).await.unwrap();
    // Migrated, and not again for the second stage (the marker and branch row survive).
    sqlx::query("SELECT 1 FROM events, marker").fetch_optional(&mut db).await.unwrap();
    db.close().await.unwrap();

    for t in [a, b] {
        drop_database(&pool, &task_db::db_name(t)).await;
    }
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn factory_env_wins_over_the_task_database(pool: PgPool) {
    let store = EventStore::new(pool.clone());
    let (dir, factory) =
        setup_agent("{ prompt: a.md, env: { DATABASE_URL: 'postgres://factory/db', WH_DATA_DIR: /factory/data } }");
    let fake = Fake::default();
    let mut foreman = foreman(&store, dir.path(), factory, fake.clone()).with_task_db_admin(admin_url());
    let t = task(&store, None).await;
    let state = foreman.drive(t).await.unwrap();
    drop_database(&pool, &task_db::db_name(t)).await;
    assert_eq!(state.status, TaskStatus::Done);

    let from_factory = (Some("postgres://factory/db".to_string()), Some("/factory/data".to_string()));
    assert_eq!(*fake.0.lock().unwrap(), [from_factory.clone(), from_factory]);
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn a_task_is_blocked_without_a_session_when_provisioning_fails(pool: PgPool) {
    let store = EventStore::new(pool);
    let (dir, factory) = setup();
    let fake = Fake::default();
    // The factory's variable is not set.
    let mut foreman = foreman(&store, dir.path(), factory, fake.clone());
    let t = task(&store, None).await;
    let state: TaskState = foreman.drive(t).await.unwrap();
    let TaskStatus::Blocked { reason } = state.status else { panic!("{:?}", state.status) };
    assert!(reason.contains("task database: WH_AGENT_DB_TEST_UNSET is not set"), "{reason}");
    assert!(fake.0.lock().unwrap().is_empty());
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn sweep_drops_only_the_task_databases_that_are_due(pool: PgPool) {
    let store = EventStore::new(pool.clone());
    let dir = tempfile::tempdir().unwrap();
    let url = admin_url();

    let done = task(&store, Some(TaskEvent::TaskCompleted { branch: None })).await;
    let merged = task(&store, Some(TaskEvent::PrMerged { merged_by: None, merged_at: None })).await;
    let cancelled = task(&store, Some(TaskEvent::TaskCancelled { reason: "x".into() })).await;
    let blocked = task(&store, Some(TaskEvent::TaskBlocked { stage: None, reason: "x".into() })).await;
    let open = task(&store, None).await;
    let pr = TaskEvent::TaskShipped {
        repo: "o/r".into(),
        pr_number: 1,
        pr_url: "u".into(),
        branch: "b".into(),
        commit: "c".into(),
        created: true,
    };
    let shipped = task(&store, Some(pr)).await;
    // The poller saw the PR close after the task was cancelled.
    let cancelled_closed = task(&store, Some(TaskEvent::TaskCancelled { reason: "x".into() })).await;
    append(&store, cancelled_closed, TaskEvent::PrClosed { closed_at: None }).await;
    let orphan = TaskId::new();
    let all = [done, merged, cancelled, cancelled_closed, blocked, open, shipped, orphan];
    for t in all {
        task_db::provision(&store, &url, t).await.unwrap();
    }
    // Not ours to drop: no comment, another store's comment, other names.
    let unmarked = task_db::db_name(TaskId::new());
    let foreign = task_db::db_name(TaskId::new());
    let near_miss = format!("{}x", task_db::db_name(TaskId::new()));
    let other = task_db::db_name(TaskId::new()).replace("wh_task_", "wh_other_");
    let mine = format!("workhub:{} ready", store.instance_id().await.unwrap());
    for (name, comment) in [
        (&unmarked, None),
        (&foreign, Some("workhub:00000000-0000-0000-0000-000000000000 ready")),
        (&near_miss, Some(mine.as_str())),
        (&other, Some(mine.as_str())),
    ] {
        sqlx::query(&format!("CREATE DATABASE {name}")).execute(&pool).await.unwrap();
        if let Some(c) = comment {
            sqlx::query(&format!("COMMENT ON DATABASE {name} IS '{c}'")).execute(&pool).await.unwrap();
        }
    }

    let mut cfg = CleanupConfig {
        data_dir: dir.path().join("data"),
        repo: dir.path().to_owned(),
        remote: None,
        grace: Duration::from_secs(3600),
        caches: vec![],
        dry_run: true,
        task_db: Some(TaskDbAdmin { env: "WH_AGENT_DB_ADMIN_URL".into(), url: Some(url.clone()) }),
    };
    let dropped = |r: &wh_foreman::cleanup::CleanupReport| {
        let mut v: Vec<String> =
            r.removed.iter().filter(|x| x.kind == "database").map(|x| x.what.clone()).collect();
        v.sort();
        v
    };
    let names = |tasks: &[TaskId]| {
        let mut v: Vec<String> = tasks.iter().map(|t| task_db::db_name(*t)).collect();
        v.sort();
        v
    };

    let dry = sweep(&store, &cfg).await.unwrap();
    assert_eq!(dropped(&dry), names(&[done, merged, orphan]));
    for t in all {
        assert!(exists(&pool, &task_db::db_name(t)).await, "a dry run drops nothing");
    }

    cfg.dry_run = false;
    let r = sweep(&store, &cfg).await.unwrap();
    assert_eq!(dropped(&r), names(&[done, merged, orphan]));
    for t in [done, merged, orphan] {
        assert!(!exists(&pool, &task_db::db_name(t)).await);
    }
    for t in [cancelled, cancelled_closed, blocked, open, shipped] {
        assert!(exists(&pool, &task_db::db_name(t)).await);
    }

    cfg.grace = Duration::ZERO;
    let r = sweep(&store, &cfg).await.unwrap();
    assert_eq!(dropped(&r), names(&[cancelled, cancelled_closed]));

    for name in [&unmarked, &foreign, &near_miss, &other] {
        assert!(exists(&pool, name).await, "{name} must be left alone");
        drop_database(&pool, name).await;
    }
    for t in [blocked, open, shipped] {
        drop_database(&pool, &task_db::db_name(t)).await;
    }

    cfg.task_db = Some(TaskDbAdmin { env: "WH_AGENT_DB_ADMIN_URL".into(), url: None });
    let r = sweep(&store, &cfg).await.unwrap();
    assert!(r.errors.iter().any(|e| e.contains("WH_AGENT_DB_ADMIN_URL is not set")), "{:?}", r.errors);
}
