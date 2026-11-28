use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    event::DomainEvent,
    ids::{AutopilotId, TaskId},
    session::TokenUsage,
};

/// What the autopilot was asked to decide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutopilotKind {
    /// Approve or reject a gate (`GateRequested`).
    Gate,
    /// Answer an agent's questions (`AgentAsked`).
    Questions,
}

impl AutopilotKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Gate => "gate",
            Self::Questions => "questions",
        }
    }
}

/// Events on an `autopilot-<id>` stream: the autopilot agent deciding one gate or
/// answering one question set of a task in autopilot mode, in place of a human.
/// `AutopilotStarted` is appended with `ExpectedVersion::NoStream` on an id derived
/// from (task, version of the request event), so a redelivered event never runs
/// (and pays for) the decision twice. The decision itself lands on the task stream
/// (`GateDecided` / `UserAnswered` by [`crate::task::AUTOPILOT_REVIEWER`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AutopilotEvent {
    AutopilotStarted {
        task_id: TaskId,
        kind: AutopilotKind,
        stage: String,
        /// Gate name (`kind: gate`).
        #[serde(default)]
        gate: Option<String>,
        /// Stream version of the `GateRequested` / `AgentAsked` being handled.
        request_version: i64,
        model: Option<String>,
    },
    /// The decision was recorded on the task stream.
    AutopilotDecided {
        /// `approve`, `reject` or `answered`.
        decision: String,
        /// The comment for the agent (gate) or the answers, as recorded.
        detail: String,
        rationale: String,
        cost_usd: f64,
        usage: TokenUsage,
        duration_ms: u64,
        transcript: Option<String>,
    },
    /// No decision (runtime failure, unusable answer, or the task moved on): the
    /// gate or questions stay for a human. The cost still counts.
    AutopilotFailed {
        message: String,
        cost_usd: f64,
        duration_ms: u64,
        transcript: Option<String>,
    },
}

impl DomainEvent for AutopilotEvent {
    fn event_type(&self) -> &'static str {
        match self {
            Self::AutopilotStarted { .. } => "AutopilotStarted",
            Self::AutopilotDecided { .. } => "AutopilotDecided",
            Self::AutopilotFailed { .. } => "AutopilotFailed",
        }
    }
}

impl AutopilotId {
    /// Stable id for the autopilot's handling of the request event at `version`
    /// of `task`'s stream.
    pub fn for_request(task: TaskId, version: i64) -> Self {
        Self(Uuid::new_v5(&task.0, format!("autopilot:{version}").as_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_ids() {
        let t = TaskId::new();
        assert_eq!(AutopilotId::for_request(t, 3), AutopilotId::for_request(t, 3));
        assert_ne!(AutopilotId::for_request(t, 3), AutopilotId::for_request(t, 4));
        assert_ne!(AutopilotId::for_request(t, 3), AutopilotId::for_request(TaskId::new(), 3));
    }

    #[test]
    fn payload_tag_matches_event_type() {
        let e = AutopilotEvent::AutopilotFailed { message: "x".into(), cost_usd: 0.0, duration_ms: 1, transcript: None };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["type"], e.event_type());
    }
}
