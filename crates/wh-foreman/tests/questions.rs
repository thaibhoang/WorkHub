//! The ask → wait → answer → resume loop, with a fake runtime that asks.

use std::{
    path::Path,
    process::Command,
    sync::{Arc, Mutex},
};

use sqlx::PgPool;
use tokio::sync::mpsc::UnboundedSender;
use wh_autopilot::{AutopilotService, Handled};
use wh_core::{
    AUTO_ANSWER_USER, Actor, Answer, BenchmarkRunId, EventMeta, NewEvent, RecordedEvent, TaskId,
    benchmark::BenchmarkContext,
    session::TokenUsage,
    task::{TaskEvent, TaskSource, TaskState, TaskStatus},
};
use wh_eventstore::{EventHandler, EventStore, ExpectedVersion};
use wh_factory::Factory;
use wh_foreman::{Dispatch, Foreman};
use wh_runner::{AgentRequest, AgentRuntime, Outcome, OutcomeStatus, Runner, RunnerConfig, SessionMode, Signal};

/// Asks on the first `asks` implement turns, then finishes.
#[derive(Clone)]
struct Fake {
    asks: usize,
    /// Prompt of every implement turn, with whether it resumed a conversation.
    turns: Arc<Mutex<Vec<(String, bool)>>>,
    /// Cancelled during the next implement turn, before it ends.
    cancel: Arc<Mutex<Option<(EventStore, TaskId)>>>,
}

impl Fake {
    fn new(asks: usize) -> Self {
        Self { asks, turns: Default::default(), cancel: Default::default() }
    }

    fn turns(&self) -> Vec<(String, bool)> {
        self.turns.lock().unwrap().clone()
    }
}

