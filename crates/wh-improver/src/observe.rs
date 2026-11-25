//! Observe: what the improver gets to see. [`gather`] reads read models (and,
//! for human/review feedback text, the task streams in the log); [`render`] is
//! pure and turns it into a bounded markdown report.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;
use wh_metrics::proposals::{self, VersionStats};

use crate::prompt::{ScorerMetric, StageMetric};

const MAX_TEXT: usize = 600;
const MAX_TRANSCRIPT: usize = 2_500;
const MAX_EXAMPLES: i64 = 4;

#[derive(Debug, Clone, Default)]
pub struct Observation {
    pub factory: String,
    pub factory_version: String,
    pub window_days: u32,
    pub versions: Vec<VersionStats>,
    pub stages: Vec<StageRow>,
    pub failures: Vec<FailureRow>,
    pub scorers: Vec<ScorerRow>,
    pub score_findings: Vec<ScoreFinding>,
    pub feedback: Vec<Feedback>,
    pub past_proposals: Vec<PastProposal>,
    pub examples: Vec<Example>,
}

impl Observation {
    /// Finished sessions in the window (all versions).
    pub fn sessions(&self) -> u64 {
        self.stages.iter().map(|s| s.finished.max(0) as u64).sum()
    }

    /// Stage metrics of the current version (trigger input).
    pub fn current_stage_metrics(&self) -> Vec<StageMetric> {
        self.stages
            .iter()
            .filter(|s| s.factory_version == self.factory_version)
            .map(|s| StageMetric {
                stage: s.stage.clone(),
                finished: s.finished,
                failed: s.failed,
                rejected: s.rejected,
                avg_cost_usd: s.avg_cost_usd,
            })
            .collect()
    }

