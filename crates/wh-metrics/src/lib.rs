//! Read models (projections) built from the event log. Every projection can be
//! dropped and rebuilt from events at any time.

pub mod tasks;

use wh_eventstore::{EventHandler, EventStore, Subscription};

/// A projection that can wipe its own tables for a full rebuild.
pub trait Projection: EventHandler {
    fn reset(&self) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;
}

/// Truncate a projection, reset its checkpoint and replay the whole log.
pub async fn rebuild<P: Projection>(store: &EventStore, projection: &mut P) -> anyhow::Result<u64> {
    projection.reset().await?;
    store.reset_checkpoint(projection.name()).await?;
    Subscription::new(store.clone()).catch_up(projection).await
}
