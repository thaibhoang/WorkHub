//! Read models (projections) built from the event log. Every projection can be
//! dropped and rebuilt from events at any time.

pub mod benchmarks;
pub mod proposals;
pub mod queries;
pub mod scores;
pub mod sessions;
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

/// Catch every projection up with the log. Returns events applied per projection.
pub async fn catch_up_all(store: &EventStore) -> anyhow::Result<Vec<(&'static str, u64)>> {
    let sub = Subscription::new(store.clone());
    let pool = store.pool().clone();
    Ok(vec![
        (tasks::TasksProjection::NAME, sub.catch_up(&mut tasks::TasksProjection::new(pool.clone())).await?),
        (sessions::SessionFactsProjection::NAME, sub.catch_up(&mut sessions::SessionFactsProjection::new(pool.clone())).await?),
        (scores::ScoreResultsProjection::NAME, sub.catch_up(&mut scores::ScoreResultsProjection::new(pool.clone())).await?),
        (proposals::ProposalsProjection::NAME, sub.catch_up(&mut proposals::ProposalsProjection::new(pool.clone())).await?),
        (benchmarks::BenchmarksProjection::NAME, sub.catch_up(&mut benchmarks::BenchmarksProjection::new(pool)).await?),
    ])
}

/// Rebuild every projection from scratch.
pub async fn rebuild_all(store: &EventStore) -> anyhow::Result<Vec<(&'static str, u64)>> {
    let pool = store.pool().clone();
    Ok(vec![
        (tasks::TasksProjection::NAME, rebuild(store, &mut tasks::TasksProjection::new(pool.clone())).await?),
        (sessions::SessionFactsProjection::NAME, rebuild(store, &mut sessions::SessionFactsProjection::new(pool.clone())).await?),
        (scores::ScoreResultsProjection::NAME, rebuild(store, &mut scores::ScoreResultsProjection::new(pool.clone())).await?),
        (proposals::ProposalsProjection::NAME, rebuild(store, &mut proposals::ProposalsProjection::new(pool.clone())).await?),
        (benchmarks::BenchmarksProjection::NAME, rebuild(store, &mut benchmarks::BenchmarksProjection::new(pool)).await?),
    ])
}
