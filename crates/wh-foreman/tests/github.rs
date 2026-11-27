//! Phase 8 end to end, without GitHub: a fake `gh` script answers from JSON files
//! and a bare git repo stands in for the remote.

use std::{path::Path, process::Command};

use sqlx::PgPool;
use tokio::sync::mpsc::UnboundedSender;
use wh_core::{
    Actor, EventMeta, NewEvent, TaskId,
    session::TokenUsage,
    task::{TaskEvent, TaskSource, TaskStatus},
};
use wh_eventstore::{EventStore, ExpectedVersion};
use wh_factory::Factory;
use wh_foreman::{Dispatch, Foreman};
use wh_github::{Gh, GithubService};
use wh_runner::{AgentRequest, AgentRuntime, Outcome, OutcomeStatus, Runner, RunnerConfig, Signal};

/// Implement appends a line (and records the feedback it got); review always approves.
struct Fake;

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
        if req.prompt.starts_with("ROLE: implement") && req.prompt.contains("## Merge conflicts") {
            let f = req.workdir.join("impl.txt");
            let text = std::fs::read_to_string(&f).unwrap();
            assert!(text.contains("<<<<<<< ") && text.contains("from main"), "{text}");
            std::fs::write(req.workdir.join("feedback.txt"), &req.prompt).unwrap();
            std::fs::write(f, "v1\nfrom main\n").unwrap();
            ok("resolved the conflict")
        } else if req.prompt.starts_with("ROLE: implement") {
            let f = req.workdir.join("impl.txt");
            let mut text = std::fs::read_to_string(&f).unwrap_or_default();
            text.push_str(if req.prompt.contains("Feedback you must address") { "fixed\n" } else { "v1\n" });
            std::fs::write(f, text).unwrap();
            if req.prompt.contains("Feedback you must address") {
                std::fs::write(req.workdir.join("feedback.txt"), &req.prompt).unwrap();
            }
            ok("implemented the greeting")
        } else {
            ok("good\nVERDICT: APPROVE")
        }
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

const FAKE_GH: &str = r#"#!/bin/sh
D="__DIR__"
echo "$*" >> "$D/calls.log"
case "$1 $2" in
  "pr list") cat "$D/pr_list.json" 2>/dev/null || echo '[]' ;;
  "pr create")
    if [ -f "$D/fail_create" ]; then echo "HTTP 502: bad gateway" >&2; exit 1; fi
    cat > "$D/pr_body.md"
    echo "Creating pull request..."
    echo "https://github.com/acme/demo/pull/7"
    echo '[{"number":7,"url":"https://github.com/acme/demo/pull/7"}]' > "$D/pr_list.json" ;;
  "pr view") cat "$D/pr_view.json" ;;
  "issue list") cat "$D/issues.json" 2>/dev/null || echo '[]' ;;
  "issue comment") ;;
  "run view") echo "test greet ... FAILED: expected hello" ;;
  "api "*)
    case "$2" in
      */reviews) cat "$D/reviews.json" ;;
      */comments) cat "$D/comments.json" ;;
    esac ;;
  *) echo "unexpected: $*" >&2; exit 1 ;;
esac
"#;

struct Env {
    _dir: tempfile::TempDir,
    gh_dir: std::path::PathBuf,
    remote: std::path::PathBuf,
    factory: Factory,
    data: std::path::PathBuf,
}

impl Env {
    fn write(&self, name: &str, json: serde_json::Value) {
        std::fs::write(self.gh_dir.join(name), json.to_string()).unwrap();
    }
    fn calls(&self) -> String {
        std::fs::read_to_string(self.gh_dir.join("calls.log")).unwrap_or_default()
    }
    fn gh(&self) -> Gh {
        Gh::new(self.gh_dir.join("gh"))
    }
}

