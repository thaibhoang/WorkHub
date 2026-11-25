use std::{path::Path, process::Command};

use sqlx::PgPool;
use tokio::sync::mpsc::UnboundedSender;
use wh_bench::{BenchConfig, BenchService, Dispatch, Request, RunResult};
use wh_core::{
    Actor, BenchmarkRunId, EventMeta, NewEvent,
    benchmark::BenchmarkEvent,
    session::TokenUsage,
    task::{TaskEvent, TaskSource},
};
use wh_eventstore::{EventStore, ExpectedVersion};
use wh_factory::Factory;
use wh_runner::{AgentRequest, AgentRuntime, Outcome, OutcomeStatus, Signal};

/// Implementer writes `answer.txt` with "42" (or "wrong" when the task body says so);
/// reviewer always approves.
struct Fake;

fn ok(summary: &str) -> Outcome {
    Outcome {
        status: OutcomeStatus::Succeeded,
        usage: TokenUsage::default(),
        cost_usd: 0.01,
        num_turns: 1,
        duration_ms: 5,
        summary: Some(summary.into()),
    }
}

impl AgentRuntime for Fake {
    fn name(&self) -> &str {
        "fake"
    }
    async fn run(&self, req: &AgentRequest, _tx: UnboundedSender<Signal>) -> Outcome {
        if req.prompt.starts_with("ROLE: implement") {
            assert!(req.workdir.join("README").exists(), "agents must work in the fixture repo");
            assert!(!req.workdir.join("answer.sh").exists());
            let answer = if req.prompt.contains("answer wrong") { "wrong" } else { "42" };
            std::fs::write(req.workdir.join("answer.txt"), answer).unwrap();
            ok("implemented")
        } else if req.prompt.starts_with("ROLE: review") {
            ok("fine\nVERDICT: APPROVE")
        } else {
            ok("spec")
        }
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A git repo holding `factory/` (spec with gate → implement → review) and
/// `benchmarks/` (suite + fixture), committed on `main`.
fn setup() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("factory/agents")).unwrap();
    for role in ["spec", "implement", "review"] {
        std::fs::write(root.join(format!("factory/agents/{role}.md")), format!("ROLE: {role}")).unwrap();
    }
    std::fs::write(
        root.join("factory/factory.yaml"),
        r#"
name: test
repo: ..
base_ref: main
defaults: { max_retries: 0 }
stages:
  - { id: spec, agent: spec, gate: spec-review }
  - { id: implement, agent: implement }
  - { id: review, agent: review, on_reject: implement }
agents:
  spec: { prompt: agents/spec.md }
  implement: { prompt: agents/implement.md }
  review: { prompt: agents/review.md }
"#,
    )
    .unwrap();
    std::fs::create_dir_all(root.join("benchmarks/fixtures/answer")).unwrap();
    std::fs::write(root.join("benchmarks/fixtures/answer/README"), "answer goes in answer.txt\n").unwrap();
    std::fs::create_dir_all(root.join("benchmarks/checks")).unwrap();
    std::fs::write(root.join("benchmarks/checks/answer.sh"), "test \"$(cat answer.txt)\" = 42 && test \"$WH_BENCH_CASE\" != \"\"\n").unwrap();
    std::fs::write(
        root.join("benchmarks/smoke.yaml"),
        r#"
name: smoke
factory: ../factory/factory.yaml
cases:
  - { id: right, title: Answer, body: "write the answer", fixture: fixtures/answer, check: "sh $WH_BENCH_SUITE_DIR/checks/answer.sh" }
  - { id: wrong, title: Answer badly, body: "answer wrong", fixture: fixtures/answer, check: "sh $WH_BENCH_SUITE_DIR/checks/answer.sh" }
"#,
    )
    .unwrap();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["add", "."]);
    git(root, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"]);
    dir
}

fn service(store: &EventStore, data: &Path) -> BenchService<Fake> {
    BenchService::new(store.clone(), || Fake, BenchConfig { data_dir: data.to_owned() }, Dispatch::Inline)
}

fn request(root: &Path, git_ref: Option<&str>, repeat: u32) -> Request {
    Request {
        suite_file: root.join("benchmarks/smoke.yaml"),
        git_ref: git_ref.map(Into::into),
        proposal: None,
        label: None,
        repeat,
        cases: Vec::new(),
    }
}