    pub fn current_scorer_metrics(&self) -> Vec<ScorerMetric> {
        self.scorers
            .iter()
            .filter(|s| s.factory_version == self.factory_version)
            .map(|s| ScorerMetric { scorer: s.scorer.clone(), scored: s.scored, failed: s.scored - s.passed })
            .collect()
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StageRow {
    pub factory_version: String,
    pub stage: String,
    pub agent: String,
    pub finished: i64,
    pub failed: i64,
    pub rejected: i64,
    pub retries: i64,
    pub avg_cost_usd: f64,
    pub p50_duration_ms: Option<f64>,
    pub tool_calls: i64,
    pub tool_errors: i64,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FailureRow {
    pub stage: String,
    pub failure_class: String,
    pub count: i64,
    pub sample: Option<String>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ScorerRow {
    pub factory_version: String,
    pub scorer: String,
    pub scored: i64,
    pub passed: i64,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ScoreFinding {
    pub scorer: String,
    pub classification: Option<String>,
    pub count: i64,
    pub sample_rationale: Option<String>,
    pub sample_session: Uuid,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Feedback {
    pub kind: String,
    pub stage: Option<String>,
    pub text: Option<String>,
    pub task_title: Option<String>,
    pub session: Option<String>,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PastProposal {
    pub status: String,
    pub title: Option<String>,
    pub message: Option<String>,
    pub decision_comment: Option<String>,
    pub files: serde_json::Value,
    pub requested_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct Example {
    pub session: Uuid,
    pub agent: String,
    pub stage: String,
    pub why: String,
    pub transcript: Option<String>,
}

#[derive(sqlx::FromRow)]
struct ExampleRow {
    session_id: Uuid,
    agent: String,
    stage: String,
    why: String,
    transcript: Option<String>,
}

/// Read everything the improver needs about `factory` over the last `window_days`.
pub async fn gather(pool: &PgPool, factory: &str, version: &str, window_days: u32) -> anyhow::Result<Observation> {
    let since = Utc::now() - chrono::Duration::days(window_days as i64);
    // Finished sessions of this factory's tasks within the window.
    const S: &str = "SELECT f.* FROM session_facts f JOIN tasks_view t ON t.task_id = f.task_id \
                     WHERE t.factory = $1 AND t.benchmark_run IS NULL AND f.dispatched_at >= $2 AND f.status IN ('succeeded', 'failed')";

    let stages: Vec<StageRow> = sqlx::query_as(&format!(
        "WITH s AS ({S}) SELECT COALESCE(factory_version, '-') AS factory_version, stage, min(agent) AS agent, \
           count(*) AS finished, count(*) FILTER (WHERE status = 'failed') AS failed, \
           count(*) FILTER (WHERE stage_outcome = 'rejected') AS rejected, \
           count(*) FILTER (WHERE attempt > 1) AS retries, avg(cost_usd) AS avg_cost_usd, \
           percentile_cont(0.5) WITHIN GROUP (ORDER BY duration_ms) AS p50_duration_ms, \
           sum(tool_calls)::int8 AS tool_calls, sum(tool_errors)::int8 AS tool_errors \
         FROM s GROUP BY 1, 2 ORDER BY min(dispatched_at), 2"
    ))
    .bind(factory)
    .bind(since)
    .fetch_all(pool)
    .await?;

    let failures: Vec<FailureRow> = sqlx::query_as(&format!(
        "WITH s AS ({S}) SELECT stage, COALESCE(failure_class, 'unknown') AS failure_class, count(*) AS count, \
           (array_agg(failure_message ORDER BY dispatched_at DESC))[1] AS sample \
         FROM s WHERE status = 'failed' GROUP BY 1, 2 ORDER BY 3 DESC LIMIT 15"
    ))
    .bind(factory)
    .bind(since)
    .fetch_all(pool)
    .await?;

    let scorers: Vec<ScorerRow> = sqlx::query_as(&format!(
        "WITH s AS ({S}) SELECT COALESCE(s.factory_version, '-') AS factory_version, r.scorer, \
           count(*) AS scored, count(*) FILTER (WHERE r.pass) AS passed \
         FROM score_results r JOIN s ON s.session_id = r.target_session WHERE r.status = 'scored' \
         GROUP BY 1, 2 ORDER BY 2, min(r.started_at)"
    ))
    .bind(factory)
    .bind(since)
    .fetch_all(pool)
    .await?;

    let score_findings: Vec<ScoreFinding> = sqlx::query_as(&format!(
        "WITH s AS ({S}) SELECT r.scorer, r.classification, count(*) AS count, \
           (array_agg(r.rationale ORDER BY r.started_at DESC))[1] AS sample_rationale, \
           (array_agg(r.target_session ORDER BY r.started_at DESC))[1] AS sample_session \
         FROM score_results r JOIN s ON s.session_id = r.target_session \
         WHERE r.status = 'scored' AND NOT r.pass GROUP BY 1, 2 ORDER BY 3 DESC LIMIT 12"
    ))
    .bind(factory)
    .bind(since)
    .fetch_all(pool)
    .await?;

    // Review rejections, human gate rejections, blocks and resume guidance (text lives in the log).
    let feedback: Vec<Feedback> = sqlx::query_as(
        "SELECT e.event_type AS kind, e.payload->>'stage' AS stage, \
           COALESCE(e.payload->>'summary', e.payload->>'comment', e.payload->>'reason') AS text, \
           t.title AS task_title, e.payload->>'session' AS session, e.recorded_at AS at \
         FROM events e JOIN tasks_view t ON e.stream_id = 'task-' || t.task_id::text \
         WHERE t.factory = $1 AND t.benchmark_run IS NULL AND e.recorded_at >= $2 \
           AND ((e.event_type = 'StageExited' AND e.payload->>'outcome' = 'rejected') \
             OR (e.event_type = 'GateDecided' AND e.payload->>'approved' = 'false') \
             OR e.event_type = 'TaskBlocked' \
             OR (e.event_type = 'TaskResumed' AND e.payload->>'comment' IS NOT NULL)) \
         ORDER BY e.global_seq DESC LIMIT 20",
    )
    .bind(factory)
    .bind(since)
    .fetch_all(pool)
    .await?;

    let past_proposals: Vec<PastProposal> = sqlx::query_as(
        "SELECT status, title, message, decision_comment, files, requested_at FROM proposals_view \
         WHERE factory = $1 AND status NOT IN ('requested', 'running') ORDER BY requested_at DESC LIMIT 10",
    )
    .bind(factory)
    .fetch_all(pool)
    .await?;

    // A few concrete bad sessions, newest first: failed, rejected, or failing a scorer.
    let rows: Vec<ExampleRow> = sqlx::query_as(&format!(
        "WITH s AS ({S}), bad AS ( \
           SELECT s.session_id, s.agent, s.stage, s.transcript, s.dispatched_at, \
             CASE WHEN s.status = 'failed' THEN 'failed [' || COALESCE(s.failure_class, 'unknown') || ']: ' \
                    || left(COALESCE(s.failure_message, ''), 300) \
                  WHEN s.stage_outcome = 'rejected' THEN 'verdict REJECT' \
                  ELSE 'failed scorer ' || ( \
                    SELECT string_agg(r.scorer, ', ') FROM score_results r \
                    WHERE r.target_session = s.session_id AND r.status = 'scored' AND NOT r.pass) END AS why \
           FROM s WHERE s.status = 'failed' OR s.stage_outcome = 'rejected' OR EXISTS ( \
             SELECT 1 FROM score_results r WHERE r.target_session = s.session_id AND r.status = 'scored' AND NOT r.pass)) \
         SELECT session_id, agent, stage, why, transcript FROM bad ORDER BY dispatched_at DESC LIMIT $3"
    ))
    .bind(factory)
    .bind(since)
    .bind(MAX_EXAMPLES)
    .fetch_all(pool)
    .await?;
    let mut examples = Vec::new();
    for r in rows {
        let transcript = match &r.transcript {
            Some(p) => tokio::fs::read_to_string(p).await.ok().map(|t| wh_scorer::condense_transcript(&t)),
            None => None,
        };
        examples.push(Example { session: r.session_id, agent: r.agent, stage: r.stage, why: r.why, transcript });
    }

    Ok(Observation {
        factory: factory.to_owned(),
        factory_version: version.to_owned(),
        window_days,
        versions: proposals::version_stats(pool, Some(factory)).await?,
        stages,
        failures,
        scorers,
        score_findings,
        feedback,
        past_proposals,
        examples,
    })
}

/// Stage and scorer metrics of `version` within the window (trigger input).
pub async fn current_metrics(
    pool: &PgPool,
    factory: &str,
    version: &str,
    window_days: u32,
) -> anyhow::Result<(Vec<StageMetric>, Vec<ScorerMetric>)> {
    let since = Utc::now() - chrono::Duration::days(window_days as i64);
    let stages: Vec<(String, i64, i64, i64, f64)> = sqlx::query_as(
        "SELECT f.stage, count(*), count(*) FILTER (WHERE f.status = 'failed'), \
           count(*) FILTER (WHERE f.stage_outcome = 'rejected'), COALESCE(avg(f.cost_usd), 0) \
         FROM session_facts f JOIN tasks_view t ON t.task_id = f.task_id \
         WHERE t.factory = $1 AND t.benchmark_run IS NULL AND f.factory_version = $2 AND f.dispatched_at >= $3 \
           AND f.status IN ('succeeded', 'failed') GROUP BY 1",
    )
    .bind(factory)
    .bind(version)
    .bind(since)
    .fetch_all(pool)
    .await?;
    let scorers: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT r.scorer, count(*), count(*) FILTER (WHERE NOT r.pass) \
         FROM score_results r JOIN session_facts f ON f.session_id = r.target_session \
         JOIN tasks_view t ON t.task_id = f.task_id \
         WHERE t.factory = $1 AND t.benchmark_run IS NULL AND f.factory_version = $2 AND f.dispatched_at >= $3 AND r.status = 'scored' \
         GROUP BY 1",
    )
    .bind(factory)
    .bind(version)
    .bind(since)
    .fetch_all(pool)
    .await?;
    Ok((
        stages
            .into_iter()
            .map(|(stage, finished, failed, rejected, avg_cost_usd)| StageMetric { stage, finished, failed, rejected, avg_cost_usd })
            .collect(),
        scorers.into_iter().map(|(scorer, scored, failed)| ScorerMetric { scorer, scored, failed }).collect(),
    ))
}

/// Markdown report for the improver prompt (and `wh improve --dry-run`).
pub fn render(o: &Observation) -> String {
    let mut p = String::new();
    p.push_str(&format!(
        "Factory `{}`, current version `{}`. Window: last {} days, {} finished sessions.\n",
        o.factory,
        o.factory_version,
        o.window_days,
        o.sessions()
    ));

    if !o.versions.is_empty() {
        p.push_str("\n## Metrics by factory version (all time, oldest first)\n\n");
        p.push_str("| version | tasks | sessions | fail% | reject% | retries | avg $ | p50 | tool err% | scorer pass% |\n");
        p.push_str("|---|---|---|---|---|---|---|---|---|---|\n");
        for v in &o.versions {
            let cur = if v.factory_version == o.factory_version { " (current)" } else { "" };
            p.push_str(&format!(
                "| {}{cur} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
                v.factory_version,
                v.tasks,
                v.sessions,
                pct(v.fail_rate),
                pct(v.reject_rate),
                v.retries,
                usd(v.avg_cost_usd),
                dur(v.p50_duration_ms),
                pct(v.tool_error_rate),
                if v.scored > 0 { format!("{} (n={})", pct(v.score_pass_rate), v.scored) } else { "-".into() },
            ));
        }
    }

    p.push_str("\n## Stages in the window\n\n");
    if o.stages.is_empty() {
        p.push_str("No finished sessions in the window.\n");
    } else {
        p.push_str("| version | stage | agent | finished | failed | rejected | retries | avg $ | p50 | tool err |\n");
        p.push_str("|---|---|---|---|---|---|---|---|---|---|\n");
        for s in &o.stages {
            p.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {}/{} |\n",
                s.factory_version,
                s.stage,
                s.agent,
                s.finished,
                s.failed,
                s.rejected,
                s.retries,
                usd(Some(s.avg_cost_usd)),
                dur(s.p50_duration_ms),
                s.tool_errors,
                s.tool_calls,
            ));
        }
    }

    if !o.failures.is_empty() {
        p.push_str("\n## Session failures in the window\n\n");
        for f in &o.failures {
            p.push_str(&format!(
                "- `{}` × {} at stage `{}` — latest: {}\n",
                f.failure_class,
                f.count,
                f.stage,
                one_line(f.sample.as_deref().unwrap_or(""), 300)
            ));
        }
    }

    if !o.scorers.is_empty() {
        p.push_str("\n## Scorers (LLM-as-judge) in the window\n\n");
        for s in &o.scorers {
            p.push_str(&format!(
                "- `{}` on version {}: {}/{} passed\n",
                s.scorer, s.factory_version, s.passed, s.scored
            ));
        }
        for f in &o.score_findings {
            p.push_str(&format!(
                "  - FAIL `{}` [{}] × {} — e.g. session {}: {}\n",
                f.scorer,
                f.classification.as_deref().unwrap_or("-"),
                f.count,
                f.sample_session,
                one_line(f.sample_rationale.as_deref().unwrap_or(""), 300)
            ));
        }
    }

    if !o.feedback.is_empty() {
        p.push_str("\n## Review rejections, human feedback and blocks (newest first)\n\n");
        for f in &o.feedback {
            let kind = match f.kind.as_str() {
                "StageExited" => "review REJECT",
                "GateDecided" => "human gate rejected",
                "TaskBlocked" => "task blocked",
                "TaskResumed" => "human resumed with guidance",
                k => k,
            };
            p.push_str(&format!(
                "- {kind}{} on \"{}\"{}: {}\n",
                f.stage.as_deref().map(|s| format!(" at `{s}`")).unwrap_or_default(),
                f.task_title.as_deref().unwrap_or("?"),
                f.session.as_deref().map(|s| format!(" (session {s})")).unwrap_or_default(),
                one_line(f.text.as_deref().unwrap_or(""), MAX_TEXT)
            ));
        }
    }

    p.push_str("\n## Previous proposals (newest first)\n\n");
    if o.past_proposals.is_empty() {
        p.push_str("None yet.\n");
    } else {
        p.push_str("Do not re-propose something a human rejected unless you have new evidence.\n\n");
        for pp in &o.past_proposals {
            let files: Vec<&str> =
                pp.files.as_array().map(|a| a.iter().filter_map(|f| f.as_str()).collect()).unwrap_or_default();
            p.push_str(&format!(
                "- [{}] {}{}{}{}\n",
                pp.status,
                pp.title.as_deref().unwrap_or("(no change)"),
                if files.is_empty() { String::new() } else { format!(" — files: {}", files.join(", ")) },
                pp.decision_comment.as_deref().map(|c| format!(" — reviewer: {}", one_line(c, 300))).unwrap_or_default(),
                pp.message.as_deref().map(|m| format!(" — {}", one_line(m, 200))).unwrap_or_default(),
            ));
        }
    }

    if !o.examples.is_empty() {
        p.push_str("\n## Example sessions (newest first)\n");
        for e in &o.examples {
            p.push_str(&format!("\n### Session {} — `{}` at `{}`: {}\n\n", e.session, e.agent, e.stage, one_line(&e.why, 400)));
            match e.transcript.as_deref().filter(|t| !t.trim().is_empty()) {
                Some(t) => p.push_str(&format!("```\n{}\n```\n", tail(t, MAX_TRANSCRIPT))),
                None => p.push_str("(no transcript)\n"),
            }
        }
    }
    p
}

fn pct(v: Option<f64>) -> String {
    v.map(|x| format!("{:.0}%", x * 100.0)).unwrap_or_else(|| "-".into())
}

fn usd(v: Option<f64>) -> String {
    v.map(|x| format!("${x:.3}")).unwrap_or_else(|| "-".into())
}

fn dur(ms: Option<f64>) -> String {
    match ms {
        None => "-".into(),
        Some(ms) if ms < 60_000.0 => format!("{:.0}s", ms / 1000.0),
        Some(ms) => format!("{:.1}m", ms / 60_000.0),
    }
}

fn one_line(s: &str, max: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s,
    }
}

fn tail(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_owned();
    }
    let skip = s.char_indices().nth(n - max).map(|(i, _)| i).unwrap_or(0);
    format!("[… truncated]\n{}", &s[skip..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stage(version: &str, stage: &str, finished: i64, failed: i64) -> StageRow {
        StageRow {
            factory_version: version.into(),
            stage: stage.into(),
            agent: "a".into(),
            finished,
            failed,
            rejected: 1,
            retries: 0,
            avg_cost_usd: 0.25,
            p50_duration_ms: Some(90_000.0),
            tool_calls: 10,
            tool_errors: 2,
        }
    }

    #[test]
    fn renders_and_derives_current_metrics() {
        let o = Observation {
            factory: "f".into(),
            factory_version: "v2".into(),
            window_days: 7,
            stages: vec![stage("v1", "impl", 4, 2), stage("v2", "impl", 6, 1)],
            failures: vec![FailureRow {
                stage: "impl".into(),
                failure_class: "tool_error".into(),
                count: 3,
                sample: Some("permission\n denied   chmod".into()),
            }],
            scorers: vec![ScorerRow { factory_version: "v2".into(), scorer: "s".into(), scored: 4, passed: 1 }],
            feedback: vec![Feedback {
                kind: "GateDecided".into(),
                stage: Some("spec".into()),
                text: Some("too vague".into()),
                task_title: Some("Add X".into()),
                session: None,
                at: Utc::now(),
            }],
            examples: vec![Example {
                session: Uuid::nil(),
                agent: "a".into(),
                stage: "impl".into(),
                why: "verdict REJECT".into(),
                transcript: Some("→ Bash ls\n".into()),
            }],
            ..Default::default()
        };
        assert_eq!(o.sessions(), 10);
        assert_eq!(o.current_stage_metrics().len(), 1);
        assert_eq!(o.current_stage_metrics()[0].failed, 1);
        assert_eq!(o.current_scorer_metrics()[0].failed, 3);
        let r = render(&o);
        assert!(r.contains("10 finished sessions"));
        assert!(r.contains("| v2 | impl | a | 6 | 1 | 1 | 0 | $0.250 | 1.5m | 2/10 |"));
        assert!(r.contains("`tool_error` × 3 at stage `impl` — latest: permission denied chmod"));
        assert!(r.contains("human gate rejected at `spec` on \"Add X\": too vague"));
        assert!(r.contains("Previous proposals"));
        assert!(r.contains("→ Bash ls"));
    }

    #[test]
    fn truncation_helpers_are_char_safe() {
        assert_eq!(one_line("éé é", 2), "éé…");
        assert!(tail("ééé", 2).ends_with("éé"));
    }
}