fn setup(github: &str) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let remote = dir.path().join("remote.git");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(dir.path(), &["init", "-q", "--bare", "-b", "main", remote.to_str().unwrap()]);
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hi").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"]);
    git(&repo, &["remote", "add", "origin", remote.to_str().unwrap()]);
    git(&repo, &["push", "-q", "origin", "main"]);

    let gh_dir = dir.path().join("gh");
    std::fs::create_dir_all(&gh_dir).unwrap();
    let script = gh_dir.join("gh");
    std::fs::write(&script, FAKE_GH.replace("__DIR__", gh_dir.to_str().unwrap())).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let fdir = dir.path().join("factory");
    std::fs::create_dir_all(fdir.join("agents")).unwrap();
    for role in ["implement", "review"] {
        std::fs::write(fdir.join(format!("agents/{role}.md")), format!("ROLE: {role}")).unwrap();
    }
    std::fs::write(
        fdir.join("factory.yaml"),
        format!(
            r#"
name: gh-test
repo: ../repo
base_ref: main
max_rework_loops: 2
defaults: {{ max_retries: 0 }}
stages:
  - {{ id: implement, agent: implement }}
  - {{ id: review, agent: review, on_reject: implement }}
agents:
  implement: {{ prompt: agents/implement.md }}
  review: {{ prompt: agents/review.md }}
{github}
"#
        ),
    )
    .unwrap();
    let factory = Factory::load(&fdir.join("factory.yaml")).unwrap();
    let data = dir.path().join("data");
    Env { _dir: dir, gh_dir, remote, factory, data }
}

fn foreman(store: &EventStore, env: &Env) -> Foreman<Fake> {
    let runner = Runner::new(store.clone(), Fake, RunnerConfig { data_dir: env.data.clone() });
    Foreman::new(store.clone(), env.factory.clone(), runner, Dispatch::Inline).with_gh(env.gh())
}

