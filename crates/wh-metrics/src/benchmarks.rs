use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;
use wh_core::{Actor, BenchmarkRunId, RecordedEvent, benchmark::BenchmarkEvent};
use wh_eventstore::EventHandler;

use crate::Projection;

/// `benchmark_runs` + `benchmark_cases`: one row per benchmark run and per case
/// run (case × repeat). Idempotent via `last_seq` on every row.
pub struct BenchmarksProjection {
    pool: PgPool,
}

impl BenchmarksProjection {
    pub const NAME: &'static str = "benchmarks";

    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    fn run_guarded(set_clause: &str) -> String {
        format!("UPDATE benchmark_runs SET {set_clause}, last_seq = $2 WHERE run_id = $1 AND last_seq < $2")
    }
}

fn actor_name(a: &Actor) -> String {
    match a {
        Actor::Human { user } => user.clone(),
        Actor::Agent { session } => format!("agent:{session}"),
        Actor::System { component } => component.clone(),
    }
}

impl EventHandler for BenchmarksProjection {
    fn name(&self) -> &str {
        Self::NAME
    }

    async fn handle(&mut self, ev: &RecordedEvent) -> anyhow::Result<()> {
        let Some(BenchmarkRunId(id)) = BenchmarkRunId::from_stream_id(&ev.stream_id) else {
            return Ok(());
        };
        let (seq, at) = (ev.global_seq, ev.meta.occurred_at);
        match ev.decode::<BenchmarkEvent>()? {
            BenchmarkEvent::BenchmarkRequested { suite, suite_file, git_ref, proposal, label, repeat, .. } => {
                sqlx::query(
                    "INSERT INTO benchmark_runs (run_id, suite, suite_file, git_ref, proposal_id, label, repeat, \
                     status, requested_by, requested_at, last_seq) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, 'requested', $8, $9, $10) ON CONFLICT (run_id) DO NOTHING",
                )
                .bind(id)
                .bind(suite)
                .bind(suite_file)
                .bind(git_ref)
                .bind(proposal.map(|p| p.0))
                .bind(label)
                .bind(repeat as i32)
                .bind(actor_name(&ev.meta.actor))
                .bind(at)
                .bind(seq)
                .execute(&self.pool)
                .await?;
            }
            BenchmarkEvent::BenchmarkStarted { factory, factory_version, factory_commit, suite_version, planned, .. } => {
                sqlx::query(&Self::run_guarded(
                    "status = 'running', started_at = $3, factory = $4, factory_version = $5, factory_commit = $6, \
                     suite_version = $7, planned = $8",
                ))
                .bind(id)
                .bind(seq)
                .bind(at)
                .bind(factory)
                .bind(factory_version)
                .bind(factory_commit)
                .bind(suite_version)
                .bind(planned as i32)
                .execute(&self.pool)
                .await?;
            }
            BenchmarkEvent::BenchmarkCaseStarted { case, repeat, task_id } => {
                sqlx::query(
                    "INSERT INTO benchmark_cases (run_id, case_id, repeat, task_id, status, started_at, last_seq) \
                     VALUES ($1, $2, $3, $4, 'running', $5, $6) ON CONFLICT DO NOTHING",
                )
                .bind(id)
                .bind(case)
                .bind(repeat as i32)
                .bind(task_id.0)
                .bind(at)
                .bind(seq)
                .execute(&self.pool)
                .await?;
            }
            BenchmarkEvent::BenchmarkCaseFinished {
                case,
                repeat,
                task_status,
                passed,
                check,
                cost_usd,
                duration_ms,
                sessions,
                rework_loops,
                message,
                ..
            } => {
                let done = sqlx::query(
                    "UPDATE benchmark_cases SET status = $5, passed = $6, check_passed = $7, check_exit_code = $8, \
                     check_output = $9, cost_usd = $10, duration_ms = $11, sessions = $12, rework_loops = $13, \
                     message = $14, finished_at = $15, last_seq = $4 \
                     WHERE run_id = $1 AND case_id = $2 AND repeat = $3 AND last_seq < $4",
                )
                .bind(id)
                .bind(&case)
                .bind(repeat as i32)
                .bind(seq)
                .bind(task_status)
                .bind(passed)
                .bind(check.as_ref().map(|c| c.passed))
                .bind(check.as_ref().and_then(|c| c.exit_code))
                .bind(check.map(|c| c.output))
                .bind(cost_usd)
                .bind(duration_ms as i64)
                .bind(sessions as i32)
                .bind(rework_loops as i32)
                .bind(message)
                .bind(at)
                .execute(&self.pool)
                .await?;
                if done.rows_affected() > 0 {
                    sqlx::query(&Self::run_guarded(
                        "cases_done = cases_done + 1, passed = passed + $3, cost_usd = cost_usd + $4",
                    ))
                    .bind(id)
                    .bind(seq)
                    .bind(i32::from(passed))
                    .bind(cost_usd)
                    .execute(&self.pool)
                    .await?;
                }
            }
            BenchmarkEvent::BenchmarkCompleted { duration_ms, aborted, .. } => {
                sqlx::query(&Self::run_guarded("status = 'completed', finished_at = $3, duration_ms = $4, message = $5"))
                    .bind(id)
                    .bind(seq)
                    .bind(at)
                    .bind(duration_ms as i64)
                    .bind(aborted)
                    .execute(&self.pool)
                    .await?;
            }
            BenchmarkEvent::BenchmarkFailed { message } => {
                sqlx::query(&Self::run_guarded("status = 'failed', finished_at = $3, message = $4"))
                    .bind(id)
                    .bind(seq)
                    .bind(at)
                    .bind(message)
                    .execute(&self.pool)
                    .await?;
            }
        }
        Ok(())
    }
}

