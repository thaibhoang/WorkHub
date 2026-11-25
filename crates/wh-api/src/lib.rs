//! `wh-api`: HTTP API (axum) over the event store and read models, plus an SSE
//! feed of the live event log. Writes go through [`commands`] (one event per
//! command); reads come from projections, except task/session event timelines
//! which are read straight from their streams.

pub mod auth;
mod bench;
pub mod commands;
pub mod dto;
mod live;

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{
        IntoResponse, Response,
        sse::{KeepAlive, Sse},
    },
    middleware::from_fn_with_state,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;
use wh_core::{ProposalId, SessionId, TaskId, task::TaskSource};
use wh_eventstore::EventStore;
use wh_factory::Factory;
use wh_metrics::{proposals, queries, scores, sessions::SessionFactsProjection};

pub use live::Live;

use auth::CurrentUser;
use commands::{CommandError, NewTask};
use dto::*;

#[derive(Clone)]
pub struct AppState {
    pub store: EventStore,
    /// Factory driving new tasks created from the UI (if the server was given one).
    pub factory: Option<Arc<Factory>>,
    pub live: Live,
    /// Directory of benchmark suites runnable from the UI.
    pub bench_dir: PathBuf,
    /// Login settings (`wh serve --auth`); disabled by default.
    pub auth: auth::AuthConfig,
}

impl AppState {
    fn pool(&self) -> &PgPool {
        self.store.pool()
    }
}

/// All `/api` routes.
pub fn api(state: AppState) -> Router {
    let public = Router::new()
        .route("/health", get(|| async { Json(serde_json::json!({ "ok": true })) }))
        .route("/auth/me", get(auth::me))
        .route("/auth/login", post(auth::login))
        .route("/auth/logout", post(auth::logout));
    let protected = Router::new()
        .route("/tasks", get(list_tasks).post(create_task))
        .route("/tasks/{id}", get(get_task))
        .route("/tasks/{id}/gate", post(decide_gate))
        .route("/tasks/{id}/resume", post(resume_task))
        .route("/tasks/{id}/cancel", post(cancel_task))
        .route("/sessions", get(list_sessions))
        .route("/sessions/{id}", get(get_session))
        .route("/sessions/{id}/transcript", get(get_transcript))
        .route("/scorers", get(list_scorers))
        .route("/scores", get(list_scores))
        .route("/proposals", get(list_proposals).post(request_improvement))
        .route("/proposals/{id}", get(get_proposal))
        .route("/proposals/{id}/decide", post(decide_proposal))
        .route("/proposals/{id}/apply", post(apply_proposal))
        .route("/improvement", get(improvement))
        .route("/benchmarks", get(bench::runs).post(bench::request))
        .route("/benchmarks/suites", get(bench::suites))
        .route("/benchmarks/compare", get(bench::compare))
        .route("/benchmarks/{id}", get(bench::run))
        .route("/stats", get(stats))
        .route("/dashboard", get(dashboard))
        .route("/factory", get(factory))
        .route("/events", get(list_events))
        .route("/events/stream", get(stream_events))
        .route_layer(from_fn_with_state(state.clone(), auth::require_user));
    public.merge(protected).layer(from_fn_with_state(state.clone(), auth::check_origin)).with_state(state)
}

/// The full app: `/api/*` plus, if given, the built UI (SPA fallback to index.html).
pub fn app(state: AppState, web_dir: Option<PathBuf>) -> Router {
    let router = Router::new().nest("/api", api(state));
    let router = match web_dir {
        Some(dir) => {
            let index = dir.join("index.html");
            router.fallback_service(tower_http::services::ServeDir::new(dir).fallback(
                tower_http::services::ServeFile::new(index),
            ))
        }
        None => router,
    };
    router.layer(tower_http::trace::TraceLayer::new_for_http())
}

/// Serve until `shutdown` resolves.
pub async fn serve(
    addr: SocketAddr,
    state: AppState,
    web_dir: Option<PathBuf>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("api listening on http://{}", listener.local_addr()?);
    let app = app(state, web_dir).into_make_service_with_connect_info::<SocketAddr>();
    axum::serve(listener, app).with_graceful_shutdown(shutdown).await?;
    Ok(())
}

