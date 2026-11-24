//! Postgres-backed append-only event store plus checkpointed subscriptions.

mod store;
mod subscription;

pub use store::{EventStore, ExpectedVersion, NOTIFY_CHANNEL, StoreError};
pub use subscription::{EventHandler, Subscription};

/// All schema migrations (event log + read models), embedded at compile time.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");
