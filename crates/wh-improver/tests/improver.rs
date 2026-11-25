use std::{path::Path, process::Command, sync::Mutex};

use sqlx::PgPool;
use tokio::sync::mpsc::UnboundedSender;
use wh_core::{
    Actor, EventMeta, FailureClass, NewEvent, ProposalId, SessionId, TaskId,
    proposal::ProposalEvent,
    session::{SessionEvent, TokenUsage},
    task::{StageOutcome, TaskEvent, TaskSource},
};
use wh_eventstore::{EventStore, ExpectedVersion, Subscription};
use wh_factory::Factory;
use wh_improver::{Dispatch, ImproverConfig, ImproverService, RunResult, commands};
use wh_metrics::proposals;
use wh_runner::{AgentRequest, AgentRuntime, Outcome, OutcomeStatus, Signal};

/// What the fake improver does in its worktree.
#[derive(Clone)]
enum Act {
    /// Write `content` to `path` (relative to the worktree) and answer `reply`.
    Edit { path: &'static str, content: &'static str, reply: String },
    Reply(String),
}

struct FakeImprover {
    act: Mutex<Act>,
    prompts: Mutex<Vec<AgentRequest>>,
}

impl FakeImprover {
    fn leak(act: Act) -> &'static Self {
        Box::leak(Box::new(Self { act: Mutex::new(act), prompts: Mutex::new(Vec::new()) }))
    }
    fn set(&self, act: Act) {
        *self.act.lock().unwrap() = act;
    }
    fn calls(&self) -> usize {
        self.prompts.lock().unwrap().len()
    }
    fn last_prompt(&self) -> String {
        self.prompts.lock().unwrap().last().unwrap().prompt.clone()
    }
}

impl AgentRuntime for &'static FakeImprover {
    fn name(&self) -> &str {
        "fake-improver"
    }
    async fn run(&self, req: &AgentRequest, _tx: UnboundedSender<Signal>) -> Outcome {
        self.prompts.lock().unwrap().push(req.clone());
        let reply = match self.act.lock().unwrap().clone() {
            Act::Edit { path, content, reply } => {
                std::fs::write(req.workdir.join(path), content).unwrap();
                reply
            }
            Act::Reply(r) => r,
        };
        Outcome {
            status: OutcomeStatus::Succeeded,
            usage: TokenUsage { input_tokens: 1000, output_tokens: 100, ..Default::default() },
            cost_usd: 0.05,
            num_turns: 3,
            duration_ms: 10,
            summary: Some(reply),
        }
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

const FACTORY_YAML: &str = r#"
name: test
repo: ..
base_ref: main
stages:
  - { id: implement, agent: impl }
  - { id: review, agent: review, on_reject: implement }
agents:
  impl: { prompt: agents/impl.md }
  review: { prompt: agents/review.md }
improvement:
  prompt: improver.md
  min_sessions: 3
  triggers:
    - { metric: stage_failure_rate, stage: implement, above: 0.5 }
"#;

/// A git repo with the factory under `factory/` (like WorkHub's own layout).
fn setup() -> (tempfile::TempDir, Factory) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let f = repo.join("factory");
    std::fs::create_dir_all(f.join("agents")).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hi").unwrap();
    std::fs::write(f.join("agents/impl.md"), "ROLE: implement").unwrap();
    std::fs::write(f.join("agents/review.md"), "ROLE: review").unwrap();
    std::fs::write(f.join("improver.md"), "IMPROVER: find the worst pattern").unwrap();
    std::fs::write(f.join("factory.yaml"), FACTORY_YAML).unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"]);
    let factory = Factory::load(&f.join("factory.yaml")).unwrap();
    (dir, factory)
}

