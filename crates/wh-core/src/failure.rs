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
            Self::Unknown => "unknown",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_str_matches_serde() {
        for c in [FailureClass::BudgetExceeded, FailureClass::EnvSetup, FailureClass::AgentGaveUp] {
            assert_eq!(serde_json::to_value(c).unwrap(), c.as_str());
        }
    }
}