// ---------- errors ----------

pub struct ApiError(StatusCode, String);

impl ApiError {
    fn not_found(what: impl std::fmt::Display) -> Self {
        Self(StatusCode::NOT_FOUND, format!("{what} not found"))
    }
    fn bad_request(msg: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, msg.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if self.0.is_server_error() {
            tracing::error!("api error: {}", self.1);
        }
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

impl From<wh_eventstore::StoreError> for ApiError {
    fn from(e: wh_eventstore::StoreError) -> Self {
        anyhow::Error::from(e).into()
    }
}

impl From<CommandError> for ApiError {
    fn from(e: CommandError) -> Self {
        let code = match &e {
            CommandError::NotFound(_) => StatusCode::NOT_FOUND,
            CommandError::InvalidState(_) => StatusCode::UNPROCESSABLE_ENTITY,
            CommandError::Conflict => StatusCode::CONFLICT,
            CommandError::Other(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self(code, format!("{e:#}"))
    }
}

impl From<wh_improver::commands::CommandError> for ApiError {
    fn from(e: wh_improver::commands::CommandError) -> Self {
        use wh_improver::commands::CommandError as E;
        let code = match &e {
            E::NotFound(_) => StatusCode::NOT_FOUND,
            E::InvalidState(_) => StatusCode::UNPROCESSABLE_ENTITY,
            E::Conflict => StatusCode::CONFLICT,
            E::Other(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self(code, format!("{e:#}"))
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

fn parse_id<T: std::str::FromStr>(raw: &str, what: &str) -> Result<T, ApiError> {
    raw.parse().map_err(|_| ApiError::bad_request(format!("invalid {what} id: {raw}")))
}

// ---------- tasks ----------

#[derive(Deserialize)]
struct TaskListQuery {
    status: Option<String>,
    limit: Option<i64>,
}

async fn list_tasks(State(s): State<AppState>, Query(q): Query<TaskListQuery>) -> ApiResult<Vec<queries::TaskSummary>> {
    let status = q.status.as_deref().filter(|s| !s.is_empty() && *s != "all");
    Ok(Json(queries::tasks(s.pool(), status, q.limit.unwrap_or(200).clamp(1, 1000)).await?))
}

#[derive(Serialize)]
struct Created {
    task_id: TaskId,
    factory: Option<String>,
}

async fn create_task(
    State(s): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Json(b): Json<CreateTaskBody>,
) -> Result<(StatusCode, Json<Created>), ApiError> {
    let factory = match (b.pipeline.unwrap_or(true), &s.factory) {
        (true, Some(f)) => Some(f.name.clone()),
        (true, None) if b.pipeline == Some(true) => {
            return Err(ApiError::bad_request("the server was started without a factory (--factory)"));
        }
        _ => None,
    };
    let t = NewTask { title: b.title, body: b.body, source: TaskSource::Web, factory: factory.clone() };
    let task_id = commands::create_task(&s.store, user.actor(), t).await?;
    Ok((StatusCode::CREATED, Json(Created { task_id, factory })))
}

async fn get_task(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<TaskDetail> {
    let task: TaskId = parse_id(&id, "task")?;
    let state = commands::load_task(&s.store, task).await?;
    let events = s.store.read_stream(&task.stream_id()).await?;
    Ok(Json(TaskDetail {
        task: queries::task(s.pool(), task.0).await?,
        state: TaskStateDto::from(&state),
        events: events.iter().map(EventDto::from).collect(),
        sessions: queries::task_sessions(s.pool(), task.0).await?,
        scores: scores::scores(s.pool(), &scores::ScoreFilter { task: Some(task.0), ..Default::default() }, 500).await?,
    }))
}

#[derive(Serialize)]
struct Ok_ {
    ok: bool,
    message: String,
}

fn ok(message: impl Into<String>) -> Json<Ok_> {
    Json(Ok_ { ok: true, message: message.into() })
}

async fn decide_gate(
    State(s): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    Json(b): Json<GateBody>,
) -> ApiResult<Ok_> {
    let task = parse_id(&id, "task")?;
    let gate = commands::decide_gate(&s.store, user.actor(), task, b.approved, b.comment).await?;
    Ok(ok(format!("gate '{gate}' {}", if b.approved { "approved" } else { "rejected" })))
}

async fn resume_task(
    State(s): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    Json(b): Json<ResumeBody>,
) -> ApiResult<Ok_> {
    commands::resume(&s.store, user.actor(), parse_id(&id, "task")?, b.stage, b.comment).await?;
    Ok(ok("resumed"))
}

async fn cancel_task(
    State(s): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    Json(b): Json<CancelBody>,
) -> ApiResult<Ok_> {
    commands::cancel(&s.store, user.actor(), parse_id(&id, "task")?, b.reason).await?;
    Ok(ok("cancelled"))
}

// ---------- sessions ----------

#[derive(Deserialize)]
struct LimitQuery {
    limit: Option<i64>,
}

async fn list_sessions(State(s): State<AppState>, Query(q): Query<LimitQuery>) -> ApiResult<Vec<queries::SessionDetail>> {
    Ok(Json(queries::sessions(s.pool(), q.limit.unwrap_or(100).clamp(1, 1000)).await?))
}

async fn get_session(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<SessionDetailResponse> {
    let sid: SessionId = parse_id(&id, "session")?;
    let session = queries::session(s.pool(), sid.0).await?.ok_or_else(|| ApiError::not_found("session"))?;
    let events = s.store.read_stream(&sid.stream_id()).await?;
    let filter = scores::ScoreFilter { session: Some(sid.0), ..Default::default() };
    Ok(Json(SessionDetailResponse {
        session,
        events: events.iter().map(EventDto::from).collect(),
        scores: scores::scores(s.pool(), &filter, 100).await?,
    }))
}

/// Max transcript bytes returned; longer transcripts keep their tail.
const TRANSCRIPT_MAX_BYTES: usize = 4 * 1024 * 1024;

#[derive(Serialize)]
struct Transcript {
    path: String,
    truncated: bool,
    lines: Vec<Value>,
}

async fn get_transcript(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Transcript> {
    let sid: Uuid = parse_id(&id, "session")?;
    let session = queries::session(s.pool(), sid).await?.ok_or_else(|| ApiError::not_found("session"))?;
    // Only paths recorded by the runner are ever read.
    let path = session.transcript.ok_or_else(|| ApiError::not_found("transcript"))?;
    let raw = tokio::fs::read(&path).await.map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => ApiError::not_found("transcript file"),
        _ => anyhow::anyhow!("read {path}: {e}").into(),
    })?;
    let truncated = raw.len() > TRANSCRIPT_MAX_BYTES;
    let slice = if truncated { &raw[raw.len() - TRANSCRIPT_MAX_BYTES..] } else { &raw[..] };
    let text = String::from_utf8_lossy(slice);
    let mut lines = text.lines();
    if truncated {
        lines.next(); // probably cut mid-line
    }
    let lines = lines.filter(|l| !l.trim().is_empty()).filter_map(|l| serde_json::from_str(l).ok()).collect();
    Ok(Json(Transcript { path, truncated, lines }))
}

// ---------- scorers ----------

/// A scorer: its definition (if the loaded factory has it) and its stats (if it has run).
#[derive(Serialize)]
struct ScorerSummary {
    id: String,
    definition: Option<ScorerDto>,
    stats: Option<scores::ScorerStats>,
}

#[derive(Serialize)]
struct Scorers {
    scorers: Vec<ScorerSummary>,
    by_version: Vec<scores::ScorerVersionStats>,
}

async fn list_scorers(State(s): State<AppState>) -> ApiResult<Scorers> {
    let mut stats = scores::scorer_stats(s.pool()).await?;
    let mut out: Vec<ScorerSummary> = s
        .factory
        .iter()
        .flat_map(|f| f.scorers.iter())
        .map(|d| {
            let st = stats.iter().position(|x| x.scorer == d.id).map(|i| stats.remove(i));
            ScorerSummary { id: d.id.clone(), definition: Some(ScorerDto::from(d)), stats: st }
        })
        .collect();
    // Scorers that ran in the past but are no longer defined.
    out.extend(stats.into_iter().map(|st| ScorerSummary { id: st.scorer.clone(), definition: None, stats: Some(st) }));
    Ok(Json(Scorers { scorers: out, by_version: scores::scorer_stats_by_version(s.pool()).await? }))
}

#[derive(Deserialize)]
struct ScoresQuery {
    scorer: Option<String>,
    task: Option<Uuid>,
    session: Option<Uuid>,
    #[serde(default)]
    failing: bool,
    limit: Option<i64>,
}

async fn list_scores(State(s): State<AppState>, Query(q): Query<ScoresQuery>) -> ApiResult<Vec<scores::ScoreRow>> {
    let filter = scores::ScoreFilter {
        scorer: q.scorer.filter(|x| !x.is_empty()),
        task: q.task,
        session: q.session,
        failing: q.failing,
    };
    Ok(Json(scores::scores(s.pool(), &filter, q.limit.unwrap_or(100).clamp(1, 1000)).await?))
}

// ---------- self-improvement ----------

#[derive(Deserialize)]
struct ProposalsQuery {
    /// A status, or `inbox` (open + approved-not-rolled-out).
    status: Option<String>,
    limit: Option<i64>,
}

async fn list_proposals(State(s): State<AppState>, Query(q): Query<ProposalsQuery>) -> ApiResult<Vec<proposals::ProposalRow>> {
    let status = q.status.as_deref().filter(|s| !s.is_empty() && *s != "all");
    Ok(Json(proposals::proposals(s.pool(), None, status, q.limit.unwrap_or(100).clamp(1, 1000)).await?))
}

#[derive(Serialize)]
struct Requested {
    proposal_id: ProposalId,
    message: String,
}

async fn request_improvement(
    State(s): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Json(b): Json<ImproveBody>,
) -> Result<(StatusCode, Json<Requested>), ApiError> {
    let f = s.factory.as_deref().ok_or_else(|| ApiError::bad_request("the server was started without a factory"))?;
    if f.improvement.is_none() {
        return Err(ApiError::bad_request(format!("factory '{}' has no `improvement:` section", f.name)));
    }
    let proposal_id = wh_improver::commands::request(&s.store, user.actor(), &f.name, b.focus).await?;
    let message = "improver run requested; it runs in `wh serve --foreman` or `wh improver`".to_owned();
    Ok((StatusCode::ACCEPTED, Json(Requested { proposal_id, message })))
}

async fn get_proposal(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<ProposalDetail> {
    let pid: ProposalId = parse_id(&id, "proposal")?;
    let events = s.store.read_stream(&pid.stream_id()).await?;
    if events.is_empty() {
        return Err(ApiError::not_found("proposal"));
    }
    let proposal = proposals::proposal(s.pool(), pid.0).await?;
    let mut evidence = Vec::new();
    if let Some(p) = &proposal {
        for sid in p.evidence.as_array().into_iter().flatten().filter_map(|v| v.as_str()?.parse::<Uuid>().ok()) {
            if let Some(row) = queries::session(s.pool(), sid).await? {
                evidence.push(row);
            }
        }
    }
    let (mut before, mut after) = (None, None);
    if let Some(p) = &proposal {
        let after_version = p.rolled_out_version.as_ref();
        for v in proposals::version_stats(s.pool(), Some(&p.factory)).await? {
            if Some(&v.factory_version) == p.base_factory_version.as_ref() {
                before = Some(v);
            } else if Some(&v.factory_version) == after_version {
                after = Some(v);
            }
        }
    }
    Ok(Json(ProposalDetail { proposal, events: events.iter().map(EventDto::from).collect(), evidence, before, after }))
}

async fn decide_proposal(
    State(s): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    Json(b): Json<GateBody>,
) -> ApiResult<Ok_> {
    let pid = parse_id(&id, "proposal")?;
    wh_improver::commands::decide(&s.store, user.actor(), pid, b.approved, b.comment).await?;
    Ok(ok(if b.approved { "proposal approved" } else { "proposal rejected" }))
}

async fn apply_proposal(
    State(s): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> ApiResult<Ok_> {
    let version = wh_improver::commands::apply(&s.store, user.actor(), parse_id(&id, "proposal")?).await?;
    Ok(ok(format!("rolled out as factory version {version}; restart the foreman to use it")))
}

#[derive(Serialize)]
struct Improvement {
    factory: Option<String>,
    current_version: Option<String>,
    config: Option<ImprovementDto>,
    stats: proposals::ImproverStats,
    versions: Vec<proposals::VersionStats>,
}

async fn improvement(State(s): State<AppState>) -> ApiResult<Improvement> {
    let f = s.factory.as_deref();
    Ok(Json(Improvement {
        factory: f.map(|f| f.name.clone()),
        current_version: f.map(|f| f.version.clone()),
        config: f.and_then(|f| f.improvement.as_ref()).map(ImprovementDto::from),
        // All factories, like the proposals list (version hashes are unique per factory).
        stats: proposals::improver_stats(s.pool(), None).await?,
        versions: proposals::version_stats(s.pool(), None).await?,
    }))
}

// ---------- metrics ----------

#[derive(Serialize)]
struct Stats {
    agents: Vec<wh_metrics::sessions::AgentStats>,
    stages: Vec<queries::StageStats>,
    failures: Vec<FailureCount>,
}

#[derive(Serialize)]
struct FailureCount {
    class: String,
    count: i64,
}

async fn stats(State(s): State<AppState>) -> ApiResult<Stats> {
    let pool = s.pool();
    Ok(Json(Stats {
        agents: SessionFactsProjection::stats_by_agent(pool).await?,
        stages: queries::stats_by_stage(pool).await?,
        failures: SessionFactsProjection::failures_by_class(pool)
            .await?
            .into_iter()
            .map(|(class, count)| FailureCount { class, count })
            .collect(),
    }))
}

#[derive(Deserialize)]
struct DaysQuery {
    days: Option<i32>,
}

#[derive(Serialize)]
struct Dashboard {
    days: i32,
    totals: queries::Totals,
    daily: Vec<queries::DailyPoint>,
    /// Pull request cycle times (phase 8); zero counts without GitHub.
    cycle: queries::CycleTimes,
}

async fn dashboard(State(s): State<AppState>, Query(q): Query<DaysQuery>) -> ApiResult<Dashboard> {
    let days = q.days.unwrap_or(30).clamp(1, 365);
    Ok(Json(Dashboard {
        days,
        totals: queries::totals(s.pool(), days).await?,
        daily: queries::daily(s.pool(), days).await?,
        cycle: queries::cycle_times(s.pool(), days).await?,
    }))
}

async fn factory(State(s): State<AppState>) -> ApiResult<Option<FactoryDto>> {
    Ok(Json(s.factory.as_deref().map(FactoryDto::from)))
}

// ---------- events ----------

#[derive(Deserialize)]
struct EventsQuery {
    after: Option<i64>,
    limit: Option<i64>,
}

async fn list_events(State(s): State<AppState>, Query(q): Query<EventsQuery>) -> ApiResult<Vec<EventDto>> {
    let events = s.store.read_all(q.after.unwrap_or(0), q.limit.unwrap_or(100).clamp(1, 1000)).await?;
    Ok(Json(events.iter().map(EventDto::from).collect()))
}

#[derive(Deserialize)]
struct StreamQuery {
    /// Start after this global_seq (default: only new events). `Last-Event-ID` wins.
    after: Option<i64>,
    /// Only events of this task (correlation id).
    task: Option<Uuid>,
}

async fn stream_events(State(s): State<AppState>, headers: HeaderMap, Query(q): Query<StreamQuery>) -> impl IntoResponse {
    let last_id = headers.get("last-event-id").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok());
    let cursor = last_id.or(q.after).unwrap_or_else(|| s.live.head());
    Sse::new(s.live.stream(s.store.clone(), cursor, q.task)).keep_alive(KeepAlive::default())
}
