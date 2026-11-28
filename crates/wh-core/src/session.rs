use serde::{Deserialize, Serialize};

use crate::{
    event::DomainEvent,
    failure::FailureClass,
    ids::{BenchmarkRunId, SessionId, TaskId},
    sandbox::Sandbox,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

/// Events on a `session-<id>` stream: one agent execution. This stream is the
/// raw material for every cost / duration / failure metric.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SessionEvent {
    AgentDispatched {
        task_id: TaskId,
        agent: String,
        stage: String,
        runtime: String,
        model: Option<String>,
        prompt: String,
        budget_usd: Option<f64>,
        timeout_secs: u64,
        /// 1 for the first try; retries count up. Defaults keep pre-retry events readable.
        #[serde(default = "first_attempt")]
        attempt: u32,
        /// The failed session this one retries.
        #[serde(default)]
        retry_of: Option<SessionId>,
        /// The session this one continues after the user answered its questions.
        #[serde(default)]
        resume_of: Option<SessionId>,
        /// Benchmark run this session belongs to (excluded from production metrics).
        #[serde(default)]
        benchmark: Option<BenchmarkRunId>,
    },
    AgentSessionStarted {
        workdir: String,
        branch: String,
        base_ref: String,
        /// Where the agent ran. Absent on events from before sandboxes (= worktree).
        #[serde(default)]
        sandbox: Option<Sandbox>,
    },
    /// The runtime reported its own session id and the model actually used.
    AgentRuntimeReady {
        runtime_session_id: Option<String>,
        model: Option<String>,
    },
    AgentToolCalled {
        tool: String,
        ok: bool,
        duration_ms: u64,
    },
    /// Cumulative totals for the session (last report wins).
    AgentUsageReported {
        usage: TokenUsage,
        cost_usd: f64,
    },
    AgentSessionCompleted {
        num_turns: u32,
        duration_ms: u64,
        summary: Option<String>,
        transcript: Option<String>,
        /// Commit WorkHub made on the task branch with the agent's changes.
        #[serde(default)]
        commit: Option<String>,
    },
    AgentSessionFailed {
        failure_class: FailureClass,
        message: String,
        retryable: bool,
        num_turns: u32,
        duration_ms: u64,
        transcript: Option<String>,
    },
}

fn first_attempt() -> u32 {
    1
}

impl DomainEvent for SessionEvent {
    fn event_type(&self) -> &'static str {
        match self {
            Self::AgentDispatched { .. } => "AgentDispatched",
            Self::AgentSessionStarted { .. } => "AgentSessionStarted",
            Self::AgentRuntimeReady { .. } => "AgentRuntimeReady",
            Self::AgentToolCalled { .. } => "AgentToolCalled",
            Self::AgentUsageReported { .. } => "AgentUsageReported",
            Self::AgentSessionCompleted { .. } => "AgentSessionCompleted",
            Self::AgentSessionFailed { .. } => "AgentSessionFailed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_of_is_absent_on_old_events() {
        let v = serde_json::json!({
            "type": "AgentDispatched", "task_id": TaskId::new(), "agent": "a", "stage": "s",
            "runtime": "r", "model": null, "prompt": "p", "budget_usd": null, "timeout_secs": 1,
        });
        assert!(matches!(
            serde_json::from_value::<SessionEvent>(v).unwrap(),
            SessionEvent::AgentDispatched { resume_of: None, .. }
        ));
    }
}
