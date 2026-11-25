//! Executes one agent session: creates a git worktree sandbox (optionally running the agent in a Docker
//! container on top of it), drives an
//! [`AgentRuntime`] (Claude Code headless first), and records every step as
//! `SessionEvent`s on the session stream.

pub mod claude_code;
pub mod docker;
mod runner;
mod runtime;
pub mod worktree;

pub use runner::{RunReport, RunSpec, Runner, RunnerConfig, SessionReport};
pub use runtime::{AgentRequest, AgentRuntime, Outcome, OutcomeStatus, Signal};
