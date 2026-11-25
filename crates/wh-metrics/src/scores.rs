use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;
use wh_core::{RecordedEvent, ScoreId, score::ScoreEvent};
use wh_eventstore::EventHandler;

use crate::Projection;

/// `score_results`: one row per scorer run. Idempotent via `last_seq`.
pub struct ScoreResultsProjection {
    pool: PgPool,
}

impl ScoreResultsProjection {
    pub const NAME: &'static str = "score_results";

    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    fn guarded(set_clause: &str) -> String {
        format!("UPDATE score_results SET {set_clause}, last_seq = $2 WHERE score_id = $1 AND last_seq < $2")
    }
}

impl EventHandler for ScoreResultsProjection {
    fn name(&self) -> &str {
        Self::NAME
    }

    async fn handle(&mut self, ev: &RecordedEvent) -> anyhow::Result<()> {
        let Some(ScoreId(id)) = ScoreId::from_stream_id(&ev.stream_id) else {
            return Ok(());
        };
        let (seq, at) = (ev.global_seq, ev.meta.occurred_at);
        match ev.decode::<ScoreEvent>()? {
            ScoreEvent::ScoreStarted { scorer, target_session, task_id, agent, stage, model, pass_threshold, manual } => {
                sqlx::query(
                    "INSERT INTO score_results (score_id, scorer, target_session, task_id, agent, stage, model, \
                     pass_threshold, manual, status, started_at, factory_version, last_seq) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'running', $10, $11, $12) \
                     ON CONFLICT (score_id) DO NOTHING",
                )
                .bind(id)
                .bind(scorer)
                .bind(target_session.0)
                .bind(task_id.0)
                .bind(agent)
                .bind(stage)
                .bind(model)
                .bind(pass_threshold)
                .bind(manual)
                .bind(at)
                .bind(&ev.meta.factory_version)
                .bind(seq)
                .execute(&self.pool)
                .await?;
            }
            ScoreEvent::ScoreRecorded { score, pass, classification, rationale, cost_usd, usage, duration_ms, transcript } => {
                sqlx::query(&Self::guarded(
                    "status = 'scored', score = $3, pass = $4, classification = $5, rationale = $6, cost_usd = $7, \
                     input_tokens = $8, output_tokens = $9, duration_ms = $10, transcript = $11, ended_at = $12",
                ))
                .bind(id)
                .bind(seq)
                .bind(score)
                .bind(pass)
                .bind(classification)
                .bind(rationale)
                .bind(cost_usd)
                .bind(usage.input_tokens as i64)
                .bind(usage.output_tokens as i64)
                .bind(duration_ms as i64)
                .bind(transcript)
                .bind(at)
                .execute(&self.pool)
                .await?;
            }
            ScoreEvent::ScoreFailed { message, cost_usd, duration_ms, transcript } => {
                sqlx::query(&Self::guarded(
                    "status = 'failed', error = $3, cost_usd = $4, duration_ms = $5, transcript = $6, ended_at = $7",
                ))
                .bind(id)
                .bind(seq)
                .bind(message)
                .bind(cost_usd)
                .bind(duration_ms as i64)
                .bind(transcript)
                .bind(at)
                .execute(&self.pool)
                .await?;
            }
        }
        Ok(())
    }
}

impl Projection for ScoreResultsProjection {
    async fn reset(&self) -> anyhow::Result<()> {
        sqlx::query("TRUNCATE score_results").execute(&self.pool).await?;
        Ok(())
    }
}

