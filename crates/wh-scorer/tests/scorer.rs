use std::{path::Path, process::Command, sync::Mutex, time::Duration};

use sqlx::PgPool;
use tokio::sync::mpsc::UnboundedSender;
use wh_core::{
    Actor, EventMeta, FailureClass, NewEvent, ScoreId, SessionId, TaskId,
    score::ScoreEvent,
    session::{SessionEvent, TokenUsage},
    task::{TaskEvent, TaskSource},
};
use wh_eventstore::{EventStore, ExpectedVersion, Subscription};
use wh_factory::Factory;
use wh_metrics::scores::{self, ScoreFilter};
use wh_runner::{AgentRequest, AgentRuntime, Outcome, OutcomeStatus, RunSpec, Runner, RunnerConfig, Signal};
use wh_scorer::{Dispatch, Graded, ScorerConfig, ScorerService};

/// Agent double: writes a file so the session has a commit to grade.
struct Worker;

impl AgentRuntime for Worker {
    fn name(&self) -> &str {
        "fake"
    }
    async fn run(&self, req: &AgentRequest, _tx: UnboundedSender<Signal>) -> Outcome {
        std::fs::write(req.workdir.join("feature.rs"), "fn feature() {}\n").unwrap();
        ok("added feature.rs", 0.02)
    }
}

/// Judge double: records prompts and answers with a canned reply.
struct Judge {
    reply: String,
    prompts: Mutex<Vec<AgentRequest>>,
}

impl Judge {
    fn new(reply: &str) -> Self {
        Self { reply: reply.into(), prompts: Mutex::new(Vec::new()) }
    }
    fn calls(&self) -> usize {
        self.prompts.lock().unwrap().len()
    }
}

impl AgentRuntime for &'static Judge {
    fn name(&self) -> &str {
        "judge"
    }
    async fn run(&self, req: &AgentRequest, _tx: UnboundedSender<Signal>) -> Outcome {
        self.prompts.lock().unwrap().push(req.clone());
        ok(&self.reply, 0.001)
    }
}