impl AgentRuntime for Fake {
    fn name(&self) -> &str {
        "fake"
    }
    async fn run(&self, req: &AgentRequest, tx: UnboundedSender<Signal>) -> Outcome {
        tx.send(Signal::Ready { runtime_session_id: Some("conversation".into()), model: None }).unwrap();
        let resumed = matches!(req.session, SessionMode::Resume(_));
        // The autopilot's own turn (no tools): it answers the pending question.
        let summary = if req.tools.as_ref().is_some_and(Vec::is_empty) {
            r#"{"answers": [{"option": "B"}], "rationale": "simpler"}"#
        } else if resumed || req.prompt.starts_with("ROLE: implement") {
            let n = {
                let mut turns = self.turns.lock().unwrap();
                turns.push((req.prompt.clone(), resumed));
                turns.len()
            };
            std::fs::write(req.workdir.join("impl.txt"), format!("turn {n}\n")).unwrap();
            let cancel = self.cancel.lock().unwrap().take();
            if let Some((store, task)) = cancel {
                append(&store, task, TaskEvent::TaskCancelled { reason: "stop".into() }).await;
            }
            if n <= self.asks { "QUESTION: {\"text\": \"Which?\", \"options\": [\"A\", \"B\"]}" } else { "implemented" }
        } else {
            "reviewed"
        };
        Outcome {
            status: OutcomeStatus::Succeeded,
            usage: TokenUsage::default(),
            cost_usd: 0.01,
            num_turns: 1,
            duration_ms: 10,
            summary: Some(summary.into()),
        }
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A git repo and a factory: implement (`max_questions: 1`) → review.
struct Env {
    dir: tempfile::TempDir,
    store: EventStore,
    fake: Fake,
}

impl Env {
    fn new(pool: PgPool, asks: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("README.md"), "hi").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"]);
        let agents = dir.path().join("factory/agents");
        std::fs::create_dir_all(&agents).unwrap();
        for role in ["implement", "review"] {
            std::fs::write(agents.join(format!("{role}.md")), format!("ROLE: {role}")).unwrap();
        }
        std::fs::write(
            dir.path().join("factory/factory.yaml"),
            r#"
name: test
repo: ../repo
base_ref: main
defaults: { max_retries: 0 }
stages:
  - { id: implement, agent: implement }
  - { id: review, agent: review }
agents:
  implement: { prompt: agents/implement.md, max_questions: 1 }
  review: { prompt: agents/review.md }
"#,
        )
        .unwrap();
        Self { dir, store: EventStore::new(pool), fake: Fake::new(asks) }
    }

    fn factory(&self) -> Factory {
        Factory::load(&self.dir.path().join("factory/factory.yaml")).unwrap()
    }

    fn runner(&self) -> Runner<Fake> {
        let config = RunnerConfig { data_dir: self.dir.path().join("data") };
        Runner::new(self.store.clone(), self.fake.clone(), config)
    }

    fn foreman(&self) -> Foreman<Fake> {
        Foreman::new(self.store.clone(), self.factory(), self.runner(), Dispatch::Inline).with_pause(true)
    }

    async fn task(&self, benchmark: Option<BenchmarkContext>) -> TaskId {
        self.create(benchmark, false).await
    }

    async fn create(&self, benchmark: Option<BenchmarkContext>, autopilot: bool) -> TaskId {
        let task = TaskId::new();
        let created = TaskEvent::TaskCreated {
            title: "t".into(),
            body: None,
            source: TaskSource::Cli,
            factory: Some("test".into()),
            benchmark,
            external_ref: None,
            parent: None,
            draft: false,
            autopilot,
        };
        self.append(task, created).await;
        task
    }

    async fn append(&self, task: TaskId, ev: TaskEvent) {
        append(&self.store, task, ev).await;
    }

    async fn answer(&self, task: TaskId) -> Result<(), wh_api::commands::CommandError> {
        let answer = Answer { option: Some("A".into()), text: None };
        let actor = Actor::Human { user: "tester".into() };
        wh_api::commands::answer(&self.store, actor, task, None, vec![answer]).await
    }

    async fn events(&self, task: TaskId) -> Vec<RecordedEvent> {
        self.store.read_stream(&task.stream_id()).await.unwrap()
    }

    async fn count(&self, task: TaskId, kind: &str) -> usize {
        self.events(task).await.iter().filter(|e| e.event_type == kind).count()
    }

    fn committed(&self, task: TaskId) -> String {
        let branch = wh_core::task::task_branch(task);
        git(&self.dir.path().join("repo"), &["show", &format!("{branch}:impl.txt")])
    }
}

async fn append(store: &EventStore, task: TaskId, ev: TaskEvent) {
    let meta = EventMeta::new(task.0, Actor::Human { user: "tester".into() });
    let events = vec![NewEvent::from_domain(&ev, meta).unwrap()];
    store.append(&task.stream_id(), ExpectedVersion::Any, events).await.unwrap();
}

fn waiting(state: &TaskState) -> bool {
    matches!(state.status, TaskStatus::AwaitingInput { .. })
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn ask_answer_resume(pool: PgPool) {
    let env = Env::new(pool, 1);
    let mut foreman = env.foreman();
    let task = env.task(None).await;

    let state = foreman.drive(task).await.unwrap();
    assert!(waiting(&state), "{:?}", state.status);
    assert!(state.paused.as_ref().unwrap().resume.is_some());
    assert_eq!(env.count(task, "StageExited").await, 0);

    env.answer(task).await.unwrap();
    let state = foreman.drive(task).await.unwrap();
    assert_eq!(state.status, TaskStatus::Done);
    assert_eq!(env.count(task, "StageResumed").await, 1);
    assert_eq!(env.count(task, "StageEntered").await, 2, "implement once, then review");
    assert_eq!(env.count(task, "StageExited").await, 2);
    let turns = env.fake.turns();
    assert_eq!(turns.len(), 2);
    assert!(turns[1].1 && turns[1].0.contains("Which?\nAnswer: A"), "{}", turns[1].0);
    assert_eq!(env.committed(task), "turn 2");
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn autopilot_answers_then_resume(pool: PgPool) {
    let env = Env::new(pool, 1);
    let mut foreman = env.foreman();
    let task = env.create(None, true).await;
    assert!(waiting(&foreman.drive(task).await.unwrap()));

    let data = env.dir.path().join("data");
    let dispatch = wh_autopilot::Dispatch::Inline;
    let autopilot = AutopilotService::new(env.store.clone(), env.factory(), env.fake.clone(), data, dispatch);
    let handled = autopilot.decide_now(task).await.unwrap();
    assert!(matches!(handled, Handled::Decided { .. }), "{handled:?}");

    assert_eq!(foreman.drive(task).await.unwrap().status, TaskStatus::Done);
    assert_eq!(env.count(task, "StageResumed").await, 1);
    assert_eq!(env.count(task, "StageExited").await, 2);
    let turns = env.fake.turns();
    assert!(turns[1].1 && turns[1].0.contains("Which?\nAnswer: B"), "{}", turns[1].0);
    assert_eq!(env.committed(task), "turn 2");
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn question_of_a_cancelled_task_is_dropped(pool: PgPool) {
    let env = Env::new(pool, 1);
    let mut foreman = env.foreman();
    let task = env.task(None).await;
    *env.fake.cancel.lock().unwrap() = Some((env.store.clone(), task));

    let state = foreman.drive(task).await.unwrap();
    assert_eq!(state.status, TaskStatus::Cancelled);
    assert_eq!(env.fake.turns().len(), 1);
    for kind in ["AgentAsked", "UserAnswered", "StageResumed"] {
        assert_eq!(env.count(task, kind).await, 0, "{kind}");
    }
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn pause_is_off_by_default(pool: PgPool) {
    let env = Env::new(pool, 1);
    let mut foreman = Foreman::new(env.store.clone(), env.factory(), env.runner(), Dispatch::Inline);
    let task = env.task(None).await;
    assert_eq!(foreman.drive(task).await.unwrap().status, TaskStatus::Done);
    assert_eq!(env.count(task, "AgentAsked").await, 0);
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn no_double_resume(pool: PgPool) {
    let env = Env::new(pool, 1);
    let mut foreman = env.foreman();
    let task = env.task(None).await;
    foreman.drive(task).await.unwrap();
    env.answer(task).await.unwrap();
    assert!(env.answer(task).await.is_err(), "a second answer is refused");

    let answered = env.events(task).await.pop().unwrap();
    assert_eq!(answered.event_type, "UserAnswered");
    foreman.handle(&answered).await.unwrap();
    foreman.handle(&answered).await.unwrap();
    assert_eq!(env.count(task, "StageResumed").await, 1);
    assert_eq!(env.fake.turns().len(), 2);
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn restart(pool: PgPool) {
    let env = Env::new(pool, 1);
    let task = env.task(None).await;
    env.foreman().drive(task).await.unwrap();

    let mut second = env.foreman();
    assert!(second.recover_interrupted().await.unwrap().is_empty());
    assert!(waiting(&second.drive(task).await.unwrap()));

    env.answer(task).await.unwrap();
    let mut third = env.foreman();
    assert!(third.recover_interrupted().await.unwrap().is_empty());
    // Another event lands after the answer: it must not hide it.
    env.append(task, TaskEvent::TaskMoved { parent: None }).await;
    assert_eq!(third.drive(task).await.unwrap().status, TaskStatus::Done);
    assert_eq!(env.count(task, "StageResumed").await, 1);
    assert!(env.fake.turns()[1].1);
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn limit(pool: PgPool) {
    let env = Env::new(pool, 3);
    let mut foreman = env.foreman();
    let task = env.task(None).await;
    foreman.drive(task).await.unwrap();
    env.answer(task).await.unwrap();

    assert_eq!(foreman.drive(task).await.unwrap().status, TaskStatus::Done);
    assert_eq!(env.count(task, "AgentAsked").await, 2, "the third question ends the stage");
    let events = env.events(task).await;
    let second = events.iter().rposition(|e| e.event_type == "AgentAsked").unwrap();
    let TaskEvent::UserAnswered { user, answers, .. } = events[second + 1].decode().unwrap() else {
        panic!("no automatic answer after {:?}", events[second + 1]);
    };
    assert_eq!(user, AUTO_ANSWER_USER);
    assert!(answers[0].text.as_ref().unwrap().contains("record your assumptions"));
    assert_eq!(env.fake.turns().len(), 3);
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn benchmark_auto_answers(pool: PgPool) {
    let env = Env::new(pool, 1);
    let run = BenchmarkRunId::new();
    let bench = BenchmarkContext { run, case: "c".into(), repo: None, base_ref: None };
    let task = env.task(Some(bench)).await;
    let mut foreman = Foreman::for_benchmark(env.store.clone(), env.factory(), env.runner(), run).with_pause(true);

    assert_eq!(foreman.drive(task).await.unwrap().status, TaskStatus::Done);
    assert_eq!(env.count(task, "AgentAsked").await, 1);
    let events = env.events(task).await;
    let answered = events.iter().find(|e| e.event_type == "UserAnswered").unwrap();
    assert!(matches!(answered.decode().unwrap(), TaskEvent::UserAnswered { user, .. } if user == AUTO_ANSWER_USER));
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn lost_blocks(pool: PgPool) {
    let env = Env::new(pool, 1);
    let mut foreman = env.foreman();
    let task = env.task(None).await;
    let state = foreman.drive(task).await.unwrap();
    let home = state.paused.unwrap().resume.unwrap().home_session;
    std::fs::remove_dir_all(env.dir.path().join("data/worktrees").join(home.to_string())).unwrap();

    env.answer(task).await.unwrap();
    let state = foreman.drive(task).await.unwrap();
    let TaskStatus::Blocked { reason } = &state.status else { panic!("{:?}", state.status) };
    assert!(reason.starts_with("cannot resume the paused conversation: "), "{reason}");

    env.append(task, TaskEvent::TaskResumed { stage: None, comment: None }).await;
    assert_eq!(foreman.drive(task).await.unwrap().status, TaskStatus::Done);
    assert!(!env.fake.turns()[1].1, "the stage was rerun, not resumed");
}
