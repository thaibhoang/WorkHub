//! Domain types shared by every WorkHub component: IDs, the event envelope,
//! and the domain events themselves. No I/O lives here.

pub mod benchmark;
pub mod event;
pub mod failure;
pub mod ids;
pub mod proposal;
pub mod score;
pub mod session;
pub mod task;

pub use event::{Actor, DomainEvent, EventMeta, NewEvent, RecordedEvent};
pub use failure::FailureClass;
pub use ids::{BenchmarkRunId, FactoryId, ProposalId, RunId, ScoreId, SessionId, TaskId};
