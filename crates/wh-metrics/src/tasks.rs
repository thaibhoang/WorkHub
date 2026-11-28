use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;
use wh_core::{RecordedEvent, TaskId, task::TaskEvent};
use wh_eventstore::EventHandler;

use crate::Projection;

/// `tasks_view`: one row per task with status, current stage and pipeline counters.
/// Idempotent: each row stores the last applied `global_seq` and ignores older events.
pub struct TasksProjection {
    pool: PgPool,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TaskRow {
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
    pub pr_url: Option<String>,
    pub pr_state: Option<String>,
    pub parent_id: Option<Uuid>,
}

impl TasksProjection {
    pub const NAME: &'static str = "tasks_view";

    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn list(pool: &PgPool) -> anyhow::Result<Vec<TaskRow>> {
        Ok(sqlx::query_as(
            "SELECT task_id, title, source, factory, status, current_stage, rework_loops, gate_rejections, \
             blocked_reason, created_at, updated_at, completed_at, pr_url, pr_state, parent_id FROM tasks_view WHERE benchmark_run IS NULL \
             ORDER BY created_at DESC",
        )
        .fetch_all(pool)
        .await?)
    }

    /// UPDATE guarded by `last_seq`: `$1` task id, `$2` seq, `$3` occurred_at, `$4..` caller binds.
    fn guarded(set: &str) -> String {
        format!(
            "UPDATE tasks_view SET {set}, updated_at = $3, last_seq = $2 WHERE task_id = $1 AND last_seq < $2"
        )
    }

    async fn exec(&self, sql: &str, id: Uuid, ev: &RecordedEvent) -> anyhow::Result<()> {
        sqlx::query(sql).bind(id).bind(ev.global_seq).bind(ev.meta.occurred_at).execute(&self.pool).await?;
        Ok(())
    }
}

impl EventHandler for TasksProjection {
    fn name(&self) -> &str {
        Self::NAME
    }

