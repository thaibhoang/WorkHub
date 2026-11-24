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
