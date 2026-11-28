use std::{
    path::Path,
    process::Command,
    sync::atomic::{AtomicU32, Ordering},
};

use sqlx::PgPool;
use tokio::sync::mpsc::UnboundedSender;
use wh_core::{
    Actor, EventMeta, FailureClass, NewEvent, TaskId,
    session::TokenUsage,
    task::{TaskEvent, TaskSource, TaskState, TaskStatus},
};
use wh_eventstore::{EventStore, ExpectedVersion, Subscription};
use wh_factory::Factory;
use wh_foreman::{Dispatch, Foreman};
use wh_metrics::tasks::TasksProjection;
use wh_runner::{AgentRequest, AgentRuntime, Outcome, OutcomeStatus, Runner, RunnerConfig, Signal};

/// Runtime double keyed on the role line of each agent prompt.
struct Fake {
    review_calls: AtomicU32,
    /// Review rejects this many times before approving.
    review_rejects: u32,
    implement_fails: bool,
}

fn ok(summary: &str) -> Outcome {
    Outcome {
        status: OutcomeStatus::Succeeded,
        usage: TokenUsage::default(),
        cost_usd: 0.01,
        num_turns: 1,
        duration_ms: 10,
        summary: Some(summary.into()),
    }
}

impl AgentRuntime for Fake {
    fn name(&self) -> &str {
        "fake"
    }
    async fn run(&self, req: &AgentRequest, _tx: UnboundedSender<Signal>) -> Outcome {
        if req.prompt.starts_with("ROLE: implement") {
            if self.implement_fails {
                return Outcome::failed(FailureClass::BudgetExceeded, "out of budget", false, Default::default());
            }
            let f = req.workdir.join("impl.txt");
            let mut text = std::fs::read_to_string(&f).unwrap_or_default();
            text.push_str(if req.prompt.contains("Feedback you must address") { "fixed\n" } else { "v1\n" });
            std::fs::write(f, text).unwrap();
            ok("implemented")
        } else if req.prompt.starts_with("ROLE: review") {
            assert!(req.workdir.join("impl.txt").exists(), "review must see implementation commits");
            let n = self.review_calls.fetch_add(1, Ordering::SeqCst);
            if n < self.review_rejects { ok("missing tests\nVERDICT: REJECT") } else { ok("good\nVERDICT: APPROVE") }
        } else {
            ok("spec written")
        }
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Temp dir with a git repo and a factory: spec (gate) → implement → review (on_reject: implement).
fn setup(max_rework: u32) -> (tempfile::TempDir, Factory) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hi").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"]);

    let fdir = dir.path().join("factory");
    std::fs::create_dir_all(fdir.join("agents")).unwrap();
    for role in ["spec", "implement", "review"] {
        std::fs::write(fdir.join(format!("agents/{role}.md")), format!("ROLE: {role}")).unwrap();
    }
    std::fs::write(
        fdir.join("factory.yaml"),
        format!(
            r#"
name: test
repo: ../repo
base_ref: main
max_rework_loops: {max_rework}
defaults: {{ max_retries: 0 }}
stages:
  - {{ id: spec, agent: spec, gate: spec-review }}
  - {{ id: implement, agent: implement }}
  - {{ id: review, agent: review, on_reject: implement }}
agents:
  spec: {{ prompt: agents/spec.md }}
  implement: {{ prompt: agents/implement.md }}
  review: {{ prompt: agents/review.md }}
"#
        ),
    )
    .unwrap();
    let f = Factory::load(&fdir.join("factory.yaml")).unwrap();
    (dir, f)
}

async fn append(store: &EventStore, task: TaskId, ev: TaskEvent) {
    let state = load(store, task).await;
    let meta = EventMeta::new(task.0, Actor::Human { user: "tester".into() });
    let expected = if state.version == 0 { ExpectedVersion::NoStream } else { ExpectedVersion::Exact(state.version) };
    store.append(&task.stream_id(), expected, vec![NewEvent::from_domain(&ev, meta).unwrap()]).await.unwrap();
}

async fn load(store: &EventStore, task: TaskId) -> TaskState {
    let mut s = TaskState::default();
    for e in store.read_stream(&task.stream_id()).await.unwrap() {
        s.apply(e.stream_version, e.decode().unwrap());
    }
    s
}

