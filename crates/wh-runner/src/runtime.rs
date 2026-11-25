use std::{future::Future, path::PathBuf, time::Duration};

use tokio::sync::mpsc::UnboundedSender;
use wh_core::{FailureClass, session::TokenUsage};

use crate::docker::Container;

/// Everything a runtime needs to execute one agent session.
#[derive(Debug, Clone)]
pub struct AgentRequest {
    pub prompt: String,
    pub workdir: PathBuf,
    pub transcript_path: PathBuf,
    pub model: Option<String>,
    pub budget_usd: Option<f64>,
    pub permission_mode: Option<String>,
    pub allowed_tools: Vec<String>,
    /// Load the user's MCP servers too (off by default: they cost context tokens).
    pub with_user_mcp: bool,
    pub timeout: Duration,
    /// Restrict the built-in tool set (`Some(vec![])` = no tools, e.g. for a judge).
    /// `None` keeps the runtime's default tools.
    pub tools: Option<Vec<String>>,
    /// Run the agent inside this container instead of directly on the host.
    pub container: Option<Container>,
}

/// Live progress reported while the session runs.
#[derive(Debug, Clone, PartialEq)]
pub enum Signal {
    Ready {
        runtime_session_id: Option<String>,
        model: Option<String>,
    },
    ToolCalled {
        tool: String,
        ok: bool,
        duration_ms: u64,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum OutcomeStatus {
    Succeeded,
    Failed {
        class: FailureClass,
        message: String,
        retryable: bool,
    },
}

/// Final result of a session. Runtimes never return `Err`: every failure is
/// classified into an `Outcome` so it shows up in metrics.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub status: OutcomeStatus,
    pub usage: TokenUsage,
    pub cost_usd: f64,
    pub num_turns: u32,
    pub duration_ms: u64,
    pub summary: Option<String>,
}

impl Outcome {
    pub fn failed(class: FailureClass, message: impl Into<String>, retryable: bool, duration: Duration) -> Self {
        Self {
            status: OutcomeStatus::Failed { class, message: message.into(), retryable },
            usage: TokenUsage::default(),
            cost_usd: 0.0,
            num_turns: 0,
            duration_ms: duration.as_millis() as u64,
            summary: None,
        }
    }
}

pub trait AgentRuntime: Send + Sync {
    /// Short identifier recorded on `AgentDispatched` (e.g. "claude-code").
    fn name(&self) -> &str;
    fn run(&self, req: &AgentRequest, signals: UnboundedSender<Signal>) -> impl Future<Output = Outcome> + Send;
}
