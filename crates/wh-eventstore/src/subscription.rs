use std::{future::Future, time::Duration};

use anyhow::Context;
use sqlx::postgres::PgListener;
use wh_core::RecordedEvent;

use crate::store::{EventStore, NOTIFY_CHANNEL};

/// A consumer of the global event log. Delivery is at-least-once: after a crash,
/// events since the last saved checkpoint are delivered again, so `handle` must
/// be idempotent.
pub trait EventHandler: Send {
    /// Stable consumer name; used as the checkpoint key.
    fn name(&self) -> &str;
    fn handle(&mut self, event: &RecordedEvent) -> impl Future<Output = anyhow::Result<()>> + Send;
}

pub struct Subscription {
    store: EventStore,
    batch_size: i64,
    idle_poll: Duration,
}

impl Subscription {
    pub fn new(store: EventStore) -> Self {
        Self {
            store,
            batch_size: 500,
            idle_poll: Duration::from_secs(5),
        }
    }

    /// Deliver every event after the handler's checkpoint; returns how many were handled.
    pub async fn catch_up<H: EventHandler>(&self, handler: &mut H) -> anyhow::Result<u64> {
        let mut handled = 0;
        loop {
            let from = self.store.load_checkpoint(handler.name()).await?;
            let batch = self.store.read_all(from, self.batch_size).await?;
            let Some(last) = batch.last().map(|e| e.global_seq) else {
                return Ok(handled);
            };
            for event in &batch {
                handler.handle(event).await.with_context(|| {
                    format!(
                        "{} failed on event #{} ({})",
                        handler.name(),
                        event.global_seq,
                        event.event_type
                    )
                })?;
                handled += 1;
            }
            self.store.save_checkpoint(handler.name(), last).await?;
        }
    }

    /// Catch up, then keep following new events (woken by NOTIFY, with a polling
    /// fallback) until `shutdown` resolves.
    pub async fn run<H: EventHandler>(
        &self,
        handler: &mut H,
        shutdown: impl Future<Output = ()>,
    ) -> anyhow::Result<()> {
        let mut listener = PgListener::connect_with(self.store.pool()).await?;
        listener.listen(NOTIFY_CHANNEL).await?;
        tokio::pin!(shutdown);
        loop {
            let n = self.catch_up(handler).await?;
            if n > 0 {
                tracing::debug!(consumer = handler.name(), handled = n, "caught up");
            }
            tokio::select! {
                _ = &mut shutdown => return Ok(()),
                r = listener.recv() => { r?; }
                _ = tokio::time::sleep(self.idle_poll) => {}
            }
        }
    }
}
