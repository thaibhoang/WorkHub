use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;
use wh_core::{RecordedEvent, SessionId, session::SessionEvent, task::TaskEvent};
use wh_eventstore::EventHandler;

use crate::Projection;

/// `session_facts`: one row per agent session with cost, tokens, duration and outcome.
/// Idempotent via `last_seq`, like every projection.
pub struct SessionFactsProjection {
    pool: PgPool,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SessionRow {
    pub session_id: Uuid,
    pub task_id: Uuid,
    pub agent: String,
    pub model: Option<String>,
    pub status: String,
    pub failure_class: Option<String>,
    pub attempt: i32,
    pub dispatched_at: DateTime<Utc>,
    pub duration_ms: Option<i64>,
    pub num_turns: Option<i32>,
    pub tool_calls: i32,
    pub tool_errors: i32,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cost_usd: f64,
}

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct AgentStats {
    pub agent: String,
    pub runs: i64,
    pub succeeded: i64,
    pub failed: i64,
    /// Sessions that were retries (attempt > 1).
    pub retries: i64,
    pub total_cost_usd: f64,
    pub avg_cost_usd: f64,
    pub p50_duration_ms: Option<f64>,
    pub p95_duration_ms: Option<f64>,
    pub avg_turns: Option<f64>,
    pub tool_calls: i64,
    pub tool_errors: i64,
}

impl AgentStats {
    pub fn failure_rate(&self) -> f64 {
        if self.runs == 0 { 0.0 } else { self.failed as f64 / self.runs as f64 }
    }
}

impl SessionFactsProjection {
    pub const NAME: &'static str = "session_facts";

    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn recent(pool: &PgPool, limit: i64) -> anyhow::Result<Vec<SessionRow>> {
        Ok(sqlx::query_as(
            "SELECT session_id, task_id, agent, model, status, failure_class, attempt, dispatched_at, duration_ms, \
             num_turns, tool_calls, tool_errors, input_tokens, output_tokens, cache_read_tokens, cost_usd \
             FROM session_facts WHERE benchmark_run IS NULL ORDER BY dispatched_at DESC LIMIT $1",
        )
        .bind(limit)
        .fetch_all(pool)
        .await?)
    }

    /// Per-agent aggregates over finished sessions.
    pub async fn stats_by_agent(pool: &PgPool) -> anyhow::Result<Vec<AgentStats>> {
        Ok(sqlx::query_as(
            "SELECT agent, \
               count(*) AS runs, \
               count(*) FILTER (WHERE status = 'succeeded') AS succeeded, \
               count(*) FILTER (WHERE status = 'failed') AS failed, \
               count(*) FILTER (WHERE attempt > 1) AS retries, \
               COALESCE(sum(cost_usd), 0) AS total_cost_usd, \
               COALESCE(avg(cost_usd), 0) AS avg_cost_usd, \
               percentile_cont(0.5) WITHIN GROUP (ORDER BY duration_ms) AS p50_duration_ms, \
               percentile_cont(0.95) WITHIN GROUP (ORDER BY duration_ms) AS p95_duration_ms, \
               avg(num_turns)::float8 AS avg_turns, \
               COALESCE(sum(tool_calls), 0)::int8 AS tool_calls, \
               COALESCE(sum(tool_errors), 0)::int8 AS tool_errors \
             FROM session_facts WHERE status IN ('succeeded', 'failed') AND benchmark_run IS NULL \
             GROUP BY agent ORDER BY agent",
        )
        .fetch_all(pool)
        .await?)
    }

    /// Failure counts by class across all agents.
    pub async fn failures_by_class(pool: &PgPool) -> anyhow::Result<Vec<(String, i64)>> {
        Ok(sqlx::query_as(
            "SELECT COALESCE(failure_class, 'unknown'), count(*) FROM session_facts \
             WHERE status = 'failed' AND benchmark_run IS NULL GROUP BY 1 ORDER BY 2 DESC",
        )
        .fetch_all(pool)
        .await?)
    }

    /// Run an UPDATE guarded by `last_seq`; `$1` = session id, `$2` = seq, `$3..` = caller's binds.
    fn guarded(set_clause: &str) -> String {
        format!("UPDATE session_facts SET {set_clause}, last_seq = $2 WHERE session_id = $1 AND last_seq < $2")
    }
}

impl EventHandler for SessionFactsProjection {
    fn name(&self) -> &str {
        Self::NAME
    }

