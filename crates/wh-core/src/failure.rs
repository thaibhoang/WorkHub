use serde::{Deserialize, Serialize};

/// Fixed taxonomy for agent/session failures so failure rates are comparable
/// across agents, stages and factory versions. Extend deliberately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    Timeout,
    BudgetExceeded,
    ToolError,
    TestFailed,
    CiFailed,
    ReviewRejected,
    GateRejected,
    SpecAmbiguous,
    EnvSetup,
    AgentGaveUp,
    Infra,
    /// The account's usage/rate limit was hit (e.g. the 5-hour subscription
    /// window). Not the agent's fault and not worth retrying until it resets;
    /// excluded from failure rates so the improver does not "fix" it.
    UsageLimit,
    /// The saved conversation a resume needs is gone. Not retryable: the caller
    /// has to fall back to a fresh session.
    SessionLost,
    Unknown,
}

impl FailureClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::BudgetExceeded => "budget_exceeded",
            Self::ToolError => "tool_error",
            Self::TestFailed => "test_failed",
            Self::CiFailed => "ci_failed",
            Self::ReviewRejected => "review_rejected",
            Self::GateRejected => "gate_rejected",
            Self::SpecAmbiguous => "spec_ambiguous",
            Self::EnvSetup => "env_setup",
            Self::AgentGaveUp => "agent_gave_up",
            Self::Infra => "infra",
            Self::UsageLimit => "usage_limit",
            Self::SessionLost => "session_lost",
            Self::Unknown => "unknown",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_str_matches_serde() {
        for c in [FailureClass::BudgetExceeded, FailureClass::EnvSetup, FailureClass::AgentGaveUp, FailureClass::UsageLimit, FailureClass::SessionLost] {
            assert_eq!(serde_json::to_value(c).unwrap(), c.as_str());
        }
    }
}