fn service(store: &EventStore, f: &Factory, data: &Path, rt: &'static FakeImprover) -> ImproverService<&'static FakeImprover> {
    ImproverService::new(store.clone(), f.clone(), rt, ImproverConfig { data_dir: data.to_owned() }, Dispatch::Inline)
        .unwrap()
}

fn human() -> Actor {
    Actor::Human { user: "tester".into() }
}

async fn append<E: wh_core::DomainEvent>(store: &EventStore, stream: &str, corr: uuid::Uuid, version: Option<&str>, ev: E) {
    let mut meta = EventMeta::new(corr, Actor::System { component: "test".into() });
    meta.factory_version = version.map(Into::into);
    store.append(stream, ExpectedVersion::Any, vec![NewEvent::from_domain(&ev, meta).unwrap()]).await.unwrap();
}

/// A factory task with one finished session at `stage` (failed or rejected/succeeded).
async fn session(store: &EventStore, f: &Factory, stage: &str, failed: bool, rejected: bool) -> SessionId {
    let (task, s) = (TaskId::new(), SessionId::new());
    let v = Some(f.version.as_str());
    let created = TaskEvent::TaskCreated {
        title: format!("task at {stage}"),
        body: None,
        source: TaskSource::Cli,
        factory: Some(f.name.clone()),
        benchmark: None,
        external_ref: None,
    };
    append(store, &task.stream_id(), task.0, v, created).await;
    let dispatched = SessionEvent::AgentDispatched {
        task_id: task,
        agent: "impl".into(),
        stage: stage.into(),
        runtime: "fake".into(),
        model: None,
        prompt: "p".into(),
        budget_usd: None,
        timeout_secs: 60,
        attempt: 1,
        retry_of: None,
        benchmark: None,
    };
    append(store, &s.stream_id(), task.0, v, dispatched).await;
    let end = if failed {
        SessionEvent::AgentSessionFailed {
            failure_class: FailureClass::ToolError,
            message: "permission denied: chmod".into(),
            retryable: false,
            num_turns: 2,
            duration_ms: 1000,
            transcript: None,
        }
    } else {
        SessionEvent::AgentSessionCompleted { num_turns: 2, duration_ms: 1000, summary: None, transcript: None, commit: None }
    };
    append(store, &s.stream_id(), task.0, v, end).await;
    let outcome = match (failed, rejected) {
        (true, _) => StageOutcome::Failed,
        (_, true) => StageOutcome::Rejected,
        _ => StageOutcome::Succeeded,
    };
    let exited = TaskEvent::StageExited {
        stage: stage.into(),
        outcome,
        summary: rejected.then(|| "missing tests\nVERDICT: REJECT".into()),
        session: Some(s),
    };
    append(store, &task.stream_id(), task.0, v, exited).await;
    s
}

fn proposal_reply(title: &str, evidence: &[SessionId]) -> String {
    let ev: Vec<String> = evidence.iter().map(|s| format!("\"{}\"", s.0)).collect();
    format!(
        "Implement fails on chmod.\n{{\"title\": \"{title}\", \"rationale\": \"tool_error x2\", \
         \"expected_impact\": \"implement fail rate 67% -> 10%\", \"evidence\": [{}]}}",
        ev.join(", ")
    )
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn proposes_reviews_applies_and_measures(pool: PgPool) {
    let (dir, f) = setup();
    let repo = f.path.parent().unwrap().parent().unwrap().to_owned();
    let data = dir.path().join("data");
    let store = EventStore::new(pool.clone());
    let bad = session(&store, &f, "implement", true, false).await;
    session(&store, &f, "implement", true, false).await;
    session(&store, &f, "implement", false, false).await;
    session(&store, &f, "review", false, true).await;
    wh_metrics::catch_up_all(&store).await.unwrap();

    let rt = FakeImprover::leak(Act::Edit {
        path: "factory/agents/impl.md",
        content: "ROLE: implement. You may use chmod.",
        reply: proposal_reply("Allow chmod in implement", &[bad]),
    });
    let svc = service(&store, &f, &data, rt);

    // The observation is what the improver sees.
    let obs = svc.observe().await.unwrap();
    assert_eq!(obs.sessions(), 4);
    let (id, result) = svc.run_now(human(), Some("why does implement fail?".into())).await.unwrap();
    let RunResult::Proposed { files, factory_version: proposed, cost_usd, .. } = result else { panic!("{result:?}") };
    assert_eq!(files, ["factory/agents/impl.md"]);
    assert_ne!(proposed, f.version);
    assert_eq!(cost_usd, 0.05);

    let prompt = rt.last_prompt();
    for needle in [
        "IMPROVER: find the worst pattern",
        "only edit files inside `factory/`",
        "why does implement fail?",
        "`tool_error` × 2 at stage `implement`",
        "review REJECT at `review`",
        "missing tests",
        &f.version,
    ] {
        assert!(prompt.contains(needle), "prompt lacks {needle:?}:\n{prompt}");
    }
    assert_eq!(rt.prompts.lock().unwrap()[0].permission_mode.as_deref(), Some("acceptEdits"));

    // The change lives on its own branch; the main checkout is untouched.
    let branch = format!("wh/proposal/{id}");
    assert_eq!(git(&repo, &["show", &format!("{branch}:factory/agents/impl.md")]), "ROLE: implement. You may use chmod.");
    assert_eq!(std::fs::read_to_string(repo.join("factory/agents/impl.md")).unwrap(), "ROLE: implement");
    assert!(!data.join(format!("worktrees/proposal-{id}")).exists(), "worktree removed");

    let row = proposals::proposal(&pool, id.0).await.unwrap().unwrap();
    assert_eq!((row.status.as_str(), row.trigger.as_str(), row.requested_by.as_str()), ("open", "manual", "tester"));
    assert_eq!(row.title.as_deref(), Some("Allow chmod in implement"));
    assert_eq!(row.base_factory_version.as_deref(), Some(f.version.as_str()));
    assert_eq!(row.sessions_observed, Some(4));
    assert_eq!(row.evidence, serde_json::json!([bad.0]));
    assert!(row.diff.as_deref().unwrap().contains("+ROLE: implement. You may use chmod."));

    // Review: rejecting needs a comment; approving twice is refused.
    assert!(matches!(
        commands::decide(&store, human(), id, false, None).await,
        Err(commands::CommandError::InvalidState(_))
    ));
    assert!(matches!(commands::apply(&store, human(), id).await, Err(commands::CommandError::InvalidState(_))));
    commands::decide(&store, human(), id, true, Some("good catch".into())).await.unwrap();
    assert!(commands::decide(&store, human(), id, true, None).await.is_err());

    // Rollout: cherry-picked onto main, factory reloads with the proposed version.
    let version = commands::apply(&store, human(), id).await.unwrap();
    assert_eq!(version, proposed);
    assert_eq!(std::fs::read_to_string(repo.join("factory/agents/impl.md")).unwrap(), "ROLE: implement. You may use chmod.");
    assert_eq!(Factory::load(&f.path).unwrap().version, proposed);
    let row = proposals::proposal(&pool, id.0).await.unwrap().unwrap();
    assert_eq!((row.status.as_str(), row.rolled_out_version.as_deref()), ("rolled_out", Some(proposed.as_str())));
    assert_eq!(row.decided_by.as_deref(), Some("tester"));

    // Measure: metrics per factory version (sessions of the old version so far).
    let versions = proposals::version_stats(&pool, Some("test")).await.unwrap();
    assert_eq!(versions.len(), 1);
    let v = &versions[0];
    assert_eq!((v.factory_version.as_str(), v.sessions, v.failed, v.rejected, v.tasks), (f.version.as_str(), 4, 2, 1, 4));
    assert_eq!(v.fail_rate, Some(0.5));

    // The improver remembers decisions: the next prompt lists this proposal.
    rt.set(Act::Reply("{\"no_proposal\": \"just changed\"}".into()));
    let (_, r) = svc.run_now(human(), None).await.unwrap();
    assert_eq!(r, RunResult::NoProposal { reason: "just changed".into(), cost_usd: 0.05 });
    assert!(rt.last_prompt().contains("[rolled_out] Allow chmod in implement — files: factory/agents/impl.md — reviewer: good catch"));

    let stats = proposals::improver_stats(&pool, Some("test")).await.unwrap();
    assert_eq!((stats.runs, stats.proposals, stats.no_proposal, stats.rolled_out), (2, 1, 1, 1));
    assert_eq!(stats.approval_rate, Some(1.0));
    assert!((stats.cost_usd - 0.10).abs() < 1e-9);
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn invalid_changes_fail_and_leave_no_branch(pool: PgPool) {
    let (dir, f) = setup();
    let repo = f.path.parent().unwrap().parent().unwrap().to_owned();
    let data = dir.path().join("data");
    let store = EventStore::new(pool.clone());
    let reply = proposal_reply("x", &[]);
    let cases = [
        (Act::Edit { path: "README.md", content: "changed", reply: reply.clone() }, "outside factory/: README.md"),
        (Act::Edit { path: "factory/factory.yaml", content: "name: [broken", reply: reply.clone() }, "invalid after the change"),
        (Act::Edit { path: "factory/notes.txt", content: "unreferenced", reply: reply.clone() }, "factory_version unchanged"),
        (Act::Reply(reply.clone()), "edited no files"),
        (Act::Edit { path: "factory/agents/impl.md", content: "new", reply: "done!".into() }, "no JSON object"),
    ];
    let rt = FakeImprover::leak(Act::Reply(String::new()));
    let svc = service(&store, &f, &data, rt);
    for (act, needle) in cases {
        rt.set(act);
        let (id, r) = svc.run_now(human(), None).await.unwrap();
        let RunResult::Failed { message, .. } = r else { panic!("{needle}: {r:?}") };
        assert!(message.contains(needle), "expected {needle:?} in {message:?}");
        assert!(Command::new("git").arg("-C").arg(&repo).args(["rev-parse", "--verify", "--quiet", &format!("wh/proposal/{id}")])
            .output().unwrap().stdout.is_empty(), "branch deleted");
        let row = proposals::proposal(&pool, id.0).await.unwrap().unwrap();
        assert_eq!((row.status.as_str(), row.cost_usd), ("failed", 0.05));
    }
    assert_eq!(git(&repo, &["status", "--porcelain"]), "", "main checkout untouched");
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn triggers_request_once_per_version_and_consumer_runs_them(pool: PgPool) {
    let (dir, f) = setup();
    let data = dir.path().join("data");
    let store = EventStore::new(pool.clone());
    let rt = FakeImprover::leak(Act::Reply("{\"no_proposal\": \"nothing to fix\"}".into()));
    let mut svc = service(&store, &f, &data, rt);

    // Below min_sessions (3): nothing fires even at 100% failure.
    session(&store, &f, "implement", true, false).await;
    session(&store, &f, "implement", true, false).await;
    assert!(svc.tick().await.unwrap().is_empty());

    session(&store, &f, "implement", false, false).await;
    let fired = svc.tick().await.unwrap();
    assert_eq!(fired.len(), 1);
    assert!(fired[0].1.contains("stage_failure_rate for implement is 0.667 > 0.5 (n=3)"), "{}", fired[0].1);
    assert!(svc.tick().await.unwrap().is_empty(), "same trigger + version fires once");

    // The consumer runs the request exactly once, even when redelivered.
    let sub = Subscription::new(store.clone());
    sub.catch_up(&mut svc).await.unwrap();
    store.reset_checkpoint("improver:test").await.unwrap();
    sub.catch_up(&mut svc).await.unwrap();
    assert_eq!(rt.calls(), 1);
    wh_metrics::catch_up_all(&store).await.unwrap();
    let row = proposals::proposal(&pool, fired[0].0.0).await.unwrap().unwrap();
    assert_eq!((row.status.as_str(), row.message.as_deref()), ("no_proposal", Some("nothing to fix")));
    assert!(row.trigger.starts_with("trigger: stage_failure_rate"));

    // Another factory's request is ignored.
    let other = ProposalId::new();
    let req = ProposalEvent::ImprovementRequested { factory: "other".into(), trigger: "manual".into(), focus: None };
    append(&store, &other.stream_id(), other.0, None, req).await;
    sub.catch_up(&mut svc).await.unwrap();
    assert_eq!(rt.calls(), 1);
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn recovery_fails_interrupted_runs_and_returns_pending(pool: PgPool) {
    let (dir, f) = setup();
    let store = EventStore::new(pool.clone());
    let rt = FakeImprover::leak(Act::Reply("{\"no_proposal\": \"-\"}".into()));
    let svc = service(&store, &f, &dir.path().join("data"), rt);

    let pending = commands::request(&store, human(), "test", None).await.unwrap();
    let interrupted = commands::request(&store, human(), "test", None).await.unwrap();
    let started = ProposalEvent::ImprovementStarted { base_factory_version: f.version.clone(), window_days: 14, sessions_observed: 0 };
    append(&store, &interrupted.stream_id(), interrupted.0, None, started).await;

    let (failed, waiting) = svc.recover().await.unwrap();
    assert_eq!((failed, waiting), (1, vec![pending]));
    let types: Vec<_> =
        store.read_stream(&interrupted.stream_id()).await.unwrap().iter().map(|e| e.event_type.clone()).collect();
    assert_eq!(types, ["ImprovementRequested", "ImprovementStarted", "ImprovementFailed"]);
    assert_eq!(svc.run(interrupted).await.unwrap(), RunResult::NotPending);
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn detects_rollout_when_loaded_factory_has_the_proposed_version(pool: PgPool) {
    let (dir, f) = setup();
    let repo = f.path.parent().unwrap().parent().unwrap().to_owned();
    let data = dir.path().join("data");
    let store = EventStore::new(pool.clone());
    let rt = FakeImprover::leak(Act::Edit {
        path: "factory/agents/review.md",
        content: "ROLE: review with file:line",
        reply: proposal_reply("Cite file:line", &[]),
    });
    let (id, _) = service(&store, &f, &data, rt).run_now(human(), None).await.unwrap();
    commands::decide(&store, human(), id, true, None).await.unwrap();

    // Merged by hand instead of `apply`.
    git(&repo, &["merge", "-q", "--ff-only", &format!("wh/proposal/{id}")]);
    let reloaded = Factory::load(&f.path).unwrap();
    let svc = service(&store, &reloaded, &data, rt);
    assert_eq!(svc.detect_rollouts().await.unwrap(), vec![id]);
    let row = proposals::proposal(&pool, id.0).await.unwrap().unwrap();
    assert_eq!((row.status.as_str(), row.rolled_out_version), ("rolled_out", Some(reloaded.version.clone())));
    assert!(svc.detect_rollouts().await.unwrap().is_empty());
}
