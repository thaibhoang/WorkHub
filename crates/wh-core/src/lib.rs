//! Domain types shared by every WorkHub component: IDs, the event envelope,
//! and the domain events themselves. No I/O lives here.

pub mod benchmark;
pub mod event;
pub mod failure;
pub mod ids;
pub mod proposal;
pub mod question;
pub mod sandbox;
pub mod score;
pub mod session;
pub mod task;

pub use event::{Actor, DomainEvent, EventMeta, NewEvent, RecordedEvent};
pub use failure::FailureClass;
pub use sandbox::{DockerSandbox, Sandbox};
pub use ids::{BenchmarkRunId, FactoryId, ProposalId, RunId, ScoreId, SessionId, TaskId};
pub use question::{Answer, Question, format_answers, parse_questions};

/// Environment variables with this prefix hold WorkHub's credentials for creating
/// agents' databases (`task_database.admin_url_env`). Agents never inherit them.
pub const AGENT_DB_ENV_PREFIX: &str = "WH_AGENT_DB_";