    async fn handle(&mut self, ev: &RecordedEvent) -> anyhow::Result<()> {
        if ev.event_type == "StageExited" {
            // Verdict the pipeline gave the session's stage (rejects are review loops).
            if let TaskEvent::StageExited { outcome, session: Some(SessionId(id)), .. } = ev.decode::<TaskEvent>()? {
                sqlx::query(&Self::guarded("stage_outcome = $3"))
                    .bind(id)
                    .bind(ev.global_seq)
                    .bind(outcome.as_str())
                    .execute(&self.pool)
                    .await?;
            }
            return Ok(());
        }
        let Some(SessionId(id)) = SessionId::from_stream_id(&ev.stream_id) else {
            return Ok(());
        };
        let seq = ev.global_seq;
        let at = ev.meta.occurred_at;
        match ev.decode::<SessionEvent>()? {
            SessionEvent::AgentDispatched { task_id, agent, stage, runtime, model, attempt, retry_of, benchmark, .. } => {
                sqlx::query(
                    "INSERT INTO session_facts \
                     (session_id, task_id, agent, stage, runtime, model, status, dispatched_at, factory_version, \
                      attempt, retry_of, last_seq, benchmark_run) \
                     VALUES ($1, $2, $3, $4, $5, $6, 'dispatched', $7, $8, $10, $11, $9, $12) \
                     ON CONFLICT (session_id) DO NOTHING",
                )
                .bind(id)
                .bind(task_id.0)
                .bind(agent)
                .bind(stage)
                .bind(runtime)
                .bind(model)
                .bind(at)
                .bind(&ev.meta.factory_version)
                .bind(seq)
                .bind(attempt as i32)
                .bind(retry_of.map(|r| r.0))
                .bind(benchmark.map(|b| b.0))
                .execute(&self.pool)
                .await?;
            }
            SessionEvent::AgentSessionStarted { workdir, branch, .. } => {
                sqlx::query(&Self::guarded("status = 'running', workdir = $3, branch = $4"))
                    .bind(id)
                    .bind(seq)
                    .bind(workdir)
                    .bind(branch)
                    .execute(&self.pool)
                    .await?;
            }
            SessionEvent::AgentRuntimeReady { model, .. } => {
                sqlx::query(&Self::guarded("model = COALESCE($3, model)"))
                    .bind(id)
                    .bind(seq)
                    .bind(model)
                    .execute(&self.pool)
                    .await?;
            }
            SessionEvent::AgentToolCalled { ok, .. } => {
                sqlx::query(&Self::guarded("tool_calls = tool_calls + 1, tool_errors = tool_errors + $3"))
                    .bind(id)
                    .bind(seq)
                    .bind(if ok { 0 } else { 1 })
                    .execute(&self.pool)
                    .await?;
            }
            SessionEvent::AgentUsageReported { usage, cost_usd } => {
                sqlx::query(&Self::guarded(
                    "input_tokens = $3, output_tokens = $4, cache_read_tokens = $5, cache_write_tokens = $6, cost_usd = $7",
                ))
                .bind(id)
                .bind(seq)
                .bind(usage.input_tokens as i64)
                .bind(usage.output_tokens as i64)
                .bind(usage.cache_read_tokens as i64)
                .bind(usage.cache_write_tokens as i64)
                .bind(cost_usd)
                .execute(&self.pool)
                .await?;
            }
            SessionEvent::AgentSessionCompleted { num_turns, duration_ms, transcript, .. } => {
                sqlx::query(&Self::guarded(
                    "status = 'succeeded', ended_at = $3, num_turns = $4, duration_ms = $5, transcript = $6",
                ))
                .bind(id)
                .bind(seq)
                .bind(at)
                .bind(num_turns as i32)
                .bind(duration_ms as i64)
                .bind(transcript)
                .execute(&self.pool)
                .await?;
            }
            SessionEvent::AgentSessionFailed { failure_class, message, num_turns, duration_ms, transcript, .. } => {
                sqlx::query(&Self::guarded(
                    "status = 'failed', failure_class = $3, failure_message = $4, ended_at = $5, \
                     num_turns = $6, duration_ms = $7, transcript = $8",
                ))
                .bind(id)
                .bind(seq)
                .bind(failure_class.as_str())
                .bind(message)
                .bind(at)
                .bind(num_turns as i32)
                .bind(duration_ms as i64)
                .bind(transcript)
                .execute(&self.pool)
                .await?;
            }
        }
        Ok(())
    }
}

impl Projection for SessionFactsProjection {
    async fn reset(&self) -> anyhow::Result<()> {
        sqlx::query("TRUNCATE session_facts").execute(&self.pool).await?;
        Ok(())
    }
}
