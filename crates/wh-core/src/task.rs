use serde::{Deserialize, Serialize};

use crate::event::DomainEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskSource {
    Cli,
    Web,
}

impl TaskSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Web => "web",
        }
    }
}

/// Events on a `task-<id>` stream. Stage ids are strings because stages are
/// defined per factory in factory-as-code, not hard-coded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum TaskEvent {
    TaskCreated {
        title: String,
        body: Option<String>,
        source: TaskSource,
    },
    StageEntered {
        stage: String,
    },
    StageExited {
        stage: String,
        outcome: StageOutcome,
    },
    TaskCancelled {
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageOutcome {
    Succeeded,
    Failed,
    Rejected,
    Skipped,
}

impl DomainEvent for TaskEvent {
    fn event_type(&self) -> &'static str {
        match self {
            Self::TaskCreated { .. } => "TaskCreated",
            Self::StageEntered { .. } => "StageEntered",
            Self::StageExited { .. } => "StageExited",
            Self::TaskCancelled { .. } => "TaskCancelled",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_tag_matches_event_type() {
        let e = TaskEvent::StageEntered { stage: "spec".into() };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["type"], e.event_type());
        assert_eq!(serde_json::from_value::<TaskEvent>(v).unwrap(), e);
    }
}