async fn types(store: &EventStore, task: TaskId) -> Vec<String> {
    store.read_stream(&task.stream_id()).await.unwrap().into_iter().map(|e| e.event_type).collect()
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn issue_to_pr_to_rework_to_merge(pool: PgPool) {
    let env = setup("github: { repo: acme/demo, labels: [bot], issues: { comment: true } }");
    let store = EventStore::new(pool.clone());
    let svc = GithubService::new(store.clone(), env.factory.clone(), env.gh()).unwrap();

    // Intake: one task per labelled issue, however often we poll.
    env.write("issues.json", serde_json::json!([
        {"number": 12, "title": "Greet the user", "body": "Print hello", "url": "https://github.com/acme/demo/issues/12"}
    ]));
    let r = svc.sync_once().await.unwrap();
    assert_eq!(r.created.len(), 1, "{r:?}");
    let task = r.created[0].0;
    assert!(svc.sync_once().await.unwrap().created.is_empty());
    assert_eq!(task, TaskId::for_external("github:acme/demo#12"));
    assert!(env.calls().contains("issue comment 12 --repo acme/demo"));

    // Pipeline, then ship: branch pushed to the remote, PR created with Closes #12.
    let mut fm = foreman(&store, &env);
    let state = fm.drive(task).await.unwrap();
    assert_eq!(state.status, TaskStatus::Shipped);
    let pr = state.pr.clone().unwrap();
    assert_eq!((pr.number, pr.repo.as_str()), (7, "acme/demo"));
    let branch = wh_core::task::task_branch(task);
    assert_eq!(git(&env.remote, &["rev-parse", &branch]), pr.commit);
    let body = std::fs::read_to_string(env.gh_dir.join("pr_body.md")).unwrap();
    assert!(body.starts_with("Print hello\n\nCloses #12"), "{body}");
    assert!(body.contains("implemented the greeting"));
    assert!(env.calls().contains("--label bot"));

    // A human requests changes and CI fails in the same poll: one rework gets both.
    env.write("pr_view.json", serde_json::json!({
        "state": "OPEN", "headRefOid": pr.commit,
        "reviews": [{"id": "PRR_1", "author": {"login": "bob"}, "state": "CHANGES_REQUESTED",
                     "body": "Say hello properly", "submittedAt": "2026-11-25T10:00:00Z"}],
        "statusCheckRollup": [{"__typename": "CheckRun", "name": "test", "status": "COMPLETED", "conclusion": "FAILURE",
                               "detailsUrl": "https://github.com/acme/demo/actions/runs/55/job/66"}]
    }));
    env.write("reviews.json", serde_json::json!([{"id": 901, "node_id": "PRR_1"}]));
    env.write("comments.json", serde_json::json!([{"path": "impl.txt", "line": 1, "body": "lowercase please"}]));
    let r = svc.sync_once().await.unwrap();
    assert_eq!(r.recorded, vec![(task, vec!["PrReviewed", "PrChecksCompleted"])], "{r:?}");
    assert!(svc.sync_once().await.unwrap().recorded.is_empty(), "nothing new on a second poll");

    let state = fm.drive(task).await.unwrap();
    assert_eq!((state.status.clone(), state.pr_rework_loops), (TaskStatus::Shipped, 1));
    let new_commit = state.pr.clone().unwrap().commit;
    assert_ne!(new_commit, pr.commit);
    assert_eq!(git(&env.remote, &["rev-parse", &branch]), new_commit, "rework pushed to the same branch");
    let feedback = git(&env.remote, &["show", &format!("{branch}:feedback.txt")]);
    for needle in ["Say hello properly", "impl.txt:1: lowercase please", "### test", "FAILED: expected hello"] {
        assert!(feedback.contains(needle), "missing {needle:?} in feedback:\n{feedback}");
    }
    assert_eq!(env.calls().matches("pr create").count(), 1, "second ship reuses the PR");

    // Merge closes the loop.
    env.write("pr_view.json", serde_json::json!({
        "state": "MERGED", "headRefOid": new_commit, "mergedAt": "2026-11-25T12:00:00Z", "mergedBy": {"login": "bob"},
        "reviews": [{"id": "PRR_1", "author": {"login": "bob"}, "state": "CHANGES_REQUESTED", "body": "x"},
                    {"id": "PRR_2", "author": {"login": "bob"}, "state": "APPROVED", "body": "", "submittedAt": "2026-11-25T11:30:00Z"}],
        "statusCheckRollup": [{"__typename": "CheckRun", "name": "test", "status": "COMPLETED", "conclusion": "SUCCESS"}]
    }));
    let r = svc.sync_once().await.unwrap();
    assert_eq!(r.recorded, vec![(task, vec!["PrReviewed", "PrChecksCompleted", "PrMerged"])]);
    let state = fm.drive(task).await.unwrap();
    assert_eq!(state.status, TaskStatus::Done);
    assert_eq!(types(&store, task).await.last().unwrap(), "PrMerged");

    wh_metrics::catch_up_all(&store).await.unwrap();
    let row: (String, Option<String>, String, i32, i32, i32, i32, bool, bool) = sqlx::query_as(
        "SELECT status, pr_url, pr_state, pr_reviews, changes_requested, ci_failures, pr_rework_loops, \
         first_review_at IS NOT NULL, merged_at = completed_at FROM tasks_view WHERE task_id = $1",
    )
    .bind(task.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row, ("done".into(), Some("https://github.com/acme/demo/pull/7".into()), "merged".into(), 2, 1, 1, 1, true, true));
    let ct = wh_metrics::queries::cycle_times(&pool, 30).await.unwrap();
    assert_eq!((ct.shipped, ct.merged, ct.changes_requested_tasks), (1, 1, 1));
    assert!(ct.kickoff_to_merge_p50_ms.is_some());
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn ship_gate_then_failed_ship_blocks_and_resume_retries(pool: PgPool) {
    let env = setup("github: { repo: acme/demo, ship_gate: ship-review }");
    let store = EventStore::new(pool.clone());
    let task = TaskId::new();
    let created = TaskEvent::TaskCreated {
        title: "t".into(),
        body: None,
        source: TaskSource::Cli,
        factory: Some("gh-test".into()),
        benchmark: None,
        external_ref: None,
    };
    let append = |ev: TaskEvent, v: ExpectedVersion| {
        let store = store.clone();
        async move {
            let meta = EventMeta::new(task.0, Actor::Human { user: "tester".into() });
            store.append(&task.stream_id(), v, vec![NewEvent::from_domain(&ev, meta).unwrap()]).await.unwrap();
        }
    };
    append(created, ExpectedVersion::NoStream).await;
    let mut fm = foreman(&store, &env);
    let state = fm.drive(task).await.unwrap();
    assert_eq!(state.status, TaskStatus::AwaitingGate { gate: "ship-review".into(), stage: "ship".into() });
    assert!(git(&env.remote, &["branch", "--list", "wh/*"]).is_empty(), "nothing pushed before the gate");

    std::fs::write(env.gh_dir.join("fail_create"), "").unwrap();
    let decided = TaskEvent::GateDecided {
        gate: "ship-review".into(),
        stage: "ship".into(),
        approved: true,
        reviewer: "tester".into(),
        comment: None,
    };
    append(decided, ExpectedVersion::Exact(state.version)).await;
    let state = fm.drive(task).await.unwrap();
    let TaskStatus::Blocked { reason } = &state.status else { panic!("expected blocked, got {:?}", state.status) };
    assert!(reason.contains("ship failed [infra]") && reason.contains("bad gateway"), "{reason}");
    assert_eq!(state.blocked_stage.as_deref(), Some("ship"));

    std::fs::remove_file(env.gh_dir.join("fail_create")).unwrap();
    append(TaskEvent::TaskResumed { stage: None, comment: None }, ExpectedVersion::Exact(state.version)).await;
    let state = fm.drive(task).await.unwrap();
    assert_eq!(state.status, TaskStatus::Shipped);
    let t = types(&store, task).await;
    assert_eq!(t.iter().filter(|e| *e == "StageEntered").count(), 2, "resuming at ship does not re-run stages: {t:?}");
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn conflicting_pr_is_gated_merged_resolved_and_pushed_to_the_same_pr(pool: PgPool) {
    let env = setup("github: { repo: acme/demo, conflict_gate: conflict-review }");
    let store = EventStore::new(pool.clone());
    let task = TaskId::new();
    let created = TaskEvent::TaskCreated {
        title: "t".into(),
        body: None,
        source: TaskSource::Cli,
        factory: Some("gh-test".into()),
        benchmark: None,
        external_ref: None,
    };
    let meta = || EventMeta::new(task.0, Actor::Human { user: "tester".into() });
    store.append(&task.stream_id(), ExpectedVersion::NoStream, vec![NewEvent::from_domain(&created, meta()).unwrap()]).await.unwrap();
    let mut fm = foreman(&store, &env);
    let state = fm.drive(task).await.unwrap();
    assert_eq!(state.status, TaskStatus::Shipped);
    let pushed = state.pr.unwrap().commit;

    // Someone else adds the same file on main.
    let other = env._dir.path().join("other");
    git(env._dir.path(), &["clone", "-q", env.remote.to_str().unwrap(), other.to_str().unwrap()]);
    std::fs::write(other.join("impl.txt"), "from main\n").unwrap();
    git(&other, &["add", "."]);
    git(&other, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "main moves"]);
    git(&other, &["push", "-q", "origin", "main"]);
    let main = git(&env.remote, &["rev-parse", "main"]);

    env.write("pr_view.json", serde_json::json!({
        "state": "OPEN", "headRefOid": pushed, "baseRefName": "main",
        "mergeable": "CONFLICTING", "mergeStateStatus": "DIRTY"
    }));
    let svc = GithubService::new(store.clone(), env.factory.clone(), env.gh()).unwrap();
    assert_eq!(svc.sync_once().await.unwrap().recorded, vec![(task, vec!["PrConflicted"])]);
    assert!(svc.sync_once().await.unwrap().recorded.is_empty(), "once per head commit");

    let state = fm.drive(task).await.unwrap();
    let gate = TaskStatus::AwaitingGate { gate: "conflict-review".into(), stage: "conflict".into() };
    assert_eq!(state.status, gate);
    let approved = TaskEvent::GateDecided {
        gate: "conflict-review".into(),
        stage: "conflict".into(),
        approved: true,
        reviewer: "tester".into(),
        comment: None,
    };
    store
        .append(&task.stream_id(), ExpectedVersion::Exact(state.version), vec![NewEvent::from_domain(&approved, meta()).unwrap()])
        .await
        .unwrap();

    let state = fm.drive(task).await.unwrap();
    assert_eq!((state.status.clone(), state.pr_rework_loops, state.rework_loops), (TaskStatus::Shipped, 1, 0));
    assert!(state.conflict.is_none());
    let resolved = state.pr.unwrap().commit;
    let branch = wh_core::task::task_branch(task);
    assert_eq!(git(&env.remote, &["rev-parse", &branch]), resolved, "pushed to the same branch");
    // Both the old push and main are ancestors: a merge, no history rewritten.
    git(&env.remote, &["merge-base", "--is-ancestor", &pushed, &resolved]);
    git(&env.remote, &["merge-base", "--is-ancestor", &main, &resolved]);
    assert_eq!(git(&env.remote, &["show", &format!("{branch}:impl.txt")]), "v1\nfrom main");
    assert!(git(&env.remote, &["show", &format!("{branch}:feedback.txt")]).contains("impl.txt (both added)"));
    assert_eq!(env.calls().matches("pr create").count(), 1);
    let stages = state.results.iter().map(|r| r.stage.as_str()).collect::<Vec<_>>();
    assert_eq!(stages, ["implement", "review", "implement", "review"]);

    env.write("pr_view.json", serde_json::json!({"state": "MERGED", "headRefOid": resolved}));
    svc.sync_once().await.unwrap();
    assert_eq!(fm.drive(task).await.unwrap().status, TaskStatus::Done);
    wh_metrics::catch_up_all(&store).await.unwrap();
    let loops: (i32, i32) = sqlx::query_as("SELECT pr_rework_loops, rework_loops FROM tasks_view WHERE task_id = $1")
        .bind(task.0)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(loops, (1, 0));
}

/// A task driven until it waits on its PR, with the pushed commit.
async fn shipped_task(store: &EventStore, env: &Env) -> (TaskId, String) {
    let task = TaskId::new();
    let created = TaskEvent::TaskCreated {
        title: "t".into(),
        body: None,
        source: TaskSource::Cli,
        factory: Some("gh-test".into()),
        benchmark: None,
        external_ref: None,
    };
    let meta = EventMeta::new(task.0, Actor::Human { user: "tester".into() });
    store.append(&task.stream_id(), ExpectedVersion::NoStream, vec![NewEvent::from_domain(&created, meta).unwrap()]).await.unwrap();
    let state = foreman(store, env).drive(task).await.unwrap();
    assert_eq!(state.status, TaskStatus::Shipped);
    (task, state.pr.unwrap().commit)
}

fn conflicting_view(head: &str) -> serde_json::Value {
    serde_json::json!({"state": "OPEN", "headRefOid": head, "baseRefName": "main", "mergeable": "CONFLICTING"})
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn conflict_on_another_head_is_not_recorded(pool: PgPool) {
    let env = setup("github: { repo: acme/demo }");
    let store = EventStore::new(pool);
    let (task, pushed) = shipped_task(&store, &env).await;
    let svc = GithubService::new(store.clone(), env.factory.clone(), env.gh()).unwrap();

    env.write("pr_view.json", conflicting_view("someone-elses-commit"));
    assert!(svc.sync_once().await.unwrap().recorded.is_empty());
    assert!(!types(&store, task).await.contains(&"PrConflicted".to_owned()));

    env.write("pr_view.json", conflicting_view(&pushed));
    assert_eq!(svc.sync_once().await.unwrap().recorded, vec![(task, vec!["PrConflicted"])]);
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn conflict_while_the_task_is_not_shipped_is_not_recorded(pool: PgPool) {
    let env = setup("github: { repo: acme/demo }");
    let store = EventStore::new(pool);
    let (task, pushed) = shipped_task(&store, &env).await;
    let svc = GithubService::new(store.clone(), env.factory.clone(), env.gh()).unwrap();

    let version = store.read_stream(&task.stream_id()).await.unwrap().last().unwrap().stream_version;
    let entered = TaskEvent::StageEntered { stage: "implement".into(), feedback: None };
    let meta = EventMeta::new(task.0, Actor::Human { user: "tester".into() });
    store
        .append(&task.stream_id(), ExpectedVersion::Exact(version), vec![NewEvent::from_domain(&entered, meta).unwrap()])
        .await
        .unwrap();

    env.write("pr_view.json", conflicting_view(&pushed));
    assert!(svc.sync_once().await.unwrap().recorded.is_empty());
    assert!(!types(&store, task).await.contains(&"PrConflicted".to_owned()));
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn closed_pr_blocks_the_task(pool: PgPool) {
    let env = setup("github: { repo: acme/demo }");
    let store = EventStore::new(pool.clone());
    let task = TaskId::new();
    let created = TaskEvent::TaskCreated {
        title: "t".into(),
        body: None,
        source: TaskSource::Cli,
        factory: Some("gh-test".into()),
        benchmark: None,
        external_ref: None,
    };
    let meta = EventMeta::new(task.0, Actor::Human { user: "tester".into() });
    store.append(&task.stream_id(), ExpectedVersion::NoStream, vec![NewEvent::from_domain(&created, meta).unwrap()]).await.unwrap();
    let mut fm = foreman(&store, &env);
    assert_eq!(fm.drive(task).await.unwrap().status, TaskStatus::Shipped);
    env.write("pr_view.json", serde_json::json!({"state": "CLOSED", "headRefOid": "x"}));
    let svc = GithubService::new(store.clone(), env.factory.clone(), env.gh()).unwrap();
    svc.sync_once().await.unwrap();
    let state = fm.drive(task).await.unwrap();
    assert!(matches!(state.status, TaskStatus::Blocked { .. }));
    assert!(svc.sync_once().await.unwrap().recorded.is_empty(), "closed PRs are no longer polled");
}
