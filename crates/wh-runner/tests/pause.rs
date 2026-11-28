//! Pausing a stage on `QUESTION:` lines and resuming it.

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};

use sqlx::PgPool;
use tokio::sync::mpsc::UnboundedSender;
use wh_core::{
    DockerSandbox, FailureClass, Sandbox, TaskId,
    session::{SessionEvent, TokenUsage},
    task::{StageOutcome, TaskEvent},
};
use wh_eventstore::EventStore;
use wh_runner::{
    AgentRequest, AgentRuntime, Merge, Outcome, OutcomeStatus, Paused, Resumed, RunReport, RunSpec, Runner,
    RunnerConfig, SessionMode, Signal,
};

const ASK: &str = "Need input.\nQUESTION: {\"text\": \"Which?\", \"options\": [\"A\", \"B\"]}";

/// Plays the queued outcomes in order, writing one new file per turn.
#[derive(Clone, Default)]
struct Script {
    outcomes: Arc<Mutex<VecDeque<Outcome>>>,
    seen: Arc<Mutex<Vec<AgentRequest>>>,
}

impl AgentRuntime for Script {
    fn name(&self) -> &str {
        "fake"
    }
    async fn run(&self, req: &AgentRequest, tx: UnboundedSender<Signal>) -> Outcome {
        let turn = {
            let mut seen = self.seen.lock().unwrap();
            seen.push(req.clone());
            seen.len()
        };
        tx.send(Signal::Ready { runtime_session_id: Some(format!("rt-{turn}")), model: None }).unwrap();
        std::fs::write(req.workdir.join(format!("turn-{turn}.txt")), "x").unwrap();
        if let Some(c) = &req.container {
            std::fs::create_dir_all(c.home_dir.join(".claude")).unwrap();
            std::fs::write(c.home_dir.join(".claude/.credentials.json"), "secret").unwrap();
            std::fs::write(c.home_dir.join(".claude/conversation"), "history").unwrap();
        }
        self.outcomes.lock().unwrap().pop_front().expect("an unexpected turn")
    }
}

fn says(text: &str) -> Outcome {
    Outcome {
        status: OutcomeStatus::Succeeded,
        usage: TokenUsage::default(),
        cost_usd: 0.25,
        num_turns: 2,
        duration_ms: 10,
        summary: Some(text.into()),
    }
}

fn fails(class: FailureClass, retryable: bool) -> Outcome {
    Outcome::failed(class, "boom", retryable, Duration::ZERO)
}

struct Env {
    _dir: tempfile::TempDir,
    repo: PathBuf,
    data: PathBuf,
    store: EventStore,
    script: Script,
    runner: Runner<Script>,
}

fn env(pool: PgPool, outcomes: impl IntoIterator<Item = Outcome>) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let (repo, data) = (dir.path().join("repo"), dir.path().join("data"));
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    commit_file(&repo, "README.md", "hi");
    let script = Script::default();
    script.outcomes.lock().unwrap().extend(outcomes);
    let store = EventStore::new(pool);
    let runner = Runner::new(store.clone(), script.clone(), RunnerConfig { data_dir: data.clone() });
    Env { _dir: dir, repo, data, store, script, runner }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let id = ["-c", "user.email=t@t", "-c", "user.name=t"];
    let out = Command::new("git").arg("-C").arg(dir).args(id).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn commit_file(repo: &Path, name: &str, text: &str) {
    std::fs::write(repo.join(name), text).unwrap();
    git(repo, &["add", "."]);
    git(repo, &["commit", "-qm", name]);
}

impl Env {
    fn spec(&self, task: TaskId) -> RunSpec {
        RunSpec {
            task_id: task,
            agent: "implementation".into(),
            stage: "implement".into(),
            prompt: "do it".into(),
            repo: self.repo.clone(),
            base_ref: "main".into(),
            model: None,
            budget_usd: Some(1.0),
            permission_mode: None,
            allowed_tools: vec![],
            tools: None,
            with_user_mcp: false,
            timeout: Duration::from_secs(60),
            keep_worktree: false,
            max_retries: 2,
            retry_backoff: Duration::ZERO,
            branch: Some("work".into()),
            commit_on_success: true,
            verdict_required: false,
            task_expected_version: None,
            feedback: None,
            factory_version: None,
            benchmark: None,
            sandbox: Default::default(),
            env: Default::default(),
            merge: None,
            pause_on_question: true,
        }
    }

    async fn pause(&self, spec: RunSpec) -> Paused {
        let report = self.runner.run(spec, |_| {}).await.unwrap();
        assert_eq!(report.stage_outcome, None);
        report.paused.expect("paused")
    }

    async fn resume(&self, spec: RunSpec, paused: &Paused, answer: &str) -> RunReport {
        match self.runner.resume(spec, paused, answer, |_| {}).await.unwrap() {
            Resumed::Ran(report) => report,
            other => panic!("expected a run, got {other:?}"),
        }
    }

