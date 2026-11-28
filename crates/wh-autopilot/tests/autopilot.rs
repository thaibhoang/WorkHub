use std::{collections::VecDeque, path::Path, process::Command, sync::Mutex};

use sqlx::PgPool;
use tokio::sync::mpsc::UnboundedSender;
use wh_autopilot::{AutopilotService, Dispatch, Handled};
use wh_core::{
    Actor, AutopilotId, EventMeta, FailureClass, NewEvent, Question, SessionId, TaskId,
    autopilot::AutopilotEvent,
    session::TokenUsage,
    task::{AUTOPILOT_REVIEWER, StageOutcome, TaskEvent, TaskSource, task_branch},
};
use wh_eventstore::{EventStore, ExpectedVersion, Subscription};
use wh_factory::Factory;
use wh_runner::{AgentRequest, AgentRuntime, Outcome, OutcomeStatus, Signal};

/// Autopilot model double: answers with the queued replies (the last one repeats).
struct Model {
    replies: Mutex<VecDeque<String>>,
    requests: Mutex<Vec<AgentRequest>>,
}

impl Model {
    fn new(replies: &[&str]) -> &'static Self {
        Box::leak(Box::new(Self {
            replies: Mutex::new(replies.iter().map(|r| r.to_string()).collect()),
            requests: Mutex::new(Vec::new()),
        }))
    }
    fn calls(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
    fn last_prompt(&self) -> String {
        self.requests.lock().unwrap().last().unwrap().prompt.clone()
    }
}

impl AgentRuntime for &'static Model {
    fn name(&self) -> &str {
        "model"
    }
    async fn run(&self, req: &AgentRequest, _tx: UnboundedSender<Signal>) -> Outcome {
        self.requests.lock().unwrap().push(req.clone());
        let mut q = self.replies.lock().unwrap();
        let reply = if q.len() > 1 { q.pop_front().unwrap() } else { q.front().cloned().unwrap_or_default() };
        if reply == "CRASH" {
            return Outcome {
                status: OutcomeStatus::Failed { class: FailureClass::Timeout, message: "timed out".into(), retryable: true },
                usage: TokenUsage::default(),
                cost_usd: 0.01,
                num_turns: 1,
                duration_ms: 5,
                summary: None,
            };
        }
        Outcome {
            status: OutcomeStatus::Succeeded,
            usage: TokenUsage { input_tokens: 100, output_tokens: 10, ..Default::default() },
            cost_usd: 0.02,
            num_turns: 1,
            duration_ms: 5,
            summary: Some(reply),
        }
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn setup() -> (tempfile::TempDir, Factory) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hi").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"]);

    let f = dir.path().join("factory");
    std::fs::create_dir_all(f.join("agents")).unwrap();
    std::fs::write(f.join("agents/a.md"), "ROLE").unwrap();
    std::fs::write(f.join("autopilot.md"), "POLICY: approve small specs").unwrap();
    std::fs::write(
        f.join("factory.yaml"),
        r#"
name: test
repo: ../repo
base_ref: main
stages:
  - { id: spec, agent: a, gate: spec-review }
  - { id: implement, agent: a }
agents:
  a: { prompt: agents/a.md }
autopilot: { prompt: autopilot.md, model: claude-haiku-5-5, budget_usd: 0.1 }
"#,
    )
    .unwrap();
    (dir, Factory::load(&f.join("factory.yaml")).unwrap())
}

async fn append(store: &EventStore, task: TaskId, ev: TaskEvent) {
    let meta = EventMeta::new(task.0, Actor::System { component: "test".into() });
    store.append(&task.stream_id(), ExpectedVersion::Any, vec![NewEvent::from_domain(&ev, meta).unwrap()]).await.unwrap();
}

/// A task of `factory` whose spec stage finished, created in autopilot mode or not.
async fn task_at_stage(store: &EventStore, factory: Option<&str>, autopilot: bool) -> TaskId {
    let task = TaskId::new();
    let created = TaskEvent::TaskCreated {
        title: "Add login".into(),
        body: Some("Users sign in with email".into()),
        source: TaskSource::Cli,
        factory: factory.map(Into::into),
        benchmark: None,
        external_ref: None,
        parent: None,
        draft: true,
        autopilot: false,
    };
    append(store, task, created).await;
    append(store, task, TaskEvent::TaskActivated { autopilot }).await;
    append(store, task, TaskEvent::StageEntered { stage: "spec".into(), feedback: None }).await;
    let summary = Some("SPEC: a login form".into());
    append(store, task, TaskEvent::StageExited { stage: "spec".into(), outcome: StageOutcome::Succeeded, summary, session: None }).await;
    task
}

