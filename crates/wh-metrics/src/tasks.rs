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
}

impl TasksProjection {
    pub const NAME: &'static str = "tasks_view";

    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn list(pool: &PgPool) -> anyhow::Result<Vec<TaskRow>> {
        Ok(sqlx::query_as(
            "SELECT task_id, title, source, factory, status, current_stage, rework_loops, gate_rejections, \
             blocked_reason, created_at, updated_at, completed_at FROM tasks_view ORDER BY created_at DESC",
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
            TaskEvent::TaskCreated { title, source, factory, .. } => {
                sqlx::query(
                    "INSERT INTO tasks_view \
                     (task_id, title, source, factory, status, current_stage, created_at, updated_at, last_seq) \
                     VALUES ($1, $2, $3, $4, 'open', NULL, $5, $5, $6) ON CONFLICT (task_id) DO NOTHING",
                )
                .bind(id)
                .bind(title)
                .bind(source.as_str())
                .bind(factory)
                .bind(ev.meta.occurred_at)
                .bind(ev.global_seq)
                .execute(&self.pool)
                .await?;
            }
            TaskEvent::StageEntered { stage, feedback } => {
                sqlx::query(&Self::guarded(
                    "status = 'running', current_stage = $4, blocked_reason = NULL, \
                     rework_loops = rework_loops + $5",
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