impl Projection for BenchmarksProjection {
    async fn reset(&self) -> anyhow::Result<()> {
        sqlx::query("TRUNCATE benchmark_runs, benchmark_cases").execute(&self.pool).await?;
        Ok(())
    }
}

/// A benchmark run with aggregates over its finished case runs.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct BenchRun {
    pub run_id: Uuid,
    pub suite: String,
    pub suite_file: String,
    pub git_ref: Option<String>,
    pub proposal_id: Option<Uuid>,
    pub label: Option<String>,
    pub repeat: i32,
    pub status: String,
    pub requested_by: String,
    pub requested_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub factory: Option<String>,
    pub factory_version: Option<String>,
    pub factory_commit: Option<String>,
    pub suite_version: Option<String>,
    pub planned: Option<i32>,
    pub cases_done: i32,
    pub passed: i32,
    pub cost_usd: f64,
    pub duration_ms: Option<i64>,
    pub message: Option<String>,
    /// passed / cases_done (None before the first case finishes).
    pub pass_rate: Option<f64>,
    pub avg_case_cost_usd: Option<f64>,
    pub p50_case_duration_ms: Option<f64>,
    pub avg_rework: Option<f64>,
}

const RUN_SQL: &str = "SELECT r.run_id, r.suite, r.suite_file, r.git_ref, r.proposal_id, r.label, r.repeat, r.status, \
     r.requested_by, r.requested_at, r.started_at, r.finished_at, r.factory, r.factory_version, r.factory_commit, \
     r.suite_version, r.planned, r.cases_done, r.passed, r.cost_usd, r.duration_ms, r.message, \
     r.passed::float8 / NULLIF(r.cases_done, 0) AS pass_rate, c.avg_cost AS avg_case_cost_usd, \
     c.p50_duration AS p50_case_duration_ms, c.avg_rework \
     FROM benchmark_runs r LEFT JOIN ( \
       SELECT run_id, avg(cost_usd) AS avg_cost, \
         percentile_cont(0.5) WITHIN GROUP (ORDER BY duration_ms) AS p50_duration, \
         avg(rework_loops)::float8 AS avg_rework \
       FROM benchmark_cases WHERE finished_at IS NOT NULL GROUP BY run_id \
     ) c ON c.run_id = r.run_id";