fn types(events: &[wh_core::RecordedEvent]) -> Vec<&str> {
    events.iter().map(|e| e.event_type.as_str()).collect()
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn full_pipeline_with_gate_and_review_rework(pool: PgPool) {
    let (dir, factory) = setup(3);
    let repo = factory.repo.clone();
    let version = factory.version.clone();
    let store = EventStore::new(pool.clone());
    let fake = Fake { review_calls: AtomicU32::new(0), review_rejects: 1, implement_fails: false };
    let runner = Runner::new(store.clone(), fake, RunnerConfig { data_dir: dir.path().join("data") });
    let mut foreman = Foreman::new(store.clone(), factory, runner, Dispatch::Inline);
    let sub = Subscription::new(store.clone());

    let task = TaskId::new();
    let created = TaskEvent::TaskCreated {
        title: "Add feature".into(),
        body: None,
        source: TaskSource::Cli,
        factory: Some("test".into()),
        benchmark: None,
        external_ref: None,
    };
    append(&store, task, created).await;
    sub.catch_up(&mut foreman).await.unwrap();
    let state = load(&store, task).await;
    assert_eq!(state.status, TaskStatus::AwaitingGate { gate: "spec-review".into(), stage: "spec".into() });

    // Redelivery must not dispatch anything twice.
    store.reset_checkpoint(foreman_name()).await.unwrap();
    sub.catch_up(&mut foreman).await.unwrap();
    assert_eq!(load(&store, task).await.version, state.version);

    append(&store, task, TaskEvent::GateDecided {
        gate: "spec-review".into(),
        stage: "spec".into(),
        approved: true,
        reviewer: "tester".into(),
        comment: None,
    })
    .await;
    sub.catch_up(&mut foreman).await.unwrap();

    let events = store.read_stream(&task.stream_id()).await.unwrap();
    assert_eq!(types(&events), [
        "TaskCreated",
        "StageEntered", "StageExited", // spec
        "GateRequested", "GateDecided",
        "StageEntered", "StageExited", // implement
        "StageEntered", "StageExited", // review → REJECT
        "StageEntered", "StageExited", // implement (rework)
        "StageEntered", "StageExited", // review → APPROVE
        "TaskCompleted",
    ]);
    let state = load(&store, task).await;
    assert_eq!((state.status, state.rework_loops), (TaskStatus::Done, 1));
    match &events[9].decode::<TaskEvent>().unwrap() {
        TaskEvent::StageEntered { stage, feedback } => {
            assert_eq!(stage, "implement");
            assert!(feedback.as_deref().unwrap().contains("missing tests"));
        }
        other => panic!("unexpected {other:?}"),
    }
    // Runner and foreman events carry the factory version.
    assert!(events[1..].iter().filter(|e| e.event_type != "GateDecided").all(|e| e.meta.factory_version.as_deref() == Some(version.as_str())));

    // Both implement runs were committed on the shared task branch.
    let branch = Foreman::<Fake>::task_branch(task);
    assert_eq!(git(&repo, &["rev-list", "--count", &format!("main..{branch}")]), "2");
    assert_eq!(git(&repo, &["show", &format!("{branch}:impl.txt")]), "v1\nfixed");

    wh_metrics::catch_up_all(&store).await.unwrap();
    let row = TasksProjection::list(&pool).await.unwrap().remove(0);
    assert_eq!((row.status.as_str(), row.rework_loops), ("done", 1));
}

fn foreman_name() -> &'static str {
    "foreman:test"
}