// ---------- queries ----------

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ScoreRow {
    pub score_id: Uuid,
    pub scorer: String,
    pub target_session: Uuid,
    pub task_id: Uuid,
    pub task_title: Option<String>,
    pub agent: String,
    pub stage: String,
    pub model: Option<String>,
    pub pass_threshold: f64,
    pub manual: bool,
    pub status: String,
    pub score: Option<f64>,
    pub pass: Option<bool>,
    pub classification: Option<String>,
    pub rationale: Option<String>,
    pub error: Option<String>,
    pub cost_usd: f64,
    pub duration_ms: Option<i64>,
    pub transcript: Option<String>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub factory_version: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct ScoreFilter {
    pub scorer: Option<String>,
    pub task: Option<Uuid>,
    pub session: Option<Uuid>,
    /// Only scored results that did not pass.
    pub failing: bool,
}

/// Scorer runs, newest first.
pub async fn scores(pool: &PgPool, f: &ScoreFilter, limit: i64) -> anyhow::Result<Vec<ScoreRow>> {
    Ok(sqlx::query_as(
        "SELECT r.score_id, r.scorer, r.target_session, r.task_id, t.title AS task_title, r.agent, r.stage, r.model, \
           r.pass_threshold, r.manual, r.status, r.score, r.pass, r.classification, r.rationale, r.error, r.cost_usd, \
           r.duration_ms, r.transcript, r.started_at, r.ended_at, r.factory_version \
         FROM score_results r LEFT JOIN tasks_view t ON t.task_id = r.task_id \
         WHERE ($1::text IS NULL OR r.scorer = $1) AND ($2::uuid IS NULL OR r.task_id = $2) \
           AND ($3::uuid IS NULL OR r.target_session = $3) AND (NOT $4 OR r.pass = false) \
         ORDER BY r.started_at DESC LIMIT $5",
    )
    .bind(&f.scorer)
    .bind(f.task)
    .bind(f.session)
    .bind(f.failing)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

pub async fn score(pool: &PgPool, id: Uuid) -> anyhow::Result<Option<ScoreRow>> {
    Ok(sqlx::query_as(
        "SELECT r.score_id, r.scorer, r.target_session, r.task_id, t.title AS task_title, r.agent, r.stage, r.model, \
           r.pass_threshold, r.manual, r.status, r.score, r.pass, r.classification, r.rationale, r.error, r.cost_usd, \
           r.duration_ms, r.transcript, r.started_at, r.ended_at, r.factory_version \
         FROM score_results r LEFT JOIN tasks_view t ON t.task_id = r.task_id WHERE r.score_id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

/// Per-scorer aggregates (all time, plus the last 7 days for trend).
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ScorerStats {
    pub scorer: String,
    pub agent: String,
    /// Finished runs (scored + failed).
    pub runs: i64,
    pub scored: i64,
    pub passed: i64,
    /// Judge could not produce a score.
    pub errors: i64,
    pub pass_rate: Option<f64>,
    pub avg_score: Option<f64>,
    pub pass_rate_7d: Option<f64>,
    pub scored_7d: i64,
    pub cost_usd: f64,
    pub avg_cost_usd: Option<f64>,
    pub last_run_at: Option<DateTime<Utc>>,
}

pub async fn scorer_stats(pool: &PgPool) -> anyhow::Result<Vec<ScorerStats>> {
    Ok(sqlx::query_as(
        "SELECT scorer, min(agent) AS agent, \
           count(*) FILTER (WHERE status <> 'running') AS runs, \
           count(*) FILTER (WHERE status = 'scored') AS scored, \
           count(*) FILTER (WHERE pass) AS passed, \
           count(*) FILTER (WHERE status = 'failed') AS errors, \
           (count(*) FILTER (WHERE pass))::float8 / NULLIF(count(*) FILTER (WHERE status = 'scored'), 0) AS pass_rate, \
           avg(score) AS avg_score, \
           (count(*) FILTER (WHERE pass AND started_at >= now() - interval '7 days'))::float8 \
             / NULLIF(count(*) FILTER (WHERE status = 'scored' AND started_at >= now() - interval '7 days'), 0) \
             AS pass_rate_7d, \
           count(*) FILTER (WHERE status = 'scored' AND started_at >= now() - interval '7 days') AS scored_7d, \
           COALESCE(sum(cost_usd), 0) AS cost_usd, \
           avg(cost_usd) FILTER (WHERE status <> 'running') AS avg_cost_usd, \
           max(ended_at) AS last_run_at \
         FROM score_results GROUP BY scorer ORDER BY scorer",
    )
    .fetch_all(pool)
    .await?)
}

/// Pass rate per scorer and the factory version that produced the *graded*
/// session: the before/after signal for a workflow change (phase 5).
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ScorerVersionStats {
    pub scorer: String,
    pub factory_version: Option<String>,
    pub scored: i64,
    pub passed: i64,
    pub avg_score: Option<f64>,
    pub first_at: DateTime<Utc>,
}

pub async fn scorer_stats_by_version(pool: &PgPool) -> anyhow::Result<Vec<ScorerVersionStats>> {
    Ok(sqlx::query_as(
        "SELECT r.scorer, s.factory_version, count(*) AS scored, count(*) FILTER (WHERE r.pass) AS passed, \
           avg(r.score) AS avg_score, min(r.started_at) AS first_at \
         FROM score_results r LEFT JOIN session_facts s ON s.session_id = r.target_session \
         WHERE r.status = 'scored' GROUP BY r.scorer, s.factory_version ORDER BY r.scorer, first_at",
    )
    .fetch_all(pool)
    .await?)
}