async fn task_events(store: &EventStore, task: TaskId) -> Vec<TaskEvent> {
    store.read_stream(&task.stream_id()).await.unwrap().iter().map(|e| e.decode().unwrap()).collect()
}

async fn autopilot_types(store: &EventStore, task: TaskId, version: i64) -> Vec<String> {
    let s = AutopilotId::for_request(task, version).stream_id();
    store.read_stream(&s).await.unwrap().into_iter().map(|e| e.event_type).collect()
}

fn service(store: &EventStore, f: &Factory, model: &'static Model) -> AutopilotService<&'static Model> {
    AutopilotService::new(store.clone(), f.clone(), model, std::env::temp_dir().join("wh-autopilot-test"), Dispatch::Inline)
}

const APPROVE: &str = "Small and clear.\n{\"decision\": \"approve\", \"comment\": \"looks good\", \"rationale\": \"matches the task\"}";

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn approves_the_gate_of_autopilot_tasks_once(pool: PgPool) {
    let (dir, f) = setup();
    let store = EventStore::new(pool);
    let model = Model::new(&[APPROVE]);
    let task = task_at_stage(&store, Some("test"), true).await;
    let manual = task_at_stage(&store, Some("test"), false).await;
    let other = task_at_stage(&store, Some("other-factory"), true).await;
    // The spec stage committed on the task branch: the autopilot sees the diff.
    let repo = dir.path().join("repo");
    git(&repo, &["checkout", "-qb", &task_branch(task)]);
    std::fs::write(repo.join("SPEC.md"), "login form spec").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "spec"]);
    git(&repo, &["checkout", "-q", "main"]);
    for t in [task, manual, other] {
        append(&store, t, TaskEvent::GateRequested { gate: "spec-review".into(), stage: "spec".into() }).await;
    }

    let mut svc = service(&store, &f, model);
    let sub = Subscription::new(store.clone());
    sub.catch_up(&mut svc).await.unwrap();
    assert_eq!(model.calls(), 1, "only the autopilot task of this factory");
    let prompt = model.last_prompt();
    for needle in ["Add login", "Users sign in", "SPEC: a login form", "+login form spec", "POLICY: approve small specs", "spec-review"] {
        assert!(prompt.contains(needle), "prompt lacks {needle:?}:\n{prompt}");
    }
    let req = model.requests.lock().unwrap()[0].clone();
    assert_eq!((req.tools, req.model.as_deref(), req.budget_usd), (Some(vec![]), Some("claude-haiku-5-5"), Some(0.1)));

    let last = task_events(&store, task).await.pop().unwrap();
    assert_eq!(
        last,
        TaskEvent::GateDecided {
            gate: "spec-review".into(),
            stage: "spec".into(),
            approved: true,
            reviewer: AUTOPILOT_REVIEWER.into(),
            comment: Some("looks good".into()),
        }
    );
    assert!(matches!(task_events(&store, manual).await.pop(), Some(TaskEvent::GateRequested { .. })), "manual task waits for a human");
    assert_eq!(autopilot_types(&store, task, 5).await, ["AutopilotStarted", "AutopilotDecided"]);
    let decided = store.read_stream(&AutopilotId::for_request(task, 5).stream_id()).await.unwrap()[1].decode::<AutopilotEvent>().unwrap();
    assert!(matches!(decided, AutopilotEvent::AutopilotDecided { ref decision, cost_usd, .. } if decision == "approve" && cost_usd == 0.02));

    // Redelivery decides nothing again.
    store.reset_checkpoint(svc.consumer_name()).await.unwrap();
    sub.catch_up(&mut svc).await.unwrap();
    assert_eq!(model.calls(), 1);
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn rejects_with_a_comment_and_leaves_failures_to_a_human(pool: PgPool) {
    let (_dir, f) = setup();
    let store = EventStore::new(pool);
    let model = Model::new(&["{\"decision\": \"reject\", \"comment\": \"say which page\"}", "I am not sure.", "CRASH"]);
    let svc = service(&store, &f, model);

    let a = task_at_stage(&store, Some("test"), true).await;
    append(&store, a, TaskEvent::GateRequested { gate: "spec-review".into(), stage: "spec".into() }).await;
    assert!(matches!(svc.decide_now(a).await.unwrap(), Handled::Decided { ref decision, .. } if decision == "reject"));
    assert!(matches!(
        task_events(&store, a).await.pop(),
        Some(TaskEvent::GateDecided { approved: false, comment: Some(c), .. }) if c == "say which page"
    ));

    for t in [task_at_stage(&store, Some("test"), true).await, task_at_stage(&store, Some("test"), true).await] {
        append(&store, t, TaskEvent::GateRequested { gate: "spec-review".into(), stage: "spec".into() }).await;
        assert!(matches!(svc.decide_now(t).await.unwrap(), Handled::Failed { .. }));
        assert!(matches!(task_events(&store, t).await.pop(), Some(TaskEvent::GateRequested { .. })), "the gate stays for a human");
        assert_eq!(autopilot_types(&store, t, 5).await, ["AutopilotStarted", "AutopilotFailed"]);
    }
    assert_eq!(model.calls(), 3);
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn answers_agent_questions(pool: PgPool) {
    let (_dir, f) = setup();
    let store = EventStore::new(pool);
    let model = Model::new(&["{\"answers\": [{\"option\": \"Email\"}, {\"text\": \"/login\"}], \"rationale\": \"task says email\"}"]);
    let task = task_at_stage(&store, Some("test"), true).await;
    append(&store, task, TaskEvent::StageEntered { stage: "implement".into(), feedback: None }).await;
    let questions = vec![
        Question { text: "Sign in with?".into(), options: vec!["Email".into(), "GitHub".into()] },
        Question { text: "Which route?".into(), options: vec![] },
    ];
    let asked = TaskEvent::AgentAsked { stage: "implement".into(), session: SessionId::new(), questions, resume: None };
    append(&store, task, asked).await;

    let mut svc = service(&store, &f, model);
    Subscription::new(store.clone()).catch_up(&mut svc).await.unwrap();
    assert!(model.last_prompt().contains("1. Sign in with?"));
    let Some(TaskEvent::UserAnswered { stage, answers, user }) = task_events(&store, task).await.pop() else {
        panic!("not answered")
    };
    assert_eq!((stage.as_str(), user.as_str()), ("implement", AUTOPILOT_REVIEWER));
    assert_eq!(answers[0].option.as_deref(), Some("Email"));
    assert_eq!(answers[1].text.as_deref(), Some("/login"));
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn decides_after_unrelated_events_and_recovers_at_startup(pool: PgPool) {
    let (_dir, f) = setup();
    let store = EventStore::new(pool);
    let model = Model::new(&[APPROVE]);
    let svc = service(&store, &f, model);

    // An event after the request (a move) does not hide it.
    let moved = task_at_stage(&store, Some("test"), true).await;
    append(&store, moved, TaskEvent::GateRequested { gate: "spec-review".into(), stage: "spec".into() }).await;
    append(&store, moved, TaskEvent::TaskMoved { parent: None }).await;

    // A claim left by a dead process (no result) is taken over.
    let claimed = task_at_stage(&store, Some("test"), true).await;
    append(&store, claimed, TaskEvent::GateRequested { gate: "spec-review".into(), stage: "spec".into() }).await;
    let started = AutopilotEvent::AutopilotStarted {
        task_id: claimed,
        kind: wh_core::autopilot::AutopilotKind::Gate,
        stage: "spec".into(),
        gate: Some("spec-review".into()),
        request_version: 5,
        model: None,
    };
    let meta = EventMeta::new(claimed.0, Actor::System { component: "autopilot".into() });
    let s = AutopilotId::for_request(claimed, 5).stream_id();
    store.append(&s, ExpectedVersion::NoStream, vec![NewEvent::from_domain(&started, meta.clone()).unwrap()]).await.unwrap();

    // A claim for a request that is no longer pending is closed.
    let gone = task_at_stage(&store, Some("test"), true).await;
    append(&store, gone, TaskEvent::GateRequested { gate: "spec-review".into(), stage: "spec".into() }).await;
    append(&store, gone, TaskEvent::TaskCancelled { reason: "x".into() }).await;
    let started_gone = AutopilotEvent::AutopilotStarted {
        task_id: gone,
        kind: wh_core::autopilot::AutopilotKind::Gate,
        stage: "spec".into(),
        gate: Some("spec-review".into()),
        request_version: 5,
        model: None,
    };
    let s = AutopilotId::for_request(gone, 5).stream_id();
    store.append(&s, ExpectedVersion::NoStream, vec![NewEvent::from_domain(&started_gone, meta).unwrap()]).await.unwrap();

    svc.start_from_head_if_new().await.unwrap();
    let handled = svc.recover().await.unwrap();
    assert_eq!(model.calls(), 2, "moved + claimed; not the cancelled task");
    assert_eq!(handled.len(), 3);
    for t in [moved, claimed] {
        assert!(matches!(task_events(&store, t).await.pop(), Some(TaskEvent::GateDecided { approved: true, .. })));
        assert_eq!(autopilot_types(&store, t, 5).await, ["AutopilotStarted", "AutopilotDecided"]);
    }
    assert_eq!(autopilot_types(&store, gone, 5).await, ["AutopilotStarted", "AutopilotFailed"]);

    // A second start finds nothing left to do.
    assert!(svc.recover().await.unwrap().is_empty());
    assert_eq!(model.calls(), 2);
}
