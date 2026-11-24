use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;
use wh_core::{RecordedEvent, TaskId, task::TaskEvent};
use wh_eventstore::EventHandler;

use crate::Projection;

/// `tasks_view`: one row per task with its current status and stage.
/// Idempotent: each row stores the last applied `global_seq` and ignores older events.
pub struct TasksProjection {
    pool: PgPool,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TaskRow {
    pub task_id: Uuid,
    pub title: String,
    pub source: String,
    pub status: String,
    pub current_stage: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl TasksProjection {
    pub const NAME: &'static str = "tasks_view";

    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn list(pool: &PgPool) -> anyhow::Result<Vec<TaskRow>> {
        Ok(sqlx::query_as(
            "SELECT task_id, title, source, status, current_stage, created_at, updated_at \
             FROM tasks_view ORDER BY created_at DESC",
        )
        .fetch_all(pool)
        .await?)
    }

    async fn touch(&self, id: Uuid, ev: &RecordedEvent, stage: Option<&str>, status: Option<&str>) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE tasks_view SET \
               current_stage = CASE WHEN $2 THEN $3 ELSE current_stage END, \
               status = COALESCE($4, status), \
               updated_at = $5, last_seq = $6 \
             WHERE task_id = $1 AND last_seq < $6",
        )
        .bind(id)
        .bind(stage.is_some())
        .bind(stage)
        .bind(status)
        .bind(ev.meta.occurred_at)
        .bind(ev.global_seq)
        .execute(&self.pool)
        .await?;
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
            TaskEvent::TaskCreated { title, source, .. } => {
                sqlx::query(
                    "INSERT INTO tasks_view \
                     (task_id, title, source, status, current_stage, created_at, updated_at, last_seq) \
                     VALUES ($1, $2, $3, 'open', NULL, $4, $4, $5) ON CONFLICT (task_id) DO NOTHING",
                )
                .bind(id)
                .bind(title)
                .bind(source.as_str())
                .bind(ev.meta.occurred_at)
                .bind(ev.global_seq)
                .execute(&self.pool)
                .await?;
            }
            TaskEvent::StageEntered { stage } => self.touch(id, ev, Some(&stage), Some("in_progress")).await?,
            TaskEvent::StageExited { .. } => self.touch(id, ev, None, None).await?,
            TaskEvent::TaskCancelled { .. } => self.touch(id, ev, None, Some("cancelled")).await?,
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