fn ok(summary: &str, cost: f64) -> Outcome {
    Outcome {
        status: OutcomeStatus::Succeeded,
        usage: TokenUsage { input_tokens: 100, output_tokens: 10, ..Default::default() },
        cost_usd: cost,
        num_turns: 1,
        duration_ms: 5,
        summary: Some(summary.into()),
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
    std::fs::create_dir_all(f.join("scorers")).unwrap();
    std::fs::write(f.join("agents/impl.md"), "ROLE: implement").unwrap();
    std::fs::write(f.join("agents/review.md"), "ROLE: review").unwrap();
    std::fs::write(f.join("scorers/scope.md"), "RUBRIC: only touch what the task asks").unwrap();
    std::fs::write(
        f.join("factory.yaml"),
        r#"
name: test
repo: ../repo
base_ref: main
stages:
  - { id: implement, agent: impl }
  - { id: review, agent: review }
agents:
  impl: { prompt: agents/impl.md }
  review: { prompt: agents/review.md }
scorers:
  - { id: scope, target: impl, prompt: scorers/scope.md, pass_threshold: 0.6 }
"#,
    )
    .unwrap();
    let factory = Factory::load(&f.join("factory.yaml")).unwrap();
    (dir, factory)
}

async fn create_task(store: &EventStore, factory: Option<&str>) -> TaskId {
    let task = TaskId::new();
    let ev = TaskEvent::TaskCreated {
        title: "Add feature".into(),
        body: Some("Implement fn feature".into()),
        source: TaskSource::Cli,
        factory: factory.map(Into::into),
        benchmark: None,
        external_ref: None,
        parent: None,
        draft: false,
    };
    let meta = EventMeta::new(task.0, Actor::Human { user: "t".into() });
    store.append(&task.stream_id(), ExpectedVersion::NoStream, vec![NewEvent::from_domain(&ev, meta).unwrap()]).await.unwrap();
    task
}

/// Run one stage with the fake worker; returns the session id.
async fn run_stage(store: &EventStore, factory: &Factory, data: &Path, task: TaskId, agent: &str) -> SessionId {
    let runner = Runner::new(store.clone(), Worker, RunnerConfig { data_dir: data.to_owned() });
    let spec = RunSpec {
        task_id: task,
        agent: agent.into(),
        stage: "implement".into(),
        prompt: "ROLE: implement".into(),
        repo: factory.repo.clone(),
        base_ref: "main".into(),
        model: None,
        budget_usd: None,
        permission_mode: None,
        allowed_tools: vec![],
        tools: None,
        with_user_mcp: false,
        timeout: Duration::from_secs(30),
        keep_worktree: false,
        pause_on_question: false,
        max_retries: 0,
        retry_backoff: Duration::ZERO,
        branch: Some(format!("wh/task/{}", &task.0.simple().to_string()[20..])),
        commit_on_success: true,
        verdict_required: false,
        task_expected_version: None,
        feedback: None,
        factory_version: Some(factory.version.clone()),
        benchmark: None,
        sandbox: Default::default(),
        env: Default::default(),
        merge: None,
    };
    runner.run(spec, |_| {}).await.unwrap().last().session_id
}

fn leak(j: Judge) -> &'static Judge {
    Box::leak(Box::new(j))
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn grades_target_sessions_once_and_projects_results(pool: PgPool) {
    let (dir, factory) = setup();
    let data = dir.path().join("data");
    let store = EventStore::new(pool.clone());
    let judge = leak(Judge::new(
        "Only feature.rs changed.\n{\"score\": 0.8, \"classification\": \"on_scope\", \"rationale\": \"one file\"}",
    ));

    let task = create_task(&store, Some("test")).await;
    let graded = run_stage(&store, &factory, &data, task, "impl").await;
    let _not_target = run_stage(&store, &factory, &data, task, "review").await;
    let other = create_task(&store, None).await; // not this factory's task
    let _other_session = run_stage(&store, &factory, &data, other, "impl").await;

    let cfg = ScorerConfig { data_dir: data.clone() };
    let mut svc = ScorerService::new(store.clone(), factory.clone(), judge, cfg, Dispatch::Inline);
    let sub = Subscription::new(store.clone());
    sub.catch_up(&mut svc).await.unwrap();
    assert_eq!(judge.calls(), 1, "only the impl session of the factory task is graded");

    // The judge saw the rubric, the task, the committed diff, and ran with no tools.
    let req = judge.prompts.lock().unwrap()[0].clone();
    for needle in ["RUBRIC: only touch", "Add feature", "Implement fn feature", "+fn feature() {}", "added feature.rs"] {
        assert!(req.prompt.contains(needle), "prompt lacks {needle:?}:\n{}", req.prompt);
    }
    assert_eq!(req.tools, Some(vec![]));
    assert_eq!(req.model.as_deref(), Some("claude-haiku-5-5"));

    // Redelivery does not grade again.
    store.reset_checkpoint(svc.consumer_name()).await.unwrap();
    sub.catch_up(&mut svc).await.unwrap();
    assert_eq!(judge.calls(), 1);

    let stream = ScoreId::for_session(graded, "scope").stream_id();
    let events = store.read_stream(&stream).await.unwrap();
    let types: Vec<_> = events.iter().map(|e| e.event_type.as_str()).collect();
    assert_eq!(types, ["ScoreStarted", "ScoreRecorded"]);
    assert_eq!(events[0].meta.correlation_id, task.0);
    assert!(matches!(events[1].decode().unwrap(), ScoreEvent::ScoreRecorded { pass: true, score: 0.8, .. }));

    wh_metrics::catch_up_all(&store).await.unwrap();
    let rows = scores::scores(&pool, &ScoreFilter::default(), 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    let r = &rows[0];
    assert_eq!((r.status.as_str(), r.pass, r.score), ("scored", Some(true), Some(0.8)));
    assert_eq!((r.target_session, r.task_title.as_deref()), (graded.0, Some("Add feature")));
    assert_eq!(r.classification.as_deref(), Some("on_scope"));

    let stats = scores::scorer_stats(&pool).await.unwrap();
    assert_eq!((stats[0].scorer.as_str(), stats[0].scored, stats[0].passed), ("scope", 1, 1));
    assert_eq!(stats[0].cost_usd, 0.001);
    let by_version = scores::scorer_stats_by_version(&pool).await.unwrap();
    assert_eq!(by_version[0].factory_version.as_deref(), Some(factory.version.as_str()));

    let totals = wh_metrics::queries::totals(&pool, 7).await.unwrap();
    assert_eq!((totals.scores, totals.scores_passed, totals.scorer_cost_usd), (1, 1, 0.001));

    // A human can re-grade explicitly; that is a new, manual score.
    let svc2 = ScorerService::new(store.clone(), factory, judge, ScorerConfig { data_dir: data }, Dispatch::Inline);
    let out = svc2.grade_session(graded, None).await.unwrap();
    assert!(matches!(&out[..], [(id, Graded::Scored { pass: true, .. })] if id == "scope"));
    wh_metrics::catch_up_all(&store).await.unwrap();
    let rows = scores::scores(&pool, &ScoreFilter { session: Some(graded.0), ..Default::default() }, 10).await.unwrap();
    assert_eq!(rows.iter().filter(|r| r.manual).count(), 1);
}

/// Record a session of agent `impl` that ends with `end`.
async fn record_session(store: &EventStore, task: TaskId, prompt: &str, resume_of: Option<SessionId>, end: SessionEvent) -> SessionId {
    let session = SessionId::new();
    let dispatched = SessionEvent::AgentDispatched {
        task_id: task,
        agent: "impl".into(),
        stage: "implement".into(),
        runtime: "fake".into(),
        model: None,
        prompt: prompt.into(),
        budget_usd: None,
        timeout_secs: 60,
        attempt: 1,
        retry_of: None,
        resume_of,
        benchmark: None,
    };
    for ev in [dispatched, end] {
        let meta = EventMeta::new(task.0, Actor::System { component: "runner".into() });
        let new = NewEvent::from_domain(&ev, meta).unwrap();
        store.append(&session.stream_id(), ExpectedVersion::Any, vec![new]).await.unwrap();
    }
    session
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn paused_and_lost_turns_are_skipped_and_a_resumed_one_keeps_the_first_prompt(pool: PgPool) {
    let (dir, _) = setup();
    let yaml = dir.path().join("factory/factory.yaml");
    let text = std::fs::read_to_string(&yaml).unwrap().replace("pass_threshold: 0.6", "pass_threshold: 0.6, include_failed: true");
    std::fs::write(&yaml, text).unwrap();
    let factory = Factory::load(&yaml).unwrap();
    let store = EventStore::new(pool);
    let judge = leak(Judge::new("{\"score\": 1.0, \"classification\": \"ok\", \"rationale\": \"fine\"}"));
    let completed = |paused| SessionEvent::AgentSessionCompleted {
        num_turns: 1,
        duration_ms: 1,
        summary: Some("done".into()),
        transcript: None,
        commit: None,
        paused,
    };
    let lost = SessionEvent::AgentSessionFailed {
        failure_class: FailureClass::SessionLost,
        message: "gone".into(),
        retryable: false,
        num_turns: 0,
        duration_ms: 0,
        transcript: None,
    };
    let task = create_task(&store, Some("test")).await;
    let first = record_session(&store, task, "FIRST PROMPT", None, completed(true)).await;
    let second = record_session(&store, task, "answer one", Some(first), completed(true)).await;
    let gone = record_session(&store, task, "answer two", Some(second), lost).await;
    let last = record_session(&store, task, "answer three", Some(second), completed(false)).await;

    let cfg = ScorerConfig { data_dir: dir.path().join("data") };
    let mut svc = ScorerService::new(store.clone(), factory, judge, cfg, Dispatch::Inline);
    Subscription::new(store.clone()).catch_up(&mut svc).await.unwrap();

    for (session, graded) in [(first, false), (second, false), (gone, false), (last, true)] {
        let events = store.read_stream(&ScoreId::for_session(session, "scope").stream_id()).await.unwrap();
        assert_eq!(!events.is_empty(), graded);
    }
    let prompt = judge.prompts.lock().unwrap()[0].prompt.clone();
    assert!(prompt.contains("FIRST PROMPT") && !prompt.contains("answer three"), "{prompt}");
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn failing_and_unparseable_judgements(pool: PgPool) {
    let (dir, factory) = setup();
    let data = dir.path().join("data");
    let store = EventStore::new(pool.clone());
    let task = create_task(&store, Some("test")).await;
    let session = run_stage(&store, &factory, &data, task, "impl").await;

    let low = leak(Judge::new("{\"score\": 0.3, \"classification\": \"scope_creep\", \"rationale\": \"edited CI\"}"));
    let svc = ScorerService::new(store.clone(), factory.clone(), low, ScorerConfig { data_dir: data.clone() }, Dispatch::Inline);
    let out = svc.grade_session(session, Some("scope")).await.unwrap();
    assert!(matches!(&out[0].1, Graded::Scored { pass: false, score, .. } if *score == 0.3));

    let garbage = leak(Judge::new("I think it is fine."));
    let svc = ScorerService::new(store.clone(), factory.clone(), garbage, ScorerConfig { data_dir: data }, Dispatch::Inline);
    let out = svc.grade_session(session, None).await.unwrap();
    assert!(matches!(&out[0].1, Graded::Failed { cost_usd, .. } if *cost_usd == 0.001));
    assert!(svc.grade_session(session, Some("nope")).await.is_err());

    wh_metrics::catch_up_all(&store).await.unwrap();
    let failing = scores::scores(&pool, &ScoreFilter { failing: true, ..Default::default() }, 10).await.unwrap();
    assert_eq!(failing.len(), 1);
    assert_eq!(failing[0].classification.as_deref(), Some("scope_creep"));
    let s = &scores::scorer_stats(&pool).await.unwrap()[0];
    assert_eq!((s.runs, s.scored, s.passed, s.errors, s.pass_rate), (2, 1, 0, 1, Some(0.0)));
    assert!((s.cost_usd - 0.002).abs() < 1e-9, "failed judgements still cost money");
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn startup_skips_history_and_recovers_interrupted(pool: PgPool) {
    let (dir, factory) = setup();
    let data = dir.path().join("data");
    let store = EventStore::new(pool.clone());
    let task = create_task(&store, Some("test")).await;
    let old = run_stage(&store, &factory, &data, task, "impl").await;

    // A grading left dangling by a crashed process.
    let dangling = ScoreId::new();
    let started = ScoreEvent::ScoreStarted {
        scorer: "scope".into(),
        target_session: old,
        task_id: task,
        agent: "impl".into(),
        stage: "implement".into(),
        model: None,
        pass_threshold: 0.6,
        manual: false,
    };
    let meta = EventMeta::new(task.0, Actor::System { component: "scorer".into() });
    store
        .append(&dangling.stream_id(), ExpectedVersion::NoStream, vec![NewEvent::from_domain(&started, meta).unwrap()])
        .await
        .unwrap();

    let judge = leak(Judge::new("{\"score\": 1}"));
    let mut svc = ScorerService::new(store.clone(), factory.clone(), judge, ScorerConfig { data_dir: data.clone() }, Dispatch::Inline);
    assert_eq!(svc.recover_interrupted().await.unwrap(), 1);
    assert_eq!(svc.recover_interrupted().await.unwrap(), 0, "recovery is idempotent");
    assert_eq!(store.read_stream(&dangling.stream_id()).await.unwrap().last().unwrap().event_type, "ScoreFailed");

    assert!(svc.start_from_head_if_new().await.unwrap());
    assert!(!svc.start_from_head_if_new().await.unwrap(), "only on the first start");
    let sub = Subscription::new(store.clone());
    sub.catch_up(&mut svc).await.unwrap();
    assert_eq!(judge.calls(), 0, "history before the first start is not graded");

    run_stage(&store, &factory, &data, task, "impl").await;
    sub.catch_up(&mut svc).await.unwrap();
    assert_eq!(judge.calls(), 1, "new sessions are graded");
}