/// Benchmark runs, newest first, optionally for one suite or proposal.
pub async fn runs(
    pool: &PgPool,
    suite: Option<&str>,
    proposal: Option<Uuid>,
    limit: i64,
) -> anyhow::Result<Vec<BenchRun>> {
    Ok(sqlx::query_as(&format!(
        "{RUN_SQL} WHERE ($1::text IS NULL OR r.suite = $1) AND ($2::uuid IS NULL OR r.proposal_id = $2) \
         ORDER BY r.requested_at DESC LIMIT $3"
    ))
    .bind(suite)
    .bind(proposal)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

pub async fn run(pool: &PgPool, id: Uuid) -> anyhow::Result<Option<BenchRun>> {
    Ok(sqlx::query_as(&format!("{RUN_SQL} WHERE r.run_id = $1")).bind(id).fetch_optional(pool).await?)
}

/// One case run of a benchmark run.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct BenchCase {
    pub run_id: Uuid,
    pub case_id: String,
    pub repeat: i32,
    pub task_id: Uuid,
    pub status: String,
    pub passed: Option<bool>,
    pub check_passed: Option<bool>,
    pub check_exit_code: Option<i32>,
    pub check_output: Option<String>,
    pub cost_usd: f64,
    pub duration_ms: Option<i64>,
    pub sessions: Option<i32>,
    pub rework_loops: Option<i32>,
    pub message: Option<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

pub async fn cases(pool: &PgPool, run: Uuid) -> anyhow::Result<Vec<BenchCase>> {
    Ok(sqlx::query_as(
        "SELECT run_id, case_id, repeat, task_id, status, passed, check_passed, check_exit_code, check_output, \
         cost_usd, duration_ms, sessions, rework_loops, message, started_at, finished_at \
         FROM benchmark_cases WHERE run_id = $1 ORDER BY started_at, case_id, repeat",
    )
    .bind(run)
    .fetch_all(pool)
    .await?)
}

/// Per-case aggregate over the repeats of one run.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct CaseStats {
    pub case_id: String,
    pub runs: i64,
    pub passed: i64,
    pub avg_cost_usd: Option<f64>,
    pub avg_duration_ms: Option<f64>,
    pub avg_rework: Option<f64>,
}

pub async fn case_stats(pool: &PgPool, run: Uuid) -> anyhow::Result<Vec<CaseStats>> {
    Ok(sqlx::query_as(
        "SELECT case_id, count(*) AS runs, count(*) FILTER (WHERE passed) AS passed, \
           avg(cost_usd) AS avg_cost_usd, avg(duration_ms)::float8 AS avg_duration_ms, \
           avg(rework_loops)::float8 AS avg_rework \
         FROM benchmark_cases WHERE run_id = $1 AND finished_at IS NOT NULL GROUP BY case_id ORDER BY case_id",
    )
    .bind(run)
    .fetch_all(pool)
    .await?)
}

/// One case side by side across two runs (A = baseline, B = candidate).
#[derive(Debug, Clone, Serialize)]
pub struct CaseComparison {
    pub case_id: String,
    pub a: Option<CaseStats>,
    pub b: Option<CaseStats>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Comparison {
    pub a: BenchRun,
    pub b: BenchRun,
    /// False when the suites differ (different file or suite version): the
    /// numbers are then not directly comparable.
    pub comparable: bool,
    pub cases: Vec<CaseComparison>,
}

pub async fn compare(pool: &PgPool, a: Uuid, b: Uuid) -> anyhow::Result<Option<Comparison>> {
    let (Some(ra), Some(rb)) = (run(pool, a).await?, run(pool, b).await?) else {
        return Ok(None);
    };
    let (ca, cb) = (case_stats(pool, a).await?, case_stats(pool, b).await?);
    let mut ids: Vec<String> = ca.iter().chain(cb.iter()).map(|c| c.case_id.clone()).collect();
    ids.sort();
    ids.dedup();
    let cases = ids
        .into_iter()
        .map(|id| CaseComparison {
            a: ca.iter().find(|c| c.case_id == id).cloned(),
            b: cb.iter().find(|c| c.case_id == id).cloned(),
            case_id: id,
        })
        .collect();
    let comparable = ra.suite_file == rb.suite_file && ra.suite_version == rb.suite_version;
    Ok(Some(Comparison { a: ra, b: rb, comparable, cases }))
}