    async fn handle(&mut self, ev: &RecordedEvent) -> anyhow::Result<()> {
        let Some(TaskId(id)) = TaskId::from_stream_id(&ev.stream_id) else {
            return Ok(());
        };
        match ev.decode::<TaskEvent>()? {
            TaskEvent::TaskCreated { title, source, factory, benchmark, external_ref, parent, draft, .. } => {
                sqlx::query(
                    "INSERT INTO tasks_view \
                     (task_id, title, source, factory, status, current_stage, created_at, updated_at, last_seq, \
                      benchmark_run, benchmark_case, external_ref, parent_id) \
                     VALUES ($1, $2, $3, $4, $10, NULL, $5, $5, $6, $7, $8, $9, $11) ON CONFLICT (task_id) DO NOTHING",
                )
                .bind(id)
                .bind(title)
                .bind(source.as_str())
                .bind(factory)
                .bind(ev.meta.occurred_at)
                .bind(ev.global_seq)
                .bind(benchmark.as_ref().map(|b| b.run.0))
                .bind(benchmark.map(|b| b.case))
                .bind(external_ref.map(|e| format!("{}#{}", e.repo, e.issue)))
                .bind(if draft { "draft" } else { "open" })
                .bind(parent.map(|p| p.0))
                .execute(&self.pool)
                .await?;
            }
            TaskEvent::TaskActivated {} => self.exec(&Self::guarded("status = 'open'"), id, ev).await?,
            TaskEvent::TaskMoved { parent } => {
                sqlx::query(&Self::guarded("parent_id = $4"))
                    .bind(id)
                    .bind(ev.global_seq)
                    .bind(ev.meta.occurred_at)
                    .bind(parent.map(|p| p.0))
                    .execute(&self.pool)
                    .await?;
            }
            TaskEvent::StageEntered { stage, feedback } => {
                // A PR rework starts while shipped, or after the conflict gate: no stage
                // has run since the push (`current_stage` is empty) and the PR is open.
                sqlx::query(&Self::guarded(
                    "rework_loops = rework_loops + CASE WHEN status = 'shipped' \
                       OR (current_stage IS NULL AND pr_state = 'open') THEN 0 ELSE $5 END, \
                     pr_rework_loops = pr_rework_loops + CASE WHEN status = 'shipped' \
                       OR (current_stage IS NULL AND pr_state = 'open') THEN 1 ELSE 0 END, \
                     status = 'running', current_stage = $4, blocked_reason = NULL",
                ))
                .bind(id)
                .bind(ev.global_seq)
                .bind(ev.meta.occurred_at)
                .bind(stage)
                .bind(i32::from(feedback.is_some()))
                .execute(&self.pool)
                .await?;
            }
            TaskEvent::StageExited { .. } => self.exec(&Self::guarded("status = status"), id, ev).await?,
            TaskEvent::GateRequested { .. } => self.exec(&Self::guarded("status = 'awaiting_gate'"), id, ev).await?,
            TaskEvent::AgentAsked { .. } => self.exec(&Self::guarded("status = 'awaiting_input'"), id, ev).await?,
            TaskEvent::UserAnswered { .. } | TaskEvent::StageResumed { .. } => self.exec(&Self::guarded("status = 'running'"), id, ev).await?,
            TaskEvent::GateDecided { approved, .. } => {
                sqlx::query(&Self::guarded("status = 'running', gate_rejections = gate_rejections + $4"))
                    .bind(id)
                    .bind(ev.global_seq)
                    .bind(ev.meta.occurred_at)
                    .bind(i32::from(!approved))
                    .execute(&self.pool)
                    .await?;
            }
            TaskEvent::TaskBlocked { reason, .. } => {
                sqlx::query(&Self::guarded("status = 'blocked', blocked_reason = $4"))
                    .bind(id)
                    .bind(ev.global_seq)
                    .bind(ev.meta.occurred_at)
                    .bind(reason)
                    .execute(&self.pool)
                    .await?;
            }
            TaskEvent::TaskResumed { .. } => {
                self.exec(&Self::guarded("status = 'open', blocked_reason = NULL"), id, ev).await?
            }
            TaskEvent::TaskCompleted { .. } => {
                self.exec(&Self::guarded("status = 'done', current_stage = NULL, completed_at = $3"), id, ev).await?
            }
            TaskEvent::TaskCancelled { .. } => self.exec(&Self::guarded("status = 'cancelled'"), id, ev).await?,
            TaskEvent::TaskShipped { pr_url, pr_number, .. } => {
                sqlx::query(&Self::guarded(
                    "status = 'shipped', current_stage = NULL, blocked_reason = NULL, pr_url = $4, pr_number = $5, \
                     pr_state = 'open', shipped_at = COALESCE(shipped_at, $3)",
                ))
                .bind(id)
                .bind(ev.global_seq)
                .bind(ev.meta.occurred_at)
                .bind(pr_url)
                .bind(pr_number as i32)
                .execute(&self.pool)
                .await?;
            }
            TaskEvent::PrReviewed { state, submitted_at, .. } => {
                sqlx::query(&Self::guarded(
                    "pr_reviews = pr_reviews + 1, changes_requested = changes_requested + $5, \
                     first_review_at = COALESCE(first_review_at, $4)",
                ))
                .bind(id)
                .bind(ev.global_seq)
                .bind(ev.meta.occurred_at)
                .bind(submitted_at.unwrap_or(ev.meta.occurred_at))
                .bind(i32::from(state == wh_core::task::CHANGES_REQUESTED))
                .execute(&self.pool)
                .await?;
            }
            TaskEvent::PrChecksCompleted { conclusion, .. } => {
                sqlx::query(&Self::guarded("ci_failures = ci_failures + $4"))
                    .bind(id)
                    .bind(ev.global_seq)
                    .bind(ev.meta.occurred_at)
                    .bind(i32::from(conclusion != "success"))
                    .execute(&self.pool)
                    .await?;
            }
            TaskEvent::PrConflicted { .. } => self.exec(&Self::guarded("status = status"), id, ev).await?,
            TaskEvent::PrMerged { merged_at, .. } => {
                let at = merged_at.unwrap_or(ev.meta.occurred_at);
                sqlx::query(&Self::guarded(
                    "status = 'done', current_stage = NULL, pr_state = 'merged', merged_at = $4, completed_at = $4",
                ))
                .bind(id)
                .bind(ev.global_seq)
                .bind(ev.meta.occurred_at)
                .bind(at)
                .execute(&self.pool)
                .await?;
            }
            TaskEvent::PrClosed { .. } => {
                sqlx::query(&Self::guarded(
                    "pr_state = 'closed', \
                     status = CASE WHEN status IN ('done', 'cancelled') THEN status ELSE 'blocked' END, \
                     blocked_reason = CASE WHEN status IN ('done', 'cancelled') THEN blocked_reason \
                       ELSE 'pull request closed without merging' END",
                ))
                .bind(id)
                .bind(ev.global_seq)
                .bind(ev.meta.occurred_at)
                .execute(&self.pool)
                .await?;
            }
        }
        Ok(())
    }
}

impl Projection for TasksProjection {
    async fn reset(&self) -> anyhow::Result<()> {
        sqlx::query("TRUNCATE tasks_view").execute(&self.pool).await?;
        Ok(())
    }
}
