//! `/api/benchmarks`: suites, runs, case results and run comparison. Runs are
//! queued here and executed by the benchmark worker (`wh serve --foreman`,
//! `wh foreman` or `wh bench worker`).

use std::path::PathBuf;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use wh_bench::{CaseRepo, Request, commands::CommandError as BenchError, suite};
use wh_core::{BenchmarkRunId, ProposalId};
use wh_metrics::benchmarks;

use crate::{ApiError, ApiResult, AppState, auth::CurrentUser, dto::EventDto, parse_id};

impl From<BenchError> for ApiError {
    fn from(e: BenchError) -> Self {
        let code = match &e {
            BenchError::NotFound(_) => StatusCode::NOT_FOUND,
            BenchError::Invalid(_) => StatusCode::UNPROCESSABLE_ENTITY,
            BenchError::Other(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        ApiError(code, format!("{e:#}"))
    }
}

#[derive(Serialize)]
pub struct SuiteCaseDto {
    id: String,
    title: String,
    /// factory | repo | fixture
    repo: &'static str,
    check: Option<String>,
}

#[derive(Serialize)]
pub struct SuiteDto {
    /// Name to pass back as `suite` when requesting a run (the file stem).
    key: String,
    file: String,
    name: Option<String>,
    description: Option<String>,
    version: Option<String>,
    factory_file: Option<String>,
    max_cost_usd: Option<f64>,
    cases: Vec<SuiteCaseDto>,
    /// Set when the suite file does not load.
    error: Option<String>,
}

pub async fn suites(State(s): State<AppState>) -> ApiResult<Vec<SuiteDto>> {
    let out = suite::discover(&s.bench_dir)
        .into_iter()
        .map(|(path, loaded)| {
            let key = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let file = path.display().to_string();
            match loaded {
                Ok(su) => SuiteDto {
                    key,
                    file,
                    name: Some(su.name),
                    description: su.description,
                    version: Some(su.version),
                    factory_file: Some(su.factory_file.display().to_string()),
                    max_cost_usd: su.max_cost_usd,
                    cases: su
                        .cases
                        .into_iter()
                        .map(|c| SuiteCaseDto {
                            id: c.id,
                            title: c.title,
                            repo: match c.repo {
                                CaseRepo::Factory => "factory",
                                CaseRepo::Repo(_) => "repo",
                                CaseRepo::Fixture(_) => "fixture",
                            },
                            check: c.check,
                        })
                        .collect(),
                    error: None,
                },
                Err(e) => SuiteDto {
                    key,
                    file,
                    name: None,
                    description: None,
                    version: None,
                    factory_file: None,
                    max_cost_usd: None,
                    cases: Vec::new(),
                    error: Some(format!("{e:#}")),
                },
            }
        })
        .collect();
    Ok(Json(out))
}

#[derive(Deserialize)]
pub struct RunsQuery {
    suite: Option<String>,
    proposal: Option<Uuid>,
    limit: Option<i64>,
}

pub async fn runs(State(s): State<AppState>, Query(q): Query<RunsQuery>) -> ApiResult<Vec<benchmarks::BenchRun>> {
    let suite = q.suite.as_deref().filter(|s| !s.is_empty());
    Ok(Json(benchmarks::runs(s.pool(), suite, q.proposal, q.limit.unwrap_or(100).clamp(1, 1000)).await?))
}

#[derive(Deserialize)]
pub struct RunBody {
    /// Suite key (file stem) within the server's benchmark directory.
    suite: String,
    git_ref: Option<String>,
    /// Benchmark a proposal: without `git_ref`, queues its base and its commit.
    proposal: Option<ProposalId>,
    label: Option<String>,
    repeat: Option<u32>,
    #[serde(default)]
    cases: Vec<String>,
}

#[derive(Serialize)]
pub struct Queued {
    run_ids: Vec<BenchmarkRunId>,
    message: String,
}

/// Only suites inside the benchmark directory can be run from the API.
fn suite_file(s: &AppState, key: &str) -> Result<PathBuf, ApiError> {
    if key.is_empty() || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return Err(ApiError::bad_request(format!("invalid suite '{key}'")));
    }
    ["yaml", "yml"]
        .iter()
        .map(|ext| s.bench_dir.join(format!("{key}.{ext}")))
        .find(|p| p.is_file())
        .ok_or_else(|| ApiError::not_found(format!("suite '{key}'")))
}

pub async fn request(
    State(s): State<AppState>,
    axum::Extension(user): axum::Extension<CurrentUser>,
    Json(b): Json<RunBody>,
) -> Result<(StatusCode, Json<Queued>), ApiError> {
    let file = suite_file(&s, &b.suite)?;
    let repeat = b.repeat.unwrap_or(1);
    if !(1..=20).contains(&repeat) {
        return Err(ApiError::bad_request("repeat must be within 1..=20"));
    }
    let git_ref = b.git_ref.filter(|r| !r.trim().is_empty());
    let run_ids = match (b.proposal, git_ref) {
        (Some(p), None) => {
            let (a, c) = wh_bench::commands::request_proposal(&s.store, user.actor(), file, p, repeat, b.cases).await?;
            vec![a, c]
        }
        (proposal, git_ref) => {
            let req = Request { suite_file: file, git_ref, proposal, label: b.label, repeat, cases: b.cases };
            vec![wh_bench::commands::request(&s.store, user.actor(), &req).await?]
        }
    };
    let message = "benchmark queued; it runs in `wh serve --foreman`, `wh foreman` or `wh bench worker`".to_owned();
    Ok((StatusCode::ACCEPTED, Json(Queued { run_ids, message })))
}

#[derive(Serialize)]
pub struct RunDetail {
    run: Option<benchmarks::BenchRun>,
    cases: Vec<benchmarks::BenchCase>,
    case_stats: Vec<benchmarks::CaseStats>,
    events: Vec<EventDto>,
}

pub async fn run(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<RunDetail> {
    let rid: BenchmarkRunId = parse_id(&id, "benchmark run")?;
    let events = s.store.read_stream(&rid.stream_id()).await?;
    if events.is_empty() {
        return Err(ApiError::not_found("benchmark run"));
    }
    Ok(Json(RunDetail {
        run: benchmarks::run(s.pool(), rid.0).await?,
        cases: benchmarks::cases(s.pool(), rid.0).await?,
        case_stats: benchmarks::case_stats(s.pool(), rid.0).await?,
        events: events.iter().map(EventDto::from).collect(),
    }))
}

#[derive(Deserialize)]
pub struct CompareQuery {
    a: Uuid,
    b: Uuid,
}

pub async fn compare(State(s): State<AppState>, Query(q): Query<CompareQuery>) -> ApiResult<benchmarks::Comparison> {
    benchmarks::compare(s.pool(), q.a, q.b).await?.map(Json).ok_or_else(|| ApiError::not_found("benchmark run"))
}