async fn bench_events(store: &EventStore, id: BenchmarkRunId) -> Vec<BenchmarkEvent> {
    store.read_stream(&id.stream_id()).await.unwrap().iter().map(|e| e.decode().unwrap()).collect()
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn runs_suite_checks_results_and_stays_out_of_live_metrics(pool: PgPool) {
    let dir = setup();
    let store = EventStore::new(pool.clone());
    let svc = service(&store, &dir.path().join("data"));
    let human = Actor::Human { user: "tester".into() };

    let (id, result) = svc.run_now(human, &request(dir.path(), None, 2)).await.unwrap();
    let RunResult::Completed { cases, passed, cost_usd, aborted } = result else { panic!("{result:?}") };
    assert_eq!((cases, passed, aborted), (4, 2, None));
    assert!((cost_usd - 4.0 * 3.0 * 0.01).abs() < 1e-9, "3 sessions per case: {cost_usd}");

    let events = bench_events(&store, id).await;
    let types: Vec<_> = events.iter().map(|e| wh_core::DomainEvent::event_type(e)).collect();
    assert_eq!(types[..2], ["BenchmarkRequested", "BenchmarkStarted"]);
    assert_eq!(types.last(), Some(&"BenchmarkCompleted"));
    let finished: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            BenchmarkEvent::BenchmarkCaseFinished { case, passed, check, task_status, sessions, task_id, .. } => {
                Some((case.clone(), *passed, check.clone().unwrap(), task_status.clone(), *sessions, *task_id))
            }
            _ => None,
        })
        .collect();
    assert_eq!(finished.len(), 4);
    for (case, passed, check, status, sessions, _) in &finished {
        assert_eq!((status.as_str(), *sessions), ("done", 3));
        assert_eq!(*passed, case == "right", "{case}: {check:?}");
        assert_eq!(check.exit_code, Some(if case == "right" { 0 } else { 1 }));
    }

    // The gate was approved by the benchmark, not left for a human.
    let task = finished[0].5;
    let task_events: Vec<TaskEvent> =
        store.read_stream(&task.stream_id()).await.unwrap().iter().map(|e| e.decode().unwrap()).collect();
    assert!(task_events.iter().any(|e| matches!(e,
        TaskEvent::GateDecided { approved: true, reviewer, .. } if reviewer == wh_foreman::BENCHMARK_REVIEWER)));
    assert!(matches!(&task_events[0], TaskEvent::TaskCreated { source: TaskSource::Benchmark, benchmark: Some(b), .. } if b.run == id));

    // Read models: the run and its cases are there, live metrics are untouched.
    let run = wh_metrics::benchmarks::run(&pool, id.0).await.unwrap().unwrap();
    assert_eq!((run.status.as_str(), run.cases_done, run.passed, run.planned), ("completed", 4, 2, Some(4)));
    assert_eq!(run.pass_rate, Some(0.5));
    assert_eq!(run.factory.as_deref(), Some("test"));
    let stats = wh_metrics::benchmarks::case_stats(&pool, id.0).await.unwrap();
    assert_eq!(stats.iter().map(|c| (c.case_id.as_str(), c.runs, c.passed)).collect::<Vec<_>>(), [("right", 2, 2), ("wrong", 2, 0)]);
    assert!(wh_metrics::queries::tasks(&pool, None, 100).await.unwrap().is_empty());
    assert!(wh_metrics::queries::task(&pool, task.0).await.unwrap().unwrap().benchmark_run.is_some());
    assert_eq!(wh_metrics::queries::task_sessions(&pool, task.0).await.unwrap().len(), 3);
    assert_eq!(wh_metrics::queries::totals(&pool, 7).await.unwrap().sessions, 0);
    assert!(wh_metrics::sessions::SessionFactsProjection::stats_by_agent(&pool).await.unwrap().is_empty());
    assert!(wh_metrics::proposals::version_stats(&pool, None).await.unwrap().is_empty());
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn variant_at_git_ref_and_queued_request(pool: PgPool) {
    let dir = setup();
    let root = dir.path();
    let base_version = Factory::load(&root.join("factory/factory.yaml")).unwrap().version;
    // A later commit changes a prompt (= a new factory version).
    std::fs::write(root.join("factory/agents/spec.md"), "ROLE: spec (v2)").unwrap();
    git(root, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qam", "v2"]);
    let store = EventStore::new(pool.clone());
    let svc = service(&store, &root.join("data"));

    // Queue a run of the old factory (HEAD^) like the API does; the worker claims it once.
    let mut req = request(root, Some("HEAD^"), 1);
    req.cases = vec!["right".into()];
    let id = wh_bench::commands::request(&store, Actor::Human { user: "t".into() }, &req).await.unwrap();
    let r = svc.run(id).await.unwrap();
    assert!(matches!(r, RunResult::Completed { cases: 1, passed: 1, .. }), "{r:?}");
    assert_eq!(svc.run(id).await.unwrap(), RunResult::NotPending);

    let started = bench_events(&store, id).await.into_iter().find_map(|e| match e {
        BenchmarkEvent::BenchmarkStarted { factory_version, factory_commit, .. } => Some((factory_version, factory_commit)),
        _ => None,
    });
    let (version, commit) = started.unwrap();
    assert_eq!(version, base_version);
    assert_eq!(commit.as_deref(), Some(git(root, &["rev-parse", "HEAD^"]).as_str()));
    // The detached factory worktree is cleaned up.
    assert!(!root.join("data/bench").join(id.to_string()).join("factory").exists());

    // An unknown ref fails the run (recorded, not an Err).
    let id = wh_bench::commands::request(&store, Actor::Human { user: "t".into() }, &request(root, Some("nope"), 1)).await.unwrap();
    assert!(matches!(svc.run(id).await.unwrap(), RunResult::Failed { message } if message.contains("unknown git ref")));
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn recovery_fails_orphaned_runs_and_lists_pending(pool: PgPool) {
    let dir = setup();
    let store = EventStore::new(pool.clone());
    let svc = service(&store, &dir.path().join("data"));
    let req = request(dir.path(), None, 1);
    let pending = wh_bench::commands::request(&store, Actor::Human { user: "t".into() }, &req).await.unwrap();

    // A run claimed by a process that no longer exists.
    let orphan = wh_bench::commands::request(&store, Actor::Human { user: "t".into() }, &req).await.unwrap();
    let started = BenchmarkEvent::BenchmarkStarted {
        factory: "test".into(),
        factory_version: "x".into(),
        factory_commit: None,
        suite_version: "y".into(),
        planned: 2,
        pid: Some(u32::MAX - 1),
    };
    let meta = EventMeta::new(orphan.0, Actor::System { component: "bench".into() });
    store.append(&orphan.stream_id(), ExpectedVersion::Exact(1), vec![NewEvent::from_domain(&started, meta).unwrap()]).await.unwrap();

    let (failed, waiting) = svc.recover().await.unwrap();
    assert_eq!((failed, waiting), (1, vec![pending]));
    assert!(matches!(bench_events(&store, orphan).await.last(), Some(BenchmarkEvent::BenchmarkFailed { .. })));
}
