use sqlx::{PgPool, Row, postgres::PgRow};
use wh_core::{NewEvent, RecordedEvent};

/// Postgres NOTIFY channel fired after every committed append (payload: last global_seq).
pub const NOTIFY_CHANNEL: &str = "wh_events";

/// Advisory lock taken by every append. Serialising appends guarantees that
/// `global_seq` order equals commit order, so a consumer reading
/// `global_seq > checkpoint` can never skip a late-committing event.
const APPEND_LOCK_KEY: i64 = 0x5748_4556; // "WHEV"

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpectedVersion {
    Any,
    NoStream,
    Exact(i64),
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("concurrency conflict on {stream_id}: expected {expected:?}, actual version {actual}")]
    Conflict {
        stream_id: String,
        expected: ExpectedVersion,
        actual: i64,
    },
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
}

#[derive(Clone)]
pub struct EventStore {
    pool: PgPool,
}

const SELECT_COLS: &str = "global_seq, event_id, stream_id, stream_version, event_type, \
                           schema_version, payload, meta, recorded_at";

impl EventStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Atomically append `events` to `stream_id` if its current version matches `expected`.
    pub async fn append(
        &self,
        stream_id: &str,
        expected: ExpectedVersion,
        events: Vec<NewEvent>,
    ) -> Result<Vec<RecordedEvent>, StoreError> {
        if events.is_empty() {
            return Ok(Vec::new());
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(APPEND_LOCK_KEY)
            .execute(&mut *tx)
            .await?;

        let current: i64 =
            sqlx::query_scalar("SELECT COALESCE(MAX(stream_version), 0) FROM events WHERE stream_id = $1")
                .bind(stream_id)
                .fetch_one(&mut *tx)
                .await?;
        let ok = match expected {
            ExpectedVersion::Any => true,
            ExpectedVersion::NoStream => current == 0,
            ExpectedVersion::Exact(v) => current == v,
        };
        if !ok {
            return Err(StoreError::Conflict {
                stream_id: stream_id.to_owned(),
                expected,
                actual: current,
            });
        }

        let mut recorded = Vec::with_capacity(events.len());
        for (i, ev) in events.into_iter().enumerate() {
            let version = current + 1 + i as i64;
            let row = sqlx::query(
                "INSERT INTO events \
                 (event_id, stream_id, stream_version, event_type, schema_version, payload, meta) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING global_seq, recorded_at",
            )
            .bind(ev.event_id)
            .bind(stream_id)
            .bind(version)
            .bind(&ev.event_type)
            .bind(ev.schema_version)
            .bind(&ev.payload)
            .bind(serde_json::to_value(&ev.meta)?)
            .fetch_one(&mut *tx)
            .await?;
            recorded.push(RecordedEvent {
                global_seq: row.try_get("global_seq")?,
                event_id: ev.event_id,
                stream_id: stream_id.to_owned(),
                stream_version: version,
                event_type: ev.event_type,
                schema_version: ev.schema_version,
                payload: ev.payload,
                meta: ev.meta,
                recorded_at: row.try_get("recorded_at")?,
            });
        }

        let last = recorded.last().map(|e| e.global_seq).unwrap_or_default();
        sqlx::query("SELECT pg_notify($1, $2)")
            .bind(NOTIFY_CHANNEL)
            .bind(last.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(recorded)
    }

    pub async fn read_stream(&self, stream_id: &str) -> Result<Vec<RecordedEvent>, StoreError> {
        let rows = sqlx::query(&format!(
            "SELECT {SELECT_COLS} FROM events WHERE stream_id = $1 ORDER BY stream_version"
        ))
        .bind(stream_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(from_row).collect()
    }

    /// Read the global log in order, starting strictly after `after_seq`.
    pub async fn read_all(&self, after_seq: i64, limit: i64) -> Result<Vec<RecordedEvent>, StoreError> {
        let rows = sqlx::query(&format!(
            "SELECT {SELECT_COLS} FROM events WHERE global_seq > $1 ORDER BY global_seq LIMIT $2"
        ))
        .bind(after_seq)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(from_row).collect()
    }

    /// `global_seq` of the newest committed event (0 for an empty log).
    pub async fn head_seq(&self) -> Result<i64, StoreError> {
        Ok(sqlx::query_scalar("SELECT COALESCE(MAX(global_seq), 0) FROM events").fetch_one(&self.pool).await?)
    }

    /// Latest (version, event_type) of every stream whose id starts with `prefix`.
    pub async fn stream_heads(&self, prefix: &str) -> Result<Vec<(String, i64, String)>, StoreError> {
        let rows: Vec<(String, i64, String)> = sqlx::query_as(
            "SELECT DISTINCT ON (stream_id) stream_id, stream_version, event_type FROM events \
             WHERE starts_with(stream_id, $1) ORDER BY stream_id, stream_version DESC",
        )
        .bind(prefix)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn load_checkpoint(&self, consumer: &str) -> Result<i64, StoreError> {
        let seq: Option<i64> =
            sqlx::query_scalar("SELECT last_seq FROM consumer_checkpoints WHERE consumer = $1")
                .bind(consumer)
                .fetch_optional(&self.pool)
                .await?;
        Ok(seq.unwrap_or(0))
    }

    pub async fn save_checkpoint(&self, consumer: &str, seq: i64) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO consumer_checkpoints (consumer, last_seq) VALUES ($1, $2) \
             ON CONFLICT (consumer) DO UPDATE SET last_seq = EXCLUDED.last_seq, updated_at = now()",
        )
        .bind(consumer)
        .bind(seq)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn reset_checkpoint(&self, consumer: &str) -> Result<(), StoreError> {
        self.save_checkpoint(consumer, 0).await
    }
}

fn from_row(row: &PgRow) -> Result<RecordedEvent, StoreError> {
    Ok(RecordedEvent {
        global_seq: row.try_get("global_seq")?,
        event_id: row.try_get("event_id")?,
        stream_id: row.try_get("stream_id")?,
        stream_version: row.try_get("stream_version")?,
        event_type: row.try_get("event_type")?,
        schema_version: row.try_get("schema_version")?,
        payload: row.try_get("payload")?,
        meta: serde_json::from_value(row.try_get("meta")?)?,
        recorded_at: row.try_get("recorded_at")?,
    })
}
