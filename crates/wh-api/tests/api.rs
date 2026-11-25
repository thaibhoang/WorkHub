use std::{path::PathBuf, sync::Arc, time::Duration};

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use wh_api::{AppState, Live};
use wh_core::{
    Actor, EventMeta, NewEvent, ProposalId, ScoreId, SessionId, TaskId,
    proposal::ProposalEvent,
    score::ScoreEvent,
    session::{SessionEvent, TokenUsage},
    task::{StageOutcome, TaskEvent},
};
use wh_eventstore::{EventStore, ExpectedVersion};
use wh_factory::Factory;

fn factory() -> Factory {
    Factory::load(std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../factory/factory.yaml"))).unwrap()
}

fn app(pool: &PgPool, factory: Option<Factory>) -> (Router, EventStore) {
    let store = EventStore::new(pool.clone());
    let state = AppState { store: store.clone(), factory: factory.map(Arc::new), live: Live::detached(0), bench_dir: PathBuf::from("benchmarks") };
    (wh_api::app(state, None), store)
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let req = Request::builder().method(method).uri(uri).header("content-type", "application/json");
    let req = req.body(body.map(|b| Body::from(b.to_string())).unwrap_or_default()).unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn append(store: &EventStore, stream: &str, ev: NewEvent) {
    store.append(stream, ExpectedVersion::Any, vec![ev]).await.unwrap();
}

fn sys(task: TaskId) -> EventMeta {
    EventMeta::new(task.0, Actor::System { component: "test".into() })
}

async fn task_ev(store: &EventStore, task: TaskId, e: TaskEvent) {
    append(store, &task.stream_id(), NewEvent::from_domain(&e, sys(task)).unwrap()).await;
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn create_list_and_show_task(pool: PgPool) {
    let (app, _) = app(&pool, Some(factory()));
    let (st, created) = call(&app, "POST", "/api/tasks", Some(json!({"title": "  Add SSE ", "body": "details"}))).await;
    assert_eq!(st, StatusCode::CREATED);
    assert_eq!(created["factory"], "workhub-dev");
    let id = created["task_id"].as_str().unwrap().to_owned();

    let (st, list) = call(&app, "GET", "/api/tasks", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(list[0]["title"], "Add SSE");
    assert_eq!(list[0]["status"], "open");
    assert_eq!(list[0]["source"], "web");

    let (_, open) = call(&app, "GET", "/api/tasks?status=open", None).await;
    assert_eq!(open.as_array().unwrap().len(), 1);
    let (_, done) = call(&app, "GET", "/api/tasks?status=done", None).await;
    assert!(done.as_array().unwrap().is_empty());

    let (st, detail) = call(&app, "GET", &format!("/api/tasks/{id}"), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(detail["state"]["status"], "open");
    assert_eq!(detail["state"]["body"], "details");
    assert_eq!(detail["events"][0]["type"], "TaskCreated");
    assert_eq!(detail["events"][0]["meta"]["actor"]["kind"], "human");

    let (st, _) = call(&app, "POST", "/api/tasks", Some(json!({"title": "   "}))).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    let (st, _) = call(&app, "GET", &format!("/api/tasks/{}", TaskId::new()), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = call(&app, "GET", "/api/tasks/not-a-uuid", None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn pipeline_task_needs_a_factory(pool: PgPool) {
    let (app, _) = app(&pool, None);
    let (st, _) = call(&app, "POST", "/api/tasks", Some(json!({"title": "x", "pipeline": true}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    // Default without a factory: a manual task.
    let (st, created) = call(&app, "POST", "/api/tasks", Some(json!({"title": "x"}))).await;
    assert_eq!(st, StatusCode::CREATED);
    assert_eq!(created["factory"], Value::Null);
    let (_, f) = call(&app, "GET", "/api/factory", None).await;
    assert_eq!(f, Value::Null);
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn gate_resume_and_cancel(pool: PgPool) {
    let (app, store) = app(&pool, Some(factory()));
    let (_, created) = call(&app, "POST", "/api/tasks", Some(json!({"title": "t"}))).await;
    let id = created["task_id"].as_str().unwrap().to_owned();
    let task: TaskId = id.parse().unwrap();

    // Not at a gate yet.
    let (st, err) = call(&app, "POST", &format!("/api/tasks/{id}/gate"), Some(json!({"approved": true}))).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{err}");

    task_ev(&store, task, TaskEvent::StageEntered { stage: "spec".into(), feedback: None }).await;
    task_ev(&store, task, TaskEvent::StageExited {
        stage: "spec".into(),
        outcome: StageOutcome::Succeeded,
        summary: Some("wrote docs/specs/x.md".into()),
        session: None,
    })
    .await;
    task_ev(&store, task, TaskEvent::GateRequested { gate: "spec-review".into(), stage: "spec".into() }).await;

    let (_, detail) = call(&app, "GET", &format!("/api/tasks/{id}"), None).await;
    assert_eq!(detail["state"]["status"], "awaiting_gate");
    assert_eq!(detail["state"]["gate"], "spec-review");
    assert_eq!(detail["state"]["results"][0]["summary"], "wrote docs/specs/x.md");

    // Rejecting needs a comment.
    let (st, _) = call(&app, "POST", &format!("/api/tasks/{id}/gate"), Some(json!({"approved": false}))).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    let (st, res) = call(&app, "POST", &format!("/api/tasks/{id}/gate"), Some(json!({"approved": true, "comment": "lgtm"}))).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(res["message"], "gate 'spec-review' approved");
    let (_, detail) = call(&app, "GET", &format!("/api/tasks/{id}"), None).await;
    let last = detail["events"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["type"], "GateDecided");
    assert_eq!(last["payload"]["approved"], true);
    assert_eq!(last["payload"]["comment"], "lgtm");

    // Resume only works on blocked tasks.
    let (st, _) = call(&app, "POST", &format!("/api/tasks/{id}/resume"), Some(json!({}))).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    task_ev(&store, task, TaskEvent::TaskBlocked { stage: Some("implement".into()), reason: "budget".into() }).await;
    let (st, _) = call(&app, "POST", &format!("/api/tasks/{id}/resume"), Some(json!({"comment": "try smaller"}))).await;
    assert_eq!(st, StatusCode::OK);

    let (st, _) = call(&app, "POST", &format!("/api/tasks/{id}/cancel"), Some(json!({"reason": "dup"}))).await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = call(&app, "POST", &format!("/api/tasks/{id}/cancel"), Some(json!({}))).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    let (_, list) = call(&app, "GET", "/api/tasks", None).await;
    assert_eq!(list[0]["status"], "cancelled");
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn sessions_stats_and_dashboard(pool: PgPool) {
    let (app, store) = app(&pool, Some(factory()));
    let (_, created) = call(&app, "POST", "/api/tasks", Some(json!({"title": "t"}))).await;
    let task: TaskId = created["task_id"].as_str().unwrap().parse().unwrap();

    let dir = tempfile_dir();
    let transcript = dir.join("s.jsonl");
    std::fs::write(&transcript, "{\"type\":\"system\"}\n\n{\"type\":\"result\",\"result\":\"done\"}\nnot json\n").unwrap();

    let session = SessionId::new();
    let s = session.stream_id();
    let evs = [
        SessionEvent::AgentDispatched {
            task_id: task,
            agent: "spec".into(),
            stage: "spec".into(),
            runtime: "fake".into(),
            model: Some("claude-haiku-5-5".into()),
            prompt: "p".into(),
            budget_usd: Some(1.0),
            timeout_secs: 60,
            attempt: 1,
            retry_of: None,
            benchmark: None,
        },
        SessionEvent::AgentToolCalled { tool: "Read".into(), ok: true, duration_ms: 10 },
        SessionEvent::AgentUsageReported { usage: TokenUsage { input_tokens: 10, ..Default::default() }, cost_usd: 0.25 },
        SessionEvent::AgentSessionCompleted {
            num_turns: 3,
            duration_ms: 1500,
            summary: Some("ok".into()),
            transcript: Some(transcript.display().to_string()),
            commit: None,
        },
    ];
    for e in &evs {
        append(&store, &s, NewEvent::from_domain(e, sys(task)).unwrap()).await;
    }
    wh_metrics::catch_up_all(&store).await.unwrap();

    let (_, list) = call(&app, "GET", "/api/sessions", None).await;
    assert_eq!(list[0]["agent"], "spec");
    assert_eq!(list[0]["cost_usd"], 0.25);
    let (_, one) = call(&app, "GET", &format!("/api/sessions/{session}"), None).await;
    assert_eq!(one["session"]["status"], "succeeded");
    assert_eq!(one["events"].as_array().unwrap().len(), 4);
    let (st, t) = call(&app, "GET", &format!("/api/sessions/{session}/transcript"), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(t["lines"].as_array().unwrap().len(), 2, "blank and non-JSON lines are skipped");
    assert_eq!(t["lines"][1]["result"], "done");

    let (_, tasks) = call(&app, "GET", "/api/tasks", None).await;
    assert_eq!(tasks[0]["sessions"], 1);
    assert_eq!(tasks[0]["cost_usd"], 0.25);
    let (_, detail) = call(&app, "GET", &format!("/api/tasks/{task}"), None).await;
    assert_eq!(detail["sessions"][0]["num_turns"], 3);

    let (st, stats) = call(&app, "GET", "/api/stats", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(stats["agents"][0]["runs"], 1);
    assert_eq!(stats["stages"][0]["stage"], "spec");

    let (st, dash) = call(&app, "GET", "/api/dashboard?days=7", None).await;
    assert_eq!(st, StatusCode::OK, "{dash}");
    assert_eq!(dash["daily"].as_array().unwrap().len(), 7);
    assert_eq!(dash["totals"]["tasks_created"], 1);
    assert_eq!(dash["totals"]["cost_usd"], 0.25);
    assert_eq!(dash["daily"][6]["sessions_succeeded"], 1);

    let (_, f) = call(&app, "GET", "/api/factory", None).await;
    assert_eq!(f["name"], "workhub-dev");
    assert_eq!(f["stages"][1]["gate"], "spec-review");
    assert_eq!(f["scorers"][0]["target"], "implementation");

    // A scorer graded the session: it shows up on the session, the task, /scores and /scorers.
    let score = ScoreId::for_session(session, "review-evidence");
    let evs = [
        ScoreEvent::ScoreStarted {
            scorer: "review-evidence".into(),
            target_session: session,
            task_id: task,
            agent: "spec".into(),
            stage: "spec".into(),
            model: Some("claude-haiku-5-5".into()),
            pass_threshold: 0.6,
            manual: false,
        },
        ScoreEvent::ScoreRecorded {
            score: 0.4,
            pass: false,
            classification: Some("vague_evidence".into()),
            rationale: "no file:line".into(),
            cost_usd: 0.003,
            usage: TokenUsage::default(),
            duration_ms: 900,
            transcript: None,
        },
    ];
    for e in &evs {
        append(&store, &score.stream_id(), NewEvent::from_domain(e, sys(task)).unwrap()).await;
    }
    wh_metrics::catch_up_all(&store).await.unwrap();

    let (_, one) = call(&app, "GET", &format!("/api/sessions/{session}"), None).await;
    assert_eq!(one["scores"][0]["classification"], "vague_evidence");
    let (_, detail) = call(&app, "GET", &format!("/api/tasks/{task}"), None).await;
    assert_eq!(detail["scores"][0]["pass"], false);
    let (_, failing) = call(&app, "GET", "/api/scores?failing=true&scorer=review-evidence", None).await;
    assert_eq!(failing[0]["task_title"], "t");
    let (_, none) = call(&app, "GET", "/api/scores?scorer=test-evidence", None).await;
    assert!(none.as_array().unwrap().is_empty());

    let (st, sc) = call(&app, "GET", "/api/scorers", None).await;
    assert_eq!(st, StatusCode::OK, "{sc}");
    let list = sc["scorers"].as_array().unwrap();
    assert_eq!(list.len(), 3, "every defined scorer is listed, run or not");
    let re = list.iter().find(|x| x["id"] == "review-evidence").unwrap();
    assert_eq!(re["definition"]["pass_threshold"], 0.6);
    assert_eq!((re["stats"]["scored"].as_i64(), re["stats"]["pass_rate"].as_f64()), (Some(1), Some(0.0)));
    assert!(list.iter().find(|x| x["id"] == "scope-discipline").unwrap()["stats"].is_null());
    assert_eq!(sc["by_version"][0]["scored"], 1);

    let (_, dash) = call(&app, "GET", "/api/dashboard?days=7", None).await;
    assert_eq!((dash["totals"]["scores"].as_i64(), dash["totals"]["scorer_cost_usd"].as_f64()), (Some(1), Some(0.003)));
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn proposals_request_review_and_improvement_overview(pool: PgPool) {
    let f = factory();
    let version = f.version.clone();
    let (app, store) = app(&pool, Some(f));

    // A human request queues an improver run for the server's factory.
    let (st, req) = call(&app, "POST", "/api/proposals", Some(json!({"focus": "review rejects"}))).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{req}");
    let requested = req["proposal_id"].as_str().unwrap().to_owned();
    let (_, inbox) = call(&app, "GET", "/api/proposals?status=inbox", None).await;
    assert!(inbox.as_array().unwrap().is_empty(), "queued runs are not in the inbox");
    let (_, all) = call(&app, "GET", "/api/proposals", None).await;
    assert_eq!((all[0]["status"].as_str(), all[0]["focus"].as_str()), (Some("requested"), Some("review rejects")));

    // An open proposal (as the improver would leave it).
    let id = ProposalId::new();
    let sys = || {
        let mut m = EventMeta::new(id.0, Actor::System { component: "improver".into() });
        m.factory_version = Some(version.clone());
        m
    };
    let evs = [
        ProposalEvent::ImprovementRequested { factory: "workhub-dev".into(), trigger: "schedule".into(), focus: None },
        ProposalEvent::ImprovementStarted { base_factory_version: version.clone(), window_days: 14, sessions_observed: 9 },
        ProposalEvent::ProposalCreated {
            title: "Tell implement to run cargo fmt".into(),
            rationale: "ci fails on fmt".into(),
            expected_impact: Some("fail 30% -> 5%".into()),
            evidence: vec![SessionId::new()],
            files: vec!["factory/agents/implementation.md".into()],
            diff: "+run cargo fmt".into(),
            branch: format!("wh/proposal/{id}"),
            commit: "abc".into(),
            proposed_factory_version: "ffffffffffff".into(),
            factory_repo: "/nonexistent".into(),
            factory_file: "factory/factory.yaml".into(),
            cost_usd: 0.4,
            usage: TokenUsage::default(),
            duration_ms: 1000,
            transcript: None,
        },
    ];
    for e in evs {
        append(&store, &id.stream_id(), NewEvent::from_domain(&e, sys()).unwrap()).await;
    }
    wh_metrics::catch_up_all(&store).await.unwrap();

    let (_, inbox) = call(&app, "GET", "/api/proposals?status=inbox", None).await;
    assert_eq!(inbox.as_array().unwrap().len(), 1);
    let (st, d) = call(&app, "GET", &format!("/api/proposals/{id}"), None).await;
    assert_eq!(st, StatusCode::OK, "{d}");
    assert_eq!(d["proposal"]["title"], "Tell implement to run cargo fmt");
    assert_eq!(d["events"].as_array().unwrap().len(), 3);
    assert!(d["evidence"].as_array().unwrap().is_empty(), "unknown sessions are skipped");

    // Review: reject needs a comment; apply needs approval; stale decisions fail.
    let uri = format!("/api/proposals/{id}/decide");
    let (st, _) = call(&app, "POST", &uri, Some(json!({"approved": false}))).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    let (st, _) = call(&app, "POST", &format!("/api/proposals/{id}/apply"), None).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    let (st, r) = call(&app, "POST", &uri, Some(json!({"approved": false, "comment": "fmt is in CI already"}))).await;
    assert_eq!(st, StatusCode::OK, "{r}");
    let (st, _) = call(&app, "POST", &uri, Some(json!({"approved": true}))).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    let (_, d) = call(&app, "GET", &format!("/api/proposals/{id}"), None).await;
    assert_eq!((d["proposal"]["status"].as_str(), d["proposal"]["decision_comment"].as_str()), (Some("rejected"), Some("fmt is in CI already")));

    let (st, _) = call(&app, "GET", "/api/proposals/not-a-uuid", None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = call(&app, "GET", &format!("/api/proposals/{}", ProposalId::new()), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    let (st, im) = call(&app, "GET", "/api/improvement", None).await;
    assert_eq!(st, StatusCode::OK, "{im}");
    assert_eq!(im["current_version"].as_str(), Some(version.as_str()));
    assert_eq!(im["config"]["triggers"].as_array().unwrap().len(), 3);
    assert_eq!((im["stats"]["proposals"].as_i64(), im["stats"]["rejected"].as_i64()), (Some(1), Some(1)));
    assert_eq!(im["stats"]["approval_rate"].as_f64(), Some(0.0));
    assert!(all.as_array().unwrap().iter().any(|p| p["proposal_id"] == requested.as_str()));

    // Without an improvement section the request is refused.
    let (app2, _) = self::app(&pool, None);
    let (st, _) = call(&app2, "POST", "/api/proposals", Some(json!({}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn sse_replays_after_cursor_and_filters_by_task(pool: PgPool) {
    let (app, store) = app(&pool, None);
    let (a, b) = (TaskId::new(), TaskId::new());
    let created = |t: &str| TaskEvent::TaskCreated {
        title: t.into(),
        body: None,
        source: wh_core::task::TaskSource::Cli,
        factory: None,
        benchmark: None,
    };
    task_ev(&store, a, created("a")).await;
    task_ev(&store, b, created("b")).await;
    task_ev(&store, a, TaskEvent::TaskCancelled { reason: "x".into() }).await;

    let req = Request::builder().uri(format!("/api/events/stream?after=0&task={a}")).body(Body::empty()).unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(res.headers()["content-type"].to_str().unwrap().starts_with("text/event-stream"));
    let mut body = res.into_body();
    let mut text = String::new();
    while text.matches("\n\n").count() < 2 {
        let frame = tokio::time::timeout(Duration::from_secs(5), body.frame()).await.unwrap().unwrap().unwrap();
        if let Some(d) = frame.data_ref() {
            text.push_str(std::str::from_utf8(d).unwrap());
        }
    }
    assert!(text.contains("id: 1\n") && text.contains("id: 3\n"), "{text}");
    assert!(!text.contains("id: 2\n"), "other task's event leaked: {text}");
    assert!(text.contains("\"type\":\"TaskCancelled\""));

    // Last-Event-ID resumes after the given seq.
    let req = Request::builder().uri("/api/events/stream").header("last-event-id", "2").body(Body::empty()).unwrap();
    let mut body = app.clone().oneshot(req).await.unwrap().into_body();
    let frame = tokio::time::timeout(Duration::from_secs(5), body.frame()).await.unwrap().unwrap().unwrap();
    let text = std::str::from_utf8(frame.data_ref().unwrap()).unwrap().to_owned();
    assert!(text.contains("id: 3\n") && !text.contains("id: 1\n"), "{text}");
}

fn tempfile_dir() -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("wh-api-test-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]
async fn benchmark_suites_queue_and_runs(pool: PgPool) {
    let dir = tempfile::tempdir().unwrap();
    let factory = concat!(env!("CARGO_MANIFEST_DIR"), "/../../factory/factory.yaml");
    std::fs::write(
        dir.path().join("smoke.yaml"),
        format!("name: smoke\nfactory: {factory}\ncases:\n  - {{ id: one, title: One, check: \"true\" }}\n"),
    )
    .unwrap();
    std::fs::write(dir.path().join("broken.yaml"), "name: broken\n").unwrap();
    let store = EventStore::new(pool.clone());
    let state = AppState { store: store.clone(), factory: None, live: Live::detached(0), bench_dir: dir.path().to_owned() };
    let app = wh_api::app(state, None);

    let (st, suites) = call(&app, "GET", "/api/benchmarks/suites", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(suites[0]["key"], "broken");
    assert!(suites[0]["error"].is_string());
    assert_eq!(suites[1]["name"], "smoke");
    assert_eq!(suites[1]["cases"][0]["repo"], "factory");

    let (st, _) = call(&app, "POST", "/api/benchmarks", Some(json!({"suite": "../etc/passwd"}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = call(&app, "POST", "/api/benchmarks", Some(json!({"suite": "nope"}))).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, err) = call(&app, "POST", "/api/benchmarks", Some(json!({"suite": "smoke", "cases": ["zz"]}))).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{err}");

    let (st, queued) = call(&app, "POST", "/api/benchmarks", Some(json!({"suite": "smoke", "repeat": 2, "git_ref": "HEAD"}))).await;
    assert_eq!(st, StatusCode::ACCEPTED);
    let id = queued["run_ids"][0].as_str().unwrap().to_owned();
    wh_metrics::catch_up_all(&store).await.unwrap();
    let (_, runs) = call(&app, "GET", "/api/benchmarks", None).await;
    assert_eq!((runs[0]["status"].as_str(), runs[0]["repeat"].as_i64()), (Some("requested"), Some(2)));
    let (st, detail) = call(&app, "GET", &format!("/api/benchmarks/{id}"), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(detail["events"][0]["type"], "BenchmarkRequested");
    assert_eq!(detail["run"]["git_ref"], "HEAD");
    let (st, cmp) = call(&app, "GET", &format!("/api/benchmarks/compare?a={id}&b={id}"), None).await;
    assert_eq!((st, cmp["comparable"].as_bool()), (StatusCode::OK, Some(true)));
    let (st, _) = call(&app, "GET", &format!("/api/benchmarks/{}", uuid::Uuid::now_v7()), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}
