use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;
use wh_core::{Actor, ProposalId, RecordedEvent, proposal::ProposalEvent};
use wh_eventstore::EventHandler;

use crate::Projection;

/// `proposals_view`: one row per improver run / proposal. Idempotent via `last_seq`.
pub struct ProposalsProjection {
    pool: PgPool,
}

impl ProposalsProjection {
    pub const NAME: &'static str = "proposals_view";

    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    fn guarded(set_clause: &str) -> String {
        format!("UPDATE proposals_view SET {set_clause}, last_seq = $2 WHERE proposal_id = $1 AND last_seq < $2")
    }
}

fn actor_name(a: &Actor) -> String {
    match a {
        Actor::Human { user } => user.clone(),
        Actor::Agent { session } => format!("agent:{session}"),
        Actor::System { component } => component.clone(),
    }
}

impl EventHandler for ProposalsProjection {
    fn name(&self) -> &str {
        Self::NAME
    }

    async fn handle(&mut self, ev: &RecordedEvent) -> anyhow::Result<()> {
        let Some(ProposalId(id)) = ProposalId::from_stream_id(&ev.stream_id) else {
            return Ok(());
        };
        let (seq, at) = (ev.global_seq, ev.meta.occurred_at);
        match ev.decode::<ProposalEvent>()? {
            ProposalEvent::ImprovementRequested { factory, trigger, focus } => {
                sqlx::query(
                    "INSERT INTO proposals_view (proposal_id, factory, status, trigger, focus, requested_by, \
                     requested_at, last_seq) VALUES ($1, $2, 'requested', $3, $4, $5, $6, $7) \
                     ON CONFLICT (proposal_id) DO NOTHING",
                )
                .bind(id)
                .bind(factory)
                .bind(trigger)
                .bind(focus)
                .bind(actor_name(&ev.meta.actor))
                .bind(at)
                .bind(seq)
                .execute(&self.pool)
                .await?;
            }
            ProposalEvent::ImprovementStarted { base_factory_version, sessions_observed, .. } => {
                sqlx::query(&Self::guarded(
                    "status = 'running', started_at = $3, base_factory_version = $4, sessions_observed = $5",
                ))
                .bind(id)
                .bind(seq)
                .bind(at)
                .bind(base_factory_version)
                .bind(sessions_observed as i64)
                .execute(&self.pool)
                .await?;
            }
            ProposalEvent::ProposalCreated {
                title,
                rationale,
                expected_impact,
                evidence,
                files,
                diff,
                branch,
                commit,
                proposed_factory_version,
                cost_usd,
                duration_ms,
                transcript,
                ..
            } => {
                let evidence: Vec<Uuid> = evidence.iter().map(|s| s.0).collect();
                sqlx::query(&Self::guarded(
                    "status = 'open', finished_at = $3, title = $4, rationale = $5, expected_impact = $6, \
                     evidence = $7, files = $8, diff = $9, branch = $10, commit_sha = $11, \
                     proposed_factory_version = $12, cost_usd = $13, duration_ms = $14, transcript = $15",
                ))
                .bind(id)
                .bind(seq)
                .bind(at)
                .bind(title)
                .bind(rationale)
                .bind(expected_impact)
                .bind(serde_json::to_value(evidence)?)
                .bind(serde_json::to_value(files)?)
                .bind(diff)
                .bind(branch)
                .bind(commit)
                .bind(proposed_factory_version)
                .bind(cost_usd)
                .bind(duration_ms as i64)
                .bind(transcript)
                .execute(&self.pool)
                .await?;
            }
            ProposalEvent::ImprovementSkipped { reason: message, cost_usd, duration_ms, transcript }
            | ProposalEvent::ImprovementFailed { message, cost_usd, duration_ms, transcript } => {
                let status = if ev.event_type == "ImprovementSkipped" { "no_proposal" } else { "failed" };
                sqlx::query(&Self::guarded(
                    "status = $3, finished_at = $4, message = $5, cost_usd = $6, duration_ms = $7, transcript = $8",
                ))
                .bind(id)
                .bind(seq)
                .bind(status)
                .bind(at)
                .bind(message)
                .bind(cost_usd)
                .bind(duration_ms as i64)
                .bind(transcript)
                .execute(&self.pool)
                .await?;
            }
            ProposalEvent::ProposalDecided { approved, reviewer, comment } => {
                sqlx::query(&Self::guarded("status = $3, decided_by = $4, decided_at = $5, decision_comment = $6"))
                    .bind(id)
                    .bind(seq)
                    .bind(if approved { "approved" } else { "rejected" })
                    .bind(reviewer)
                    .bind(at)
                    .bind(comment)
                    .execute(&self.pool)
                    .await?;
            }
            ProposalEvent::ProposalWithdrawn { reviewer, comment } => {
                sqlx::query(&Self::guarded(
                    "status = 'withdrawn', decided_by = $3, decided_at = $4, decision_comment = $5",
                ))
                .bind(id)
                .bind(seq)
                .bind(reviewer)
                .bind(at)
                .bind(comment)
                .execute(&self.pool)
                .await?;
            }
            ProposalEvent::ProposalRolledOut { factory_version, .. } => {
                sqlx::query(&Self::guarded("status = 'rolled_out', rolled_out_at = $3, rolled_out_version = $4"))
                    .bind(id)
                    .bind(seq)
                    .bind(at)
                    .bind(factory_version)
                    .execute(&self.pool)
                    .await?;
            }
        }
        Ok(())
    }
}

