use std::{
    path::Path,
    process::Command,
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};

use sqlx::PgPool;
use tokio::sync::mpsc::UnboundedSender;
use wh_core::{DockerSandbox, FailureClass, Sandbox, TaskId, session::{SessionEvent, TokenUsage}};
use wh_eventstore::EventStore;
use wh_metrics::sessions::SessionFactsProjection;
use wh_runner::{AgentRequest, AgentRuntime, Merge, Outcome, OutcomeStatus, RunSpec, Runner, RunnerConfig, Signal};

/// Runtime double: checks it runs inside the worktree, emits scripted signals.
struct Fake {
    outcome: Outcome,
}

impl AgentRuntime for Fake {
    fn name(&self) -> &str {
        "fake"
    }
    async fn run(&self, req: &AgentRequest, tx: UnboundedSender<Signal>) -> Outcome {
        assert!(req.workdir.join("README.md").exists(), "runtime must run inside the worktree");
        tx.send(Signal::Ready { runtime_session_id: Some("rt-1".into()), model: Some("m-1".into()) }).unwrap();
        tx.send(Signal::ToolCalled { tool: "Bash".into(), ok: true, duration_ms: 5 }).unwrap();
        tx.send(Signal::ToolCalled { tool: "Edit".into(), ok: false, duration_ms: 7 }).unwrap();
        self.outcome.clone()
    }
}