async fn run_to_gate_and_approve(store: &EventStore, foreman: &mut Foreman<Fake>, task: TaskId) {
    let sub = Subscription::new(store.clone());
    let created =
        TaskEvent::TaskCreated { title: "t".into(), body: None, source: TaskSource::Cli, factory: Some("test".into()), benchmark: None, external_ref: None };
    append(store, task, created).await;
    sub.catch_up(foreman).await.unwrap();
    append(store, task, TaskEvent::GateDecided {
        gate: "spec-review".into(),
        stage: "spec".into(),
        approved: true,
        reviewer: "tester".into(),
        comment: None,
    })
    .await;
    sub.catch_up(foreman).await.unwrap();
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn rework_limit_blocks_task_and_resume_restarts_it(pool: PgPool) {
    let (dir, factory) = setup(1);
    let store = EventStore::new(pool.clone());
    let fake = Fake { review_calls: AtomicU32::new(0), review_rejects: 2, implement_fails: false };
    let runner = Runner::new(store.clone(), fake, RunnerConfig { data_dir: dir.path().join("data") });
    let mut foreman = Foreman::new(store.clone(), factory, runner, Dispatch::Inline);
    let task = TaskId::new();
    run_to_gate_and_approve(&store, &mut foreman, task).await;

    let state = load(&store, task).await;
    assert!(matches!(&state.status, TaskStatus::Blocked { reason } if reason.contains("rework limit")), "{:?}", state.status);
    assert_eq!(state.rework_loops, 1);

    // A human resumes at review with guidance; review now approves (3rd call).
    append(&store, task, TaskEvent::TaskResumed { stage: Some("review".into()), comment: None }).await;
    Subscription::new(store.clone()).catch_up(&mut foreman).await.unwrap();
    assert_eq!(load(&store, task).await.status, TaskStatus::Done);
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn failed_stage_blocks_and_gate_reject_reruns_stage(pool: PgPool) {
    let (dir, factory) = setup(3);
    let store = EventStore::new(pool.clone());
    let fake = Fake { review_calls: AtomicU32::new(0), review_rejects: 0, implement_fails: true };
    let runner = Runner::new(store.clone(), fake, RunnerConfig { data_dir: dir.path().join("data") });
    let mut foreman = Foreman::new(store.clone(), factory, runner, Dispatch::Inline);
    let sub = Subscription::new(store.clone());

    // Gate rejection sends spec back with the reviewer's comment as feedback.
    let task = TaskId::new();
    let created =
        TaskEvent::TaskCreated { title: "t".into(), body: None, source: TaskSource::Cli, factory: Some("test".into()), benchmark: None, external_ref: None };
    append(&store, task, created).await;
    sub.catch_up(&mut foreman).await.unwrap();
    append(&store, task, TaskEvent::GateDecided {
        gate: "spec-review".into(),
        stage: "spec".into(),
        approved: false,
        reviewer: "tester".into(),
        comment: Some("cover error cases".into()),
    })
    .await;
    sub.catch_up(&mut foreman).await.unwrap();
    let state = load(&store, task).await;
    assert!(matches!(state.status, TaskStatus::AwaitingGate { .. }));
    assert_eq!(state.rework_loops, 1);

    // Approve; implement fails (non-retryable) → blocked with the failure reason.
    append(&store, task, TaskEvent::GateDecided {
        gate: "spec-review".into(),
        stage: "spec".into(),
        approved: true,
        reviewer: "tester".into(),
        comment: None,
    })
    .await;
    sub.catch_up(&mut foreman).await.unwrap();
    let state = load(&store, task).await;
    assert!(matches!(&state.status, TaskStatus::Blocked { reason } if reason.contains("budget_exceeded")), "{:?}", state.status);

    // Tasks without this factory are ignored.
    let manual = TaskId::new();
    append(&store, manual, TaskEvent::TaskCreated { title: "m".into(), body: None, source: TaskSource::Cli, factory: None, benchmark: None, external_ref: None }).await;
    sub.catch_up(&mut foreman).await.unwrap();
    assert_eq!(load(&store, manual).await.version, 1);
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn interrupted_stage_is_blocked_on_startup_and_resumable(pool: PgPool) {
    let (dir, factory) = setup(3);
    let store = EventStore::new(pool.clone());
    let task = TaskId::new();
    let created =
        TaskEvent::TaskCreated { title: "t".into(), body: None, source: TaskSource::Cli, factory: Some("test".into()), benchmark: None, external_ref: None };
    append(&store, task, created).await;
    // Simulate a foreman that died right after dispatching: StageEntered + dangling session.
    append(&store, task, TaskEvent::StageEntered { stage: "spec".into(), feedback: None }).await;
    let session = wh_core::SessionId::new();
    let dispatched = wh_core::session::SessionEvent::AgentDispatched {
        task_id: task,
        agent: "spec".into(),
        stage: "spec".into(),
        runtime: "fake".into(),
        model: None,
        prompt: "p".into(),
        budget_usd: None,
        timeout_secs: 60,
        attempt: 1,
        retry_of: None,
        resume_of: None,
        benchmark: None,
    };
    let meta = EventMeta::new(task.0, Actor::System { component: "runner".into() });
    store
        .append(&session.stream_id(), ExpectedVersion::NoStream, vec![NewEvent::from_domain(&dispatched, meta).unwrap()])
        .await
        .unwrap();

    let fake = Fake { review_calls: AtomicU32::new(0), review_rejects: 0, implement_fails: false };
    let runner = Runner::new(store.clone(), fake, RunnerConfig { data_dir: dir.path().join("data") });
    let mut foreman = Foreman::new(store.clone(), factory, runner, Dispatch::Inline);
    assert_eq!(foreman.recover_interrupted().await.unwrap(), vec![task]);
    assert!(foreman.recover_interrupted().await.unwrap().is_empty(), "recovery is idempotent");

    let state = load(&store, task).await;
    assert!(matches!(&state.status, TaskStatus::Blocked { reason } if reason.contains("interrupted")));
    let last = store.read_stream(&session.stream_id()).await.unwrap().pop().unwrap();
    assert_eq!(last.event_type, "AgentSessionFailed");

    // Resume without naming a stage restarts the interrupted one (spec → gate).
    append(&store, task, TaskEvent::TaskResumed { stage: None, comment: None }).await;
    Subscription::new(store.clone()).catch_up(&mut foreman).await.unwrap();
    assert_eq!(
        load(&store, task).await.status,
        TaskStatus::AwaitingGate { gate: "spec-review".into(), stage: "spec".into() }
    );
}