impl Projection for ProposalsProjection {
    async fn reset(&self) -> anyhow::Result<()> {
        sqlx::query("TRUNCATE proposals_view").execute(&self.pool).await?;
        Ok(())
    }
}

// ---------- queries ----------

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ProposalRow {
    pub proposal_id: Uuid,
    pub factory: String,
    pub status: String,
    pub trigger: String,
    pub focus: Option<String>,
    pub requested_by: String,
    pub requested_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub base_factory_version: Option<String>,
    pub sessions_observed: Option<i64>,
    pub title: Option<String>,
    pub rationale: Option<String>,
    pub expected_impact: Option<String>,
    pub evidence: serde_json::Value,
    pub files: serde_json::Value,
    pub diff: Option<String>,
    pub branch: Option<String>,
    pub commit_sha: Option<String>,
    pub proposed_factory_version: Option<String>,
    pub message: Option<String>,
    pub cost_usd: f64,
    pub duration_ms: Option<i64>,
    pub transcript: Option<String>,
    pub decided_by: Option<String>,
    pub decided_at: Option<DateTime<Utc>>,
    pub decision_comment: Option<String>,
    pub rolled_out_at: Option<DateTime<Utc>>,
    pub rolled_out_version: Option<String>,
}

const PROPOSAL_COLS: &str = "proposal_id, factory, status, trigger, focus, requested_by, requested_at, started_at, \
     finished_at, base_factory_version, sessions_observed, title, rationale, expected_impact, evidence, files, diff, \
     branch, commit_sha, proposed_factory_version, message, cost_usd, duration_ms, transcript, decided_by, decided_at, \
     decision_comment, rolled_out_at, rolled_out_version";