    fn worktree(&self, p: &Paused) -> PathBuf {
        self.data.join("worktrees").join(p.home_session.to_string())
    }

    fn home(&self, p: &Paused) -> PathBuf {
        self.data.join("sandbox-home").join(p.home_session.to_string())
    }

    fn commits(&self) -> usize {
        git(&self.repo, &["rev-list", "--count", "main..work"]).parse().unwrap()
    }

    /// (`StageEntered`, `StageExited` outcomes) on the task stream.
    async fn stages(&self, task: TaskId) -> (usize, Vec<StageOutcome>) {
        let events = self.store.read_stream(&task.stream_id()).await.unwrap();
        let events: Vec<_> = events.iter().map(|e| e.decode::<TaskEvent>().unwrap()).collect();
        let entered = events.iter().filter(|e| matches!(e, TaskEvent::StageEntered { .. })).count();
        let exited = events.iter().filter_map(|e| match e {
            TaskEvent::StageExited { outcome, .. } => Some(*outcome),
            _ => None,
        });
        (entered, exited.collect())
    }
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn pause_keeps_the_worktree_and_leaves_the_stage_open(pool: PgPool) {
    let env = env(pool, [says(ASK)]);
    let task = TaskId::new();
    let mut s = env.spec(task);
    s.verdict_required = true; // asking wins over the missing verdict
    let paused = env.pause(s).await;

    assert_eq!(paused.session, paused.home_session);
    assert_eq!(paused.runtime_session_id, "rt-1");
    assert_eq!(paused.branch, "work");
    assert_eq!(paused.questions[0].text, "Which?");
    assert!(env.worktree(&paused).join("turn-1.txt").exists(), "uncommitted work is kept");
    assert_eq!(env.commits(), 0);
    assert_eq!(env.stages(task).await, (1, vec![]));
    assert_eq!(env.script.seen.lock().unwrap()[0].session, SessionMode::Persist);
    let events = env.store.read_stream(&paused.session.stream_id()).await.unwrap();
    assert!(matches!(
        events.last().unwrap().decode::<SessionEvent>().unwrap(),
        SessionEvent::AgentSessionCompleted { commit: None, .. }
    ));
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn no_pause_when_the_flag_is_off(pool: PgPool) {
    let env = env(pool, [says(ASK)]);
    let task = TaskId::new();
    let mut s = env.spec(task);
    s.pause_on_question = false;
    let report = env.runner.run(s, |_| {}).await.unwrap();

    assert!(report.paused.is_none());
    assert_eq!(report.stage_outcome, Some(StageOutcome::Succeeded));
    assert_eq!(env.commits(), 1);
    assert_eq!(env.stages(task).await, (1, vec![StageOutcome::Succeeded]));
    assert!(report.last().workdir.is_none());
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn resume_continues_in_place_and_commits_when_the_stage_ends(pool: PgPool) {
    let env = env(pool, [says(ASK), says(ASK), says("done")]);
    let task = TaskId::new();
    let first = env.pause(env.spec(task)).await;

    let again = env.resume(env.spec(task), &first, "-x").await.paused.expect("asks again");
    assert_eq!(again.home_session, first.home_session);
    assert_ne!(again.session, first.session);
    assert_eq!(again.runtime_session_id, "rt-2");
    assert_eq!((env.commits(), env.stages(task).await), (0, (1, vec![])));

    let report = env.resume(env.spec(task), &again, "B").await;
    assert_eq!(report.stage_outcome, Some(StageOutcome::Succeeded));
    assert_eq!(env.commits(), 1);
    assert_eq!(env.stages(task).await, (1, vec![StageOutcome::Succeeded]));
    assert!(!env.worktree(&first).exists(), "cleaned up");
    let files = git(&env.repo, &["ls-tree", "--name-only", "work"]);
    assert!(files.contains("turn-1.txt") && files.contains("turn-3.txt"), "{files}");

    let seen = env.script.seen.lock().unwrap().clone();
    assert_eq!(seen[1].session, SessionMode::Resume("rt-1".into()));
    assert_eq!(seen[2].session, SessionMode::Resume("rt-2".into()));
    assert_eq!(seen[1].workdir, seen[0].workdir);
    assert_eq!(seen[1].prompt, "The user answered your question(s):\n\n-x");
    assert_eq!((seen[1].timeout, seen[1].budget_usd), (Duration::from_secs(60), Some(1.0)));

    let events = env.store.read_stream(&again.session.stream_id()).await.unwrap();
    assert!(matches!(
        events[0].decode::<SessionEvent>().unwrap(),
        SessionEvent::AgentDispatched { resume_of: Some(of), retry_of: None, attempt: 1, .. } if of == first.session
    ));
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn resume_is_never_retried_and_a_failure_ends_the_stage(pool: PgPool) {
    let env = env(pool, [says(ASK), fails(FailureClass::Unknown, true)]);
    let task = TaskId::new();
    let paused = env.pause(env.spec(task)).await;
    let report = env.resume(env.spec(task), &paused, "A").await;

    assert_eq!(report.attempts.len(), 1);
    assert_eq!(report.stage_outcome, Some(StageOutcome::Failed));
    assert_eq!(env.script.seen.lock().unwrap().len(), 2);
    assert_eq!(env.stages(task).await, (1, vec![StageOutcome::Failed]));
    assert!(!env.worktree(&paused).exists());
    assert_eq!(env.commits(), 0);
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn resume_still_refuses_conflict_markers(pool: PgPool) {
    let env = env(pool, [says(ASK), says("done")]);
    git(&env.repo, &["checkout", "-q", "-b", "work"]);
    commit_file(&env.repo, "shared.txt", "ours\n");
    git(&env.repo, &["checkout", "-q", "main"]);
    commit_file(&env.repo, "shared.txt", "theirs\n");
    let before = git(&env.repo, &["rev-parse", "work"]);
    let task = TaskId::new();
    let mut s = env.spec(task);
    s.merge = Some(Merge { remote: ".".into(), base: "main".into() });
    let paused = env.pause(s.clone()).await;
    assert_eq!(paused.conflicts, ["shared.txt"]);

    let report = env.resume(s, &paused, "A").await;
    assert!(
        matches!(&report.last().outcome.status, OutcomeStatus::Failed { message, .. } if message.contains("shared.txt")),
        "{:?}",
        report.last().outcome.status
    );
    assert_eq!(git(&env.repo, &["rev-parse", "work"]), before);
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn resume_lost_without_worktree_or_home(pool: PgPool) {
    let env = env(pool, [says(ASK)]);
    let task = TaskId::new();
    let mut s = env.spec(task);
    s.sandbox = Sandbox::Docker(DockerSandbox::new("img:test"));
    let paused = env.pause(s.clone()).await;
    let lost = async |spec: RunSpec| {
        let resumed = env.runner.resume(spec, &paused, "A", |_| {}).await.unwrap();
        assert!(matches!(resumed, Resumed::Lost { session: None, .. }), "{resumed:?}");
    };

    std::fs::remove_dir_all(env.home(&paused)).unwrap();
    lost(s.clone()).await;
    assert!(!env.home(&paused).exists(), "not re-created");
    assert!(env.worktree(&paused).exists(), "left alone");

    std::fs::remove_dir_all(env.worktree(&paused)).unwrap();
    s.sandbox = Sandbox::default();
    lost(s).await;

    assert_eq!(env.script.seen.lock().unwrap().len(), 1, "the agent never ran again");
    assert_eq!(env.stages(task).await, (1, vec![]));
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn resume_lost_when_the_conversation_is_gone(pool: PgPool) {
    let env = env(pool, [says(ASK), fails(FailureClass::SessionLost, false)]);
    let task = TaskId::new();
    let paused = env.pause(env.spec(task)).await;
    let resumed = env.runner.resume(env.spec(task), &paused, "A", |_| {}).await.unwrap();

    let Resumed::Lost { session: Some(session), .. } = resumed else { panic!("{resumed:?}") };
    let events = env.store.read_stream(&session.stream_id()).await.unwrap();
    assert!(matches!(
        events.last().unwrap().decode::<SessionEvent>().unwrap(),
        SessionEvent::AgentSessionFailed { failure_class: FailureClass::SessionLost, .. }
    ));
    assert_eq!(env.script.seen.lock().unwrap().len(), 2, "no retry");
    assert_eq!(env.stages(task).await, (1, vec![]));
    assert!(env.worktree(&paused).join("turn-1.txt").exists(), "left alone");
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn docker_resume_uses_a_new_container_over_the_first_home(pool: PgPool) {
    let env = env(pool, [says(ASK), says(ASK), says("done")]);
    let task = TaskId::new();
    let mut s = env.spec(task);
    s.sandbox = Sandbox::Docker(DockerSandbox::new("img:test"));
    let first = env.pause(s.clone()).await;
    let home = env.home(&first);
    assert!(home.join(".claude/conversation").exists(), "home kept");
    assert!(!home.join(".claude/.credentials.json").exists(), "credentials removed while paused");

    let again = env.resume(s.clone(), &first, "A").await.paused.expect("asks again");
    env.resume(s, &again, "B").await;
    assert!(!home.exists(), "removed when the stage ends");

    let seen = env.script.seen.lock().unwrap().clone();
    let containers: Vec<_> = seen.iter().map(|r| r.container.clone().unwrap()).collect();
    let mount = format!("{}:/home/wh:rw", home.display());
    for c in &containers {
        assert!(c.mounts.contains(&mount), "{:?}", c.mounts);
    }
    assert_ne!(containers[0].name, containers[1].name);
    assert_ne!(containers[1].name, containers[2].name);
}
