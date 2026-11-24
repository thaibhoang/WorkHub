//! Executes one agent session: creates a git worktree sandbox, drives an
//! [`AgentRuntime`] (Claude Code headless first), and records every step as
//! `SessionEvent`s on the session stream.

pub mod claude_code;
mod runner;
mod runtime;
pub mod worktree;

pub use runner::{RunReport, RunSpec, Runner, RunnerConfig, SessionReport};
pub use runtime::{AgentRequest, AgentRuntime, Outcome, OutcomeStatus, Signal};