/// Improver runs, newest first. `status` may be a single status or `inbox`
/// (open proposals plus approved ones not rolled out yet).
pub async fn proposals(
    pool: &PgPool,
    factory: Option<&str>,
    status: Option<&str>,
    limit: i64,
) -> anyhow::Result<Vec<ProposalRow>> {
    Ok(sqlx::query_as(&format!(
        "SELECT {PROPOSAL_COLS} FROM proposals_view \
         WHERE ($1::text IS NULL OR factory = $1) \
           AND ($2::text IS NULL OR status = $2 OR ($2 = 'inbox' AND status IN ('open', 'approved'))) \
         ORDER BY requested_at DESC LIMIT $3"
    ))
    .bind(factory)
    .bind(status)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

pub async fn proposal(pool: &PgPool, id: Uuid) -> anyhow::Result<Option<ProposalRow>> {
    Ok(sqlx::query_as(&format!("SELECT {PROPOSAL_COLS} FROM proposals_view WHERE proposal_id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

/// How well the improver itself is doing.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ImproverStats {
    /// Finished runs (proposal, no proposal, or failed).
    pub runs: i64,
    pub proposals: i64,
    pub no_proposal: i64,
    pub failed: i64,
    pub open: i64,
    pub approved: i64,
    pub rejected: i64,
    pub rolled_out: i64,
    /// Approved (incl. rolled out) / decided.
    pub approval_rate: Option<f64>,
    pub cost_usd: f64,
    pub last_run_at: Option<DateTime<Utc>>,
}

pub async fn improver_stats(pool: &PgPool, factory: Option<&str>) -> anyhow::Result<ImproverStats> {
    Ok(sqlx::query_as(
        "SELECT \
           count(*) FILTER (WHERE status NOT IN ('requested', 'running')) AS runs, \
           count(*) FILTER (WHERE status IN ('open', 'approved', 'rejected', 'withdrawn', 'rolled_out')) AS proposals, \
           count(*) FILTER (WHERE status = 'no_proposal') AS no_proposal, \
           count(*) FILTER (WHERE status = 'failed') AS failed, \
           count(*) FILTER (WHERE status = 'open') AS open, \
           count(*) FILTER (WHERE status IN ('approved', 'rolled_out')) AS approved, \
           count(*) FILTER (WHERE status = 'rejected') AS rejected, \
           count(*) FILTER (WHERE status = 'rolled_out') AS rolled_out, \
           (count(*) FILTER (WHERE status IN ('approved', 'rolled_out')))::float8 \
             / NULLIF(count(*) FILTER (WHERE status IN ('approved', 'rejected', 'rolled_out')), 0) AS approval_rate, \
           COALESCE(sum(cost_usd), 0) AS cost_usd, \
           max(finished_at) AS last_run_at \
         FROM proposals_view WHERE ($1::text IS NULL OR factory = $1)",
    )
    .bind(factory)
    .fetch_one(pool)
    .await?)
}

/// Pipeline metrics per `factory_version`: the before/after view of a workflow change.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct VersionStats {
    pub factory_version: String,
    pub tasks: i64,
    /// Finished sessions (succeeded + failed).
    pub sessions: i64,
    pub failed: i64,
    pub retries: i64,
    /// Sessions whose stage verdict was REJECT (review loops).
    pub rejected: i64,
    pub fail_rate: Option<f64>,
    pub reject_rate: Option<f64>,
    pub avg_cost_usd: Option<f64>,
    pub cost_usd: f64,
    pub p50_duration_ms: Option<f64>,
    pub tool_error_rate: Option<f64>,
    pub scored: i64,
    pub score_pass_rate: Option<f64>,
    pub first_at: DateTime<Utc>,
    pub last_at: DateTime<Utc>,
}

/// One row per factory version that ran sessions, oldest first.
pub async fn version_stats(pool: &PgPool, factory: Option<&str>) -> anyhow::Result<Vec<VersionStats>> {
    Ok(sqlx::query_as(
        "WITH s AS ( \
           SELECT f.* FROM session_facts f LEFT JOIN tasks_view t ON t.task_id = f.task_id \
           WHERE f.factory_version IS NOT NULL AND f.status IN ('succeeded', 'failed') AND f.benchmark_run IS NULL \
             AND f.failure_class IS DISTINCT FROM 'usage_limit' \
             AND ($1::text IS NULL OR t.factory = $1)), \
         sc AS ( \
           SELECT s.factory_version, count(*) AS scored, count(*) FILTER (WHERE r.pass) AS passed \
           FROM score_results r JOIN s ON s.session_id = r.target_session \
           WHERE r.status = 'scored' GROUP BY s.factory_version) \
         SELECT s.factory_version, count(DISTINCT s.task_id) AS tasks, count(*) AS sessions, \
           count(*) FILTER (WHERE s.status = 'failed') AS failed, \
           count(*) FILTER (WHERE s.attempt > 1) AS retries, \
           count(*) FILTER (WHERE s.stage_outcome = 'rejected') AS rejected, \
           (count(*) FILTER (WHERE s.status = 'failed'))::float8 / count(*) AS fail_rate, \
           (count(*) FILTER (WHERE s.stage_outcome = 'rejected'))::float8 / count(*) AS reject_rate, \
           avg(s.cost_usd) AS avg_cost_usd, COALESCE(sum(s.cost_usd), 0) AS cost_usd, \
           percentile_cont(0.5) WITHIN GROUP (ORDER BY s.duration_ms) AS p50_duration_ms, \
           sum(s.tool_errors)::float8 / NULLIF(sum(s.tool_calls), 0) AS tool_error_rate, \
           COALESCE(min(sc.scored), 0) AS scored, \
           min(sc.passed)::float8 / NULLIF(min(sc.scored), 0) AS score_pass_rate, \
           min(s.dispatched_at) AS first_at, max(s.dispatched_at) AS last_at \
         FROM s LEFT JOIN sc ON sc.factory_version = s.factory_version \
         GROUP BY s.factory_version ORDER BY first_at",
    )
    .bind(factory)
    .fetch_all(pool)
    .await?)
}
