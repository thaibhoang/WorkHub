//! Live event feed for SSE clients.
//!
//! One background task LISTENs on `wh_events` and publishes the newest
//! `global_seq` on a `watch` channel. Each SSE client keeps its own cursor and
//! reads the log itself (`read_all(cursor)`), so a slow client never loses
//! events and a missed NOTIFY only delays delivery until the next wake-up.

use std::{convert::Infallible, time::Duration};

use axum::response::sse::Event;
use futures_util::Stream;
use sqlx::postgres::PgListener;
use tokio::sync::watch;
use uuid::Uuid;
use wh_eventstore::{EventStore, NOTIFY_CHANNEL};

use crate::dto::EventDto;

const BATCH: i64 = 200;
/// Re-check the log even without a NOTIFY (listener reconnects, missed notifications).
const IDLE_RECHECK: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct Live {
    head: watch::Receiver<i64>,
    /// Only set by [`Live::detached`], to keep its channel open.
    _tx: Option<std::sync::Arc<watch::Sender<i64>>>,
}

impl Live {
    /// Start the listener task. The feed ends when the process does.
    pub async fn start(store: EventStore) -> anyhow::Result<Self> {
        let (tx, rx) = watch::channel(store.head_seq().await?);
        tokio::spawn(async move {
            loop {
                if let Err(e) = listen(&store, &tx).await {
                    tracing::warn!("live feed listener: {e:#}; reconnecting");
                }
                if tx.is_closed() {
                    return;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        Ok(Self { head: rx, _tx: None })
    }

    /// A feed that never wakes up on its own (tests drive it via the idle re-check).
    pub fn detached(head: i64) -> Self {
        let (tx, rx) = watch::channel(head);
        Self { head: rx, _tx: Some(std::sync::Arc::new(tx)) }
    }

    pub fn head(&self) -> i64 {
        *self.head.borrow()
    }

    /// Events after `cursor`, optionally only those of one task (by correlation id).
    pub fn stream(
        &self,
        store: EventStore,
        cursor: i64,
        task: Option<Uuid>,
    ) -> impl Stream<Item = Result<Event, Infallible>> + Send + use<> {
        let state = Feed { store, rx: self.head.clone(), cursor, task, buf: Default::default() };
        futures_util::stream::unfold(state, |mut s| async move {
            loop {
                if let Some(ev) = s.buf.pop_front() {
                    return Some((Ok(ev), s));
                }
                match s.store.read_all(s.cursor, BATCH).await {
                    Ok(batch) if !batch.is_empty() => {
                        s.cursor = batch.last().map(|e| e.global_seq).unwrap_or(s.cursor);
                        for e in batch.iter().filter(|e| s.task.is_none_or(|t| e.meta.correlation_id == t)) {
                            let dto = EventDto::from(e);
                            match Event::default().id(dto.seq.to_string()).event("event").json_data(&dto) {
                                Ok(ev) => s.buf.push_back(ev),
                                Err(err) => tracing::warn!("encode event #{}: {err}", dto.seq),
                            }
                        }
                        continue;
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!("live feed read: {e}"),
                }
                // Caught up: wait for a newer head (or re-check after a while).
                tokio::select! {
                    r = s.rx.changed() => if r.is_err() { return None },
                    _ = tokio::time::sleep(IDLE_RECHECK) => {}
                }
            }
        })
    }
}

struct Feed {
    store: EventStore,
    rx: watch::Receiver<i64>,
    cursor: i64,
    task: Option<Uuid>,
    buf: std::collections::VecDeque<Event>,
}

async fn listen(store: &EventStore, tx: &watch::Sender<i64>) -> anyhow::Result<()> {
    let mut listener = PgListener::connect_with(store.pool()).await?;
    listener.listen(NOTIFY_CHANNEL).await?;
    // Events committed while we were (re)connecting.
    let head = store.head_seq().await?;
    tx.send_if_modified(|h| bump(h, head));
    loop {
        let n = listener.recv().await?;
        let seq = n.payload().parse().unwrap_or(0);
        tx.send_if_modified(|h| bump(h, seq));
        if tx.is_closed() {
            return Ok(());
        }
    }
}

fn bump(head: &mut i64, seq: i64) -> bool {
    if seq > *head {
        *head = seq;
        true
    } else {
        false
    }
}