fn git_repo(dir: &Path) {
    let git = |args: &[&str]| {
        let ok = Command::new("git").arg("-C").arg(dir).args(args).status().unwrap().success();
        assert!(ok, "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("README.md"), "hi").unwrap();
    git(&["add", "."]);
    git(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"]);
}

fn spec(repo: &Path, task: TaskId, base_ref: &str) -> RunSpec {
    RunSpec {
        task_id: task,
        agent: "implementation".into(),
        stage: "implement".into(),
        prompt: "do it".into(),
        repo: repo.to_owned(),
        base_ref: base_ref.into(),
        model: None,
        budget_usd: Some(1.0),
        permission_mode: None,
        allowed_tools: vec![],
        tools: None,
        with_user_mcp: false,
        timeout: Duration::from_secs(60),
        keep_worktree: false,
        max_retries: 0,
        retry_backoff: Duration::ZERO,
        branch: None,
        commit_on_success: false,
        verdict_required: false,
        task_expected_version: None,
        feedback: None,
        factory_version: None,
        benchmark: None,
        sandbox: Default::default(),
        env: Default::default(),
        merge: None,
    }
}

/// Writes `shared.txt` (when given content), then succeeds.
struct Resolver(Option<&'static str>);

impl AgentRuntime for Resolver {
    fn name(&self) -> &str {
        "fake"
    }
    async fn run(&self, req: &AgentRequest, _tx: UnboundedSender<Signal>) -> Outcome {
        assert!(req.prompt.contains("`main`"), "the prompt describes the merge: {}", req.prompt);
        if let Some(text) = self.0 {
            std::fs::write(req.workdir.join("shared.txt"), text).unwrap();
        }
        success()
    }
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let id = ["-c", "user.email=t@t", "-c", "user.name=t"];
    let out = Command::new("git").arg("-C").arg(dir).args(id).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn commit_file(repo: &Path, name: &str, text: &str) {
    std::fs::write(repo.join(name), text).unwrap();
    git_out(repo, &["add", "."]);
    git_out(repo, &["commit", "-qm", name]);
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn merges_the_base_before_the_agent_and_commits_only_resolved_merges(pool: PgPool) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git_repo(&repo);
    // `task` and `main` change the same file; `clean` touches another one.
    for (branch, file) in [("task", "shared.txt"), ("clean", "other.txt")] {
        git_out(&repo, &["checkout", "-q", "-b", branch, "main"]);
        commit_file(&repo, file, "ours\n");
    }
    git_out(&repo, &["checkout", "-q", "main"]);
    commit_file(&repo, "shared.txt", "theirs\n");

    let store = EventStore::new(pool);
    let config = RunnerConfig { data_dir: dir.path().join("data") };
    let run = |branch: &'static str, content: Option<&'static str>| {
        let runner = Runner::new(store.clone(), Resolver(content), config.clone());
        let mut s = spec(&repo, TaskId::new(), "main");
        s.branch = Some(branch.into());
        s.commit_on_success = true;
        // The repo stands in as its own remote.
        s.merge = Some(Merge { remote: ".".into(), base: "main".into() });
        async move { runner.run(s, |_| {}).await.unwrap() }
    };
    let parents = |branch: &str| git_out(&repo, &["log", "-1", "--format=%P", branch]).split(' ').count();

    let before = git_out(&repo, &["rev-parse", "task"]);
    let report = run("task", None).await;
    assert!(
        matches!(&report.last().outcome.status, OutcomeStatus::Failed { message, retryable: false, .. } if message.contains("shared.txt")),
        "{:?}",
        report.last().outcome.status
    );
    assert_eq!(git_out(&repo, &["rev-parse", "task"]), before, "nothing committed with markers left");

    // Resolving to exactly our side leaves no diff, but the merge is still committed.
    let report = run("task", Some("ours\n")).await;
    assert_eq!(report.last().outcome.status, OutcomeStatus::Succeeded);
    assert_eq!(parents("task"), 2);
    assert_eq!(git_out(&repo, &["show", "task:shared.txt"]), "ours");

    run("clean", None).await;
    assert_eq!(parents("clean"), 2);
    assert_eq!(git_out(&repo, &["show", "clean:shared.txt"]), "theirs");
}

/// For setups that must fail before any agent runs.
struct NeverRuns;

impl AgentRuntime for NeverRuns {
    fn name(&self) -> &str {
        "fake"
    }
    async fn run(&self, _req: &AgentRequest, _tx: UnboundedSender<Signal>) -> Outcome {
        panic!("the runtime must not be called")
    }
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn failed_merge_setup_is_env_setup_and_not_retried(pool: PgPool) {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    git_repo(repo.path());
    git_out(repo.path(), &["branch", "task"]);
    let before = git_out(repo.path(), &["rev-parse", "task"]);
    let runner = Runner::new(EventStore::new(pool), NeverRuns, RunnerConfig { data_dir: data.path().into() });

    let mut s = spec(repo.path(), TaskId::new(), "HEAD");
    s.branch = Some("task".into());
    s.commit_on_success = true;
    s.merge = Some(Merge { remote: ".".into(), base: "no-such-branch".into() });
    s.max_retries = 2;
    let report = runner.run(s, |_| {}).await.unwrap();
    assert_eq!(report.attempts.len(), 1, "env_setup is not retried");
    match &report.last().outcome.status {
        OutcomeStatus::Failed { class, message, retryable } => {
            assert_eq!(*class, FailureClass::EnvSetup);
            assert!(message.contains("no-such-branch"), "{message}");
            assert!(!retryable);
        }
        other => panic!("expected failure, got {other:?}"),
    }
    assert!(std::fs::read_dir(data.path().join("worktrees")).unwrap().next().is_none(), "worktree cleaned up");
    assert_eq!(git_out(repo.path(), &["rev-parse", "task"]), before, "the task branch has not moved");
}

fn success() -> Outcome {
    Outcome {
        status: OutcomeStatus::Succeeded,
        usage: TokenUsage { input_tokens: 100, output_tokens: 50, cache_read_tokens: 1000, cache_write_tokens: 10 },
        cost_usd: 0.25,
        num_turns: 4,
        duration_ms: 3000,
        summary: Some("done".into()),
    }
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn records_full_session_and_feeds_session_facts(pool: PgPool) {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    git_repo(repo.path());
    let store = EventStore::new(pool.clone());
    let runner = Runner::new(store.clone(), Fake { outcome: success() }, RunnerConfig { data_dir: data.path().into() });

    let task = TaskId::new();
    let mut seen = 0;
    let report = runner.run(spec(repo.path(), task, "HEAD"), |_| seen += 1).await.unwrap();
    let report = report.last().clone();
    assert_eq!(seen, 3);
    assert!(report.workdir.is_none(), "worktree removed when keep_worktree = false");

    let types: Vec<String> = store
        .read_stream(&report.session_id.stream_id())
        .await
        .unwrap()
        .iter()
        .map(|e| e.event_type.clone())
        .collect();
    assert_eq!(types, [
        "AgentDispatched",
        "AgentSessionStarted",
        "AgentRuntimeReady",
        "AgentToolCalled",
        "AgentToolCalled",
        "AgentUsageReported",
        "AgentSessionCompleted"
    ]);
    let task_events = store.read_stream(&task.stream_id()).await.unwrap();
    assert_eq!(task_events.iter().map(|e| e.event_type.as_str()).collect::<Vec<_>>(), [
        "StageEntered",
        "StageExited"
    ]);
    // Every session event is causally chained to the previous one.
    let session = store.read_stream(&report.session_id.stream_id()).await.unwrap();
    assert!(session.iter().all(|e| e.meta.correlation_id == task.0 && e.meta.causation_id.is_some()));

    wh_metrics::catch_up_all(&store).await.unwrap();
    let rows = SessionFactsProjection::recent(&pool, 10).await.unwrap();
    let r = &rows[0];
    assert_eq!((r.status.as_str(), r.model.as_deref()), ("succeeded", Some("m-1")));
    assert_eq!((r.tool_calls, r.tool_errors, r.num_turns), (2, 1, Some(4)));
    assert_eq!((r.cost_usd, r.cache_read_tokens, r.duration_ms), (0.25, 1000, Some(3000)));
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn failures_are_classified_and_counted(pool: PgPool) {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    git_repo(repo.path());
    let store = EventStore::new(pool.clone());
    let cfg = RunnerConfig { data_dir: data.path().into() };

    Runner::new(store.clone(), Fake { outcome: success() }, cfg.clone())
        .run(spec(repo.path(), TaskId::new(), "HEAD"), |_| {})
        .await
        .unwrap();
    let mut timeout = Outcome::failed(FailureClass::Timeout, "too slow", true, Duration::from_secs(9));
    timeout.cost_usd = 0.75;
    Runner::new(store.clone(), Fake { outcome: timeout }, cfg.clone())
        .run(spec(repo.path(), TaskId::new(), "HEAD"), |_| {})
        .await
        .unwrap();
    // Bad base ref: the worktree cannot be created -> env_setup, runtime never called.
    let report = Runner::new(store.clone(), Fake { outcome: success() }, cfg)
        .run(spec(repo.path(), TaskId::new(), "no-such-ref"), |_| {})
        .await
        .unwrap()
        .last()
        .clone();
    assert!(matches!(report.outcome.status, OutcomeStatus::Failed { class: FailureClass::EnvSetup, .. }));
    let last = store.read_stream(&report.session_id.stream_id()).await.unwrap().pop().unwrap();
    assert!(matches!(last.decode::<SessionEvent>().unwrap(), SessionEvent::AgentSessionFailed { .. }));

    wh_metrics::catch_up_all(&store).await.unwrap();
    let stats = SessionFactsProjection::stats_by_agent(&pool).await.unwrap();
    assert_eq!((stats[0].runs, stats[0].succeeded, stats[0].failed), (3, 1, 2));
    assert!((stats[0].total_cost_usd - 1.0).abs() < 1e-9);
    let mut failures = SessionFactsProjection::failures_by_class(&pool).await.unwrap();
    failures.sort();
    assert_eq!(failures, vec![("env_setup".into(), 1), ("timeout".into(), 1)]);
}

/// Fails with `first` for the first `fail_times` calls, then succeeds.
struct Flaky {
    calls: AtomicU32,
    fail_times: u32,
    first: Outcome,
}

impl AgentRuntime for Flaky {
    fn name(&self) -> &str {
        "flaky"
    }
    async fn run(&self, _req: &AgentRequest, _tx: UnboundedSender<Signal>) -> Outcome {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n < self.fail_times { self.first.clone() } else { success() }
    }
}

fn flaky(fail_times: u32, class: FailureClass, retryable: bool) -> Flaky {
    let mut first = Outcome::failed(class, "boom", retryable, Duration::from_secs(1));
    first.cost_usd = 0.1;
    Flaky { calls: AtomicU32::new(0), fail_times, first }
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn retryable_failure_is_retried_and_linked(pool: PgPool) {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    git_repo(repo.path());
    let store = EventStore::new(pool.clone());
    let task = TaskId::new();
    let mut s = spec(repo.path(), task, "HEAD");
    s.max_retries = 2;

    let runner = Runner::new(store.clone(), flaky(1, FailureClass::Timeout, true), RunnerConfig {
        data_dir: data.path().into(),
    });
    let report = runner.run(s, |_| {}).await.unwrap();
    assert_eq!(report.attempts.len(), 2);
    assert_eq!(report.last().outcome.status, OutcomeStatus::Succeeded);
    assert!((report.total_cost_usd() - 0.35).abs() < 1e-9);

    let second = store.read_stream(&report.attempts[1].session_id.stream_id()).await.unwrap();
    match second[0].decode::<SessionEvent>().unwrap() {
        SessionEvent::AgentDispatched { attempt, retry_of, .. } => {
            assert_eq!(attempt, 2);
            assert_eq!(retry_of, Some(report.attempts[0].session_id));
        }
        other => panic!("unexpected {other:?}"),
    }
    // The task sees a single stage run, ending in success.
    let task_events = store.read_stream(&task.stream_id()).await.unwrap();
    assert_eq!(task_events.iter().map(|e| e.event_type.as_str()).collect::<Vec<_>>(), [
        "StageEntered",
        "StageExited"
    ]);

    wh_metrics::catch_up_all(&store).await.unwrap();
    let stats = SessionFactsProjection::stats_by_agent(&pool).await.unwrap();
    assert_eq!((stats[0].runs, stats[0].failed, stats[0].retries), (2, 1, 1));
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn retries_stop_at_max_and_skip_non_retryable(pool: PgPool) {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    git_repo(repo.path());
    let store = EventStore::new(pool.clone());
    let cfg = RunnerConfig { data_dir: data.path().into() };
    let mut s = spec(repo.path(), TaskId::new(), "HEAD");
    s.max_retries = 2;

    let always = Runner::new(store.clone(), flaky(u32::MAX, FailureClass::Unknown, true), cfg.clone());
    let report = always.run(s.clone(), |_| {}).await.unwrap();
    assert_eq!(report.attempts.len(), 3, "1 run + 2 retries");
    assert_eq!(report.attempts.iter().map(|a| a.attempt).collect::<Vec<_>>(), [1, 2, 3]);

    s.task_id = TaskId::new();
    let budget = Runner::new(store.clone(), flaky(u32::MAX, FailureClass::BudgetExceeded, false), cfg);
    assert_eq!(budget.run(s, |_| {}).await.unwrap().attempts.len(), 1, "non-retryable is not retried");
}

/// Runtime double for the docker sandbox: checks the container the runner prepared.
struct ContainerProbe;

impl AgentRuntime for ContainerProbe {
    fn name(&self) -> &str {
        "fake"
    }
    async fn run(&self, req: &AgentRequest, _tx: UnboundedSender<Signal>) -> Outcome {
        let c = req.container.as_ref().expect("docker sandbox must hand the runtime a container");
        let wd = req.workdir.display().to_string();
        assert_eq!(c.workdir, req.workdir);
        assert_eq!(c.mounts[0], format!("{wd}:{wd}:rw"), "worktree mounted at the same path");
        // Linked worktree: the main repo's git dir read-only, this worktree's git dir read-write.
        assert!(c.mounts[1].ends_with("/.git:ro"), "{:?}", c.mounts);
        assert!(c.mounts[2].contains("/.git/worktrees/") && c.mounts[2].ends_with(":rw"), "{:?}", c.mounts);
        assert!(c.mounts[3].ends_with(":/home/wh:rw"));
        assert!(c.home_dir.is_dir(), "home exists while the session runs");
        assert!(c.user.is_some());
        assert_eq!(c.name, format!("wh-{}", c.session));
        std::fs::write(req.workdir.join("out.txt"), "x").unwrap();
        success()
    }
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn docker_sandbox_prepares_container_and_records_it(pool: PgPool) {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    git_repo(repo.path());
    let store = EventStore::new(pool.clone());
    let runner = Runner::new(store.clone(), ContainerProbe, RunnerConfig { data_dir: data.path().into() });

    let mut s = spec(repo.path(), TaskId::new(), "HEAD");
    s.sandbox = Sandbox::Docker(DockerSandbox::new("img:test"));
    s.commit_on_success = true;
    let report = runner.run(s, |_| {}).await.unwrap();
    let last = report.last();
    assert_eq!(last.outcome.status, OutcomeStatus::Succeeded);
    // The host commits what the agent wrote in the container.
    let started = store.read_stream(&last.session_id.stream_id()).await.unwrap();
    let sandbox = started.iter().find_map(|e| match e.decode::<SessionEvent>() {
        Ok(SessionEvent::AgentSessionStarted { sandbox, .. }) => sandbox,
        _ => None,
    });
    assert_eq!(sandbox, Some(Sandbox::Docker(DockerSandbox::new("img:test"))));
    let completed = started.iter().find_map(|e| match e.decode::<SessionEvent>() {
        Ok(SessionEvent::AgentSessionCompleted { commit, .. }) => commit,
        _ => None,
    });
    assert!(completed.is_some(), "changes made in the container are committed");
    assert!(!data.path().join("sandbox-home").join(last.session_id.to_string()).exists(), "home removed");

    wh_metrics::catch_up_all(&store).await.unwrap();
    let row = wh_metrics::queries::session(&pool, last.session_id.0).await.unwrap().unwrap();
    assert_eq!(row.sandbox.as_deref(), Some("docker:img:test"));
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn missing_volume_is_env_setup_and_not_retried(pool: PgPool) {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    git_repo(repo.path());
    let store = EventStore::new(pool.clone());
    let runner = Runner::new(store.clone(), ContainerProbe, RunnerConfig { data_dir: data.path().into() });

    let mut d = DockerSandbox::new("img:test");
    d.volumes = vec!["/definitely/not/here:/cache".into()];
    let mut s = spec(repo.path(), TaskId::new(), "HEAD");
    s.sandbox = Sandbox::Docker(d);
    s.max_retries = 2;
    let report = runner.run(s, |_| {}).await.unwrap();
    assert_eq!(report.attempts.len(), 1, "env_setup is not retried");
    match &report.last().outcome.status {
        OutcomeStatus::Failed { class, message, retryable } => {
            assert_eq!(*class, FailureClass::EnvSetup);
            assert!(message.contains("/definitely/not/here"), "{message}");
            assert!(!retryable);
        }
        other => panic!("expected failure, got {other:?}"),
    }
    assert!(std::fs::read_dir(data.path().join("worktrees")).unwrap().next().is_none(), "worktree cleaned up");
}
