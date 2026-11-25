//! Read-side queries over the projections, shaped for the API/UI. Everything
//! here reads read-model tables only (never the event log).

use chrono::{DateTime, NaiveDate, Utc};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

/// A `tasks_view` row plus aggregates from `session_facts`.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct TaskSummary {
    pub task_id: Uuid,
    pub title: String,
    pub source: String,
    pub factory: Option<String>,
    pub status: String,
    pub current_stage: Option<String>,
    pub rework_loops: i32,
    pub gate_rejections: i32,
    pub blocked_reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub sessions: i64,
    pub cost_usd: f64,
    /// Set for benchmark cases (hidden from the task list).
    pub benchmark_run: Option<Uuid>,
    pub benchmark_case: Option<String>,
    /// GitHub (phase 8): the issue the task came from and its pull request.
    pub external_ref: Option<String>,
    pub pr_url: Option<String>,
    pub pr_number: Option<i32>,
    /// open | merged | closed
    pub pr_state: Option<String>,
    pub shipped_at: Option<DateTime<Utc>>,
    pub first_review_at: Option<DateTime<Utc>>,
    pub merged_at: Option<DateTime<Utc>>,
    pub pr_reviews: i32,
    pub changes_requested: i32,
    pub ci_failures: i32,
    pub pr_rework_loops: i32,
}

const TASK_SUMMARY_SQL: &str = "SELECT t.task_id, t.title, t.source, t.factory, t.status, t.current_stage, \
     t.rework_loops, t.gate_rejections, t.blocked_reason, t.created_at, t.updated_at, t.completed_at, \
     COALESCE(s.sessions, 0) AS sessions, COALESCE(s.cost_usd, 0) AS cost_usd, \
     t.benchmark_run, t.benchmark_case, t.external_ref, t.pr_url, t.pr_number, t.pr_state, t.shipped_at, \
     t.first_review_at, t.merged_at, t.pr_reviews, t.changes_requested, t.ci_failures, t.pr_rework_loops \
     FROM tasks_view t LEFT JOIN ( \
       SELECT task_id, count(*) AS sessions, sum(cost_usd) AS cost_usd FROM session_facts GROUP BY task_id \
     ) s ON s.task_id = t.task_id";

/// Tasks (not benchmark cases), newest first, optionally filtered by status
/// (`open`, `running`, `awaiting_gate`, ...).
pub async fn tasks(pool: &PgPool, status: Option<&str>, limit: i64) -> anyhow::Result<Vec<TaskSummary>> {
    Ok(sqlx::query_as(&format!(
        "{TASK_SUMMARY_SQL} WHERE t.benchmark_run IS NULL AND ($1::text IS NULL OR t.status = $1) ORDER BY t.created_at DESC LIMIT $2"
    ))
    .bind(status)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

pub async fn task(pool: &PgPool, id: Uuid) -> anyhow::Result<Option<TaskSummary>> {
    Ok(sqlx::query_as(&format!("{TASK_SUMMARY_SQL} WHERE t.task_id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

/// Every column of a `session_facts` row.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct SessionDetail {
    pub session_id: Uuid,
    pub task_id: Uuid,
    pub agent: String,
    pub stage: String,
    pub runtime: String,
    pub model: Option<String>,
    pub status: String,
    pub failure_class: Option<String>,
    pub failure_message: Option<String>,
    pub attempt: i32,
    pub retry_of: Option<Uuid>,
    pub dispatched_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<i64>,
    pub num_turns: Option<i32>,
    pub tool_calls: i32,
    pub tool_errors: i32,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub cost_usd: f64,
    pub workdir: Option<String>,
    pub branch: Option<String>,
    /// `worktree` or `docker:<image>` (NULL before phase 7 = worktree).
    pub sandbox: Option<String>,
    pub transcript: Option<String>,
    pub factory_version: Option<String>,
    /// Pipeline verdict for the session's stage: succeeded | failed | rejected.
    pub stage_outcome: Option<String>,
}

const SESSION_COLS: &str = "session_id, task_id, agent, stage, runtime, model, status, failure_class, \
     failure_message, attempt, retry_of, dispatched_at, ended_at, duration_ms, num_turns, tool_calls, tool_errors, \
     input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, cost_usd, workdir, branch, sandbox, \
     transcript, factory_version, stage_outcome";

/// Sessions of one task in dispatch order (the run timeline).
pub async fn task_sessions(pool: &PgPool, task: Uuid) -> anyhow::Result<Vec<SessionDetail>> {
    Ok(sqlx::query_as(&format!(
        "SELECT {SESSION_COLS} FROM session_facts WHERE task_id = $1 ORDER BY dispatched_at"
    ))
    .bind(task)
    .fetch_all(pool)
    .await?)
}

pub async fn sessions(pool: &PgPool, limit: i64) -> anyhow::Result<Vec<SessionDetail>> {
    Ok(sqlx::query_as(&format!(
        "SELECT {SESSION_COLS} FROM session_facts WHERE benchmark_run IS NULL ORDER BY dispatched_at DESC LIMIT $1"
    ))
        .bind(limit)
        .fetch_all(pool)
        .await?)
}

pub async fn session(pool: &PgPool, id: Uuid) -> anyhow::Result<Option<SessionDetail>> {
    Ok(sqlx::query_as(&format!("SELECT {SESSION_COLS} FROM session_facts WHERE session_id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

/// Headline numbers for the dashboard over the last `days` days.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Totals {
    pub tasks_created: i64,
    pub tasks_done: i64,
    pub tasks_blocked: i64,
    pub tasks_in_flight: i64,
    pub awaiting_gate: i64,
    pub sessions: i64,
    pub sessions_failed: i64,
    pub cost_usd: f64,
    /// Median / p90 kickoff → completion over tasks completed in the window.
    pub cycle_p50_ms: Option<f64>,
    pub cycle_p90_ms: Option<f64>,
    /// Average rework loops per completed task.
    pub avg_rework: Option<f64>,
    /// Gate rejections / gate decisions are not tracked separately yet; this is
    /// rejections per task that reached a gate.
    pub gate_rejections: i64,
    /// LLM-as-judge spend in the window (not included in `cost_usd`).
    pub scorer_cost_usd: f64,
    pub scores: i64,
    pub scores_passed: i64,
}

pub async fn totals(pool: &PgPool, days: i32) -> anyhow::Result<Totals> {
    Ok(sqlx::query_as(
        "WITH win AS (SELECT now() - make_interval(days => $1) AS since), \
         tv AS (SELECT * FROM tasks_view WHERE benchmark_run IS NULL), \
         t AS (SELECT * FROM tv, win WHERE created_at >= win.since OR updated_at >= win.since), \
         s AS (SELECT * FROM session_facts, win WHERE dispatched_at >= win.since AND benchmark_run IS NULL), \
         r AS (SELECT * FROM score_results, win WHERE started_at >= win.since) \
         SELECT \
           (SELECT count(*) FROM tv, win WHERE created_at >= win.since) AS tasks_created, \
           (SELECT count(*) FROM t WHERE status = 'done' AND completed_at >= (SELECT since FROM win)) AS tasks_done, \
           (SELECT count(*) FROM tv WHERE status = 'blocked') AS tasks_blocked, \
           (SELECT count(*) FROM tv WHERE status IN ('open', 'running')) AS tasks_in_flight, \
           (SELECT count(*) FROM tv WHERE status = 'awaiting_gate') AS awaiting_gate, \
           (SELECT count(*) FROM s) AS sessions, \
           (SELECT count(*) FROM s WHERE status = 'failed') AS sessions_failed, \
           (SELECT COALESCE(sum(cost_usd), 0) FROM s) AS cost_usd, \
           (SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY extract(epoch FROM completed_at - created_at) * 1000) \
              FROM t WHERE status = 'done' AND completed_at IS NOT NULL) AS cycle_p50_ms, \
           (SELECT percentile_cont(0.9) WITHIN GROUP (ORDER BY extract(epoch FROM completed_at - created_at) * 1000) \
              FROM t WHERE status = 'done' AND completed_at IS NOT NULL) AS cycle_p90_ms, \
           (SELECT avg(rework_loops)::float8 FROM t WHERE status = 'done') AS avg_rework, \
           (SELECT COALESCE(sum(gate_rejections), 0)::int8 FROM t) AS gate_rejections, \
           (SELECT COALESCE(sum(cost_usd), 0) FROM r) AS scorer_cost_usd, \
           (SELECT count(*) FROM r WHERE status = 'scored') AS scores, \
           (SELECT count(*) FROM r WHERE pass) AS scores_passed",
    )
    .bind(days)
    .fetch_one(pool)
    .await?)
}

/// One day of activity (UTC days, zero-filled).
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct DailyPoint {
    pub day: NaiveDate,
    pub cost_usd: f64,
    pub sessions_succeeded: i64,
    pub sessions_failed: i64,
    pub tasks_created: i64,
    pub tasks_done: i64,
}

pub async fn daily(pool: &PgPool, days: i32) -> anyhow::Result<Vec<DailyPoint>> {
    Ok(sqlx::query_as(
        "WITH d AS ( \
           SELECT generate_series((now() AT TIME ZONE 'UTC')::date - ($1 - 1), (now() AT TIME ZONE 'UTC')::date, \
                                  interval '1 day')::date AS day), \
         s AS ( \
           SELECT (dispatched_at AT TIME ZONE 'UTC')::date AS day, sum(cost_usd) AS cost, \
                  count(*) FILTER (WHERE status = 'succeeded') AS ok, count(*) FILTER (WHERE status = 'failed') AS failed \
           FROM session_facts WHERE benchmark_run IS NULL GROUP BY 1), \
         c AS (SELECT (created_at AT TIME ZONE 'UTC')::date AS day, count(*) AS n FROM tasks_view \
               WHERE benchmark_run IS NULL GROUP BY 1), \
         f AS (SELECT (completed_at AT TIME ZONE 'UTC')::date AS day, count(*) AS n FROM tasks_view \
               WHERE completed_at IS NOT NULL AND benchmark_run IS NULL GROUP BY 1) \
         SELECT d.day, COALESCE(s.cost, 0) AS cost_usd, COALESCE(s.ok, 0) AS sessions_succeeded, \
                COALESCE(s.failed, 0) AS sessions_failed, COALESCE(c.n, 0) AS tasks_created, \
                COALESCE(f.n, 0) AS tasks_done \
         FROM d LEFT JOIN s USING (day) LEFT JOIN c USING (day) LEFT JOIN f USING (day) ORDER BY d.day",
    )
    .bind(days)
    .fetch_all(pool)
    .await?)
}

/// Per-stage outcome and rework statistics over finished sessions.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct StageStats {
    pub stage: String,
    pub runs: i64,
    pub succeeded: i64,
    pub failed: i64,
    pub retries: i64,
    pub total_cost_usd: f64,
    pub avg_cost_usd: f64,
    pub p50_duration_ms: Option<f64>,
    pub p95_duration_ms: Option<f64>,
}

pub async fn stats_by_stage(pool: &PgPool) -> anyhow::Result<Vec<StageStats>> {
    Ok(sqlx::query_as(
        "SELECT stage, count(*) AS runs, \
           count(*) FILTER (WHERE status = 'succeeded') AS succeeded, \
           count(*) FILTER (WHERE status = 'failed') AS failed, \
           count(*) FILTER (WHERE attempt > 1) AS retries, \
           COALESCE(sum(cost_usd), 0) AS total_cost_usd, COALESCE(avg(cost_usd), 0) AS avg_cost_usd, \
           percentile_cont(0.5) WITHIN GROUP (ORDER BY duration_ms) AS p50_duration_ms, \
           percentile_cont(0.95) WITHIN GROUP (ORDER BY duration_ms) AS p95_duration_ms \
         FROM session_facts WHERE status IN ('succeeded', 'failed') AND benchmark_run IS NULL \
         GROUP BY stage ORDER BY stage",
    )
    .fetch_all(pool)
    .await?)
}

/// Pull request cycle times over tasks shipped in the last `days` days (live
/// tasks only). Durations are milliseconds; `None` when there is no data.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct CycleTimes {
    pub shipped: i64,
    pub merged: i64,
    /// PRs still open.
    pub open: i64,
    /// Shipped tasks whose PR got at least one `CHANGES_REQUESTED` review.
    pub changes_requested_tasks: i64,
    /// Shipped tasks whose PR had at least one failed CI run.
    pub ci_failed_tasks: i64,
    pub avg_pr_rework: Option<f64>,
    pub kickoff_to_pr_p50_ms: Option<f64>,
    pub kickoff_to_pr_p95_ms: Option<f64>,
    pub pr_to_first_review_p50_ms: Option<f64>,
    pub pr_to_first_review_p95_ms: Option<f64>,
    pub review_to_merge_p50_ms: Option<f64>,
    pub review_to_merge_p95_ms: Option<f64>,
    pub kickoff_to_merge_p50_ms: Option<f64>,
    pub kickoff_to_merge_p95_ms: Option<f64>,
}

pub async fn cycle_times(pool: &PgPool, days: i32) -> anyhow::Result<CycleTimes> {
    let ms = |a: &str, b: &str, p: f64| {
        format!("percentile_cont({p}) WITHIN GROUP (ORDER BY extract(epoch FROM {b} - {a}) * 1000) FILTER (WHERE {a} IS NOT NULL AND {b} IS NOT NULL)")
    };
    let sql = format!(
        "SELECT count(*) AS shipped, count(*) FILTER (WHERE pr_state = 'merged') AS merged, \
           count(*) FILTER (WHERE pr_state = 'open') AS open, \
           count(*) FILTER (WHERE changes_requested > 0) AS changes_requested_tasks, \
           count(*) FILTER (WHERE ci_failures > 0) AS ci_failed_tasks, \
           avg(pr_rework_loops)::float8 AS avg_pr_rework, \
           {} AS kickoff_to_pr_p50_ms, {} AS kickoff_to_pr_p95_ms, \
           {} AS pr_to_first_review_p50_ms, {} AS pr_to_first_review_p95_ms, \
           {} AS review_to_merge_p50_ms, {} AS review_to_merge_p95_ms, \
           {} AS kickoff_to_merge_p50_ms, {} AS kickoff_to_merge_p95_ms \
         FROM tasks_view WHERE benchmark_run IS NULL AND shipped_at IS NOT NULL \
           AND shipped_at >= now() - make_interval(days => $1)",
        ms("created_at", "shipped_at", 0.5),
        ms("created_at", "shipped_at", 0.95),
        ms("shipped_at", "first_review_at", 0.5),
        ms("shipped_at", "first_review_at", 0.95),
        ms("first_review_at", "merged_at", 0.5),
        ms("first_review_at", "merged_at", 0.95),
        ms("created_at", "merged_at", 0.5),
        ms("created_at", "merged_at", 0.95),
    );
    Ok(sqlx::query_as(&sql).bind(days).fetch_one(pool).await?)
}
