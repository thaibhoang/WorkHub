use serde::{Deserialize, Serialize};

use crate::{event::DomainEvent, ids::SessionId};

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
/// defined per factory in factory-as-code, not hard-coded. Fields added after
/// the first release carry `#[serde(default)]` so older events still decode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum TaskEvent {
    TaskCreated {
        title: String,
        body: Option<String>,
        source: TaskSource,
        /// Name of the factory that should drive this task (None = manual only).
        #[serde(default)]
        factory: Option<String>,
    },
    StageEntered {
        stage: String,
        /// Present when the stage is re-run to address review / gate feedback.
        #[serde(default)]
        feedback: Option<String>,
    },
    StageExited {
        stage: String,
        outcome: StageOutcome,
        #[serde(default)]
        summary: Option<String>,
        #[serde(default)]
        session: Option<SessionId>,
    },
    GateRequested {
        gate: String,
        stage: String,
    },
    GateDecided {
        gate: String,
        stage: String,
        approved: bool,
        reviewer: String,
        comment: Option<String>,
    },
    /// Needs a human: stage failed after retries, rework limit hit, bad verdict...
    TaskBlocked {
        stage: Option<String>,
        reason: String,
    },
    TaskResumed {
        stage: Option<String>,
        comment: Option<String>,
    },
    TaskCompleted {
        branch: Option<String>,
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

impl StageOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Rejected => "rejected",
            Self::Skipped => "skipped",
        }
    }
}

impl DomainEvent for TaskEvent {
    fn event_type(&self) -> &'static str {
        match self {
            Self::TaskCreated { .. } => "TaskCreated",
            Self::StageEntered { .. } => "StageEntered",
            Self::StageExited { .. } => "StageExited",
            Self::GateRequested { .. } => "GateRequested",
            Self::GateDecided { .. } => "GateDecided",
            Self::TaskBlocked { .. } => "TaskBlocked",
            Self::TaskResumed { .. } => "TaskResumed",
            Self::TaskCompleted { .. } => "TaskCompleted",
            Self::TaskCancelled { .. } => "TaskCancelled",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TaskStatus {
    #[default]
    Open,
    Running { stage: String },
    AwaitingGate { gate: String, stage: String },
    Blocked { reason: String },
    Done,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StageResult {
    pub stage: String,
    pub outcome: StageOutcome,
    pub summary: Option<String>,
}

/// Task aggregate state, folded from its event stream.
#[derive(Debug, Clone, Default)]
pub struct TaskState {
    /// Stream version of the last applied event (for optimistic concurrency).
    pub version: i64,
    pub title: String,
    pub body: Option<String>,
    pub factory: Option<String>,
    pub status: TaskStatus,
    pub results: Vec<StageResult>,
    /// Stages re-run because of feedback (review reject, gate reject, resume comment).
    pub rework_loops: u32,
    /// Stage named by the latest `TaskBlocked` (where `resume` restarts by default).
    pub blocked_stage: Option<String>,
    pub last: Option<TaskEvent>,
}

impl TaskState {
    pub fn fold(events: impl IntoIterator<Item = (i64, TaskEvent)>) -> Self {
        let mut s = Self::default();
        for (version, ev) in events {
            s.apply(version, ev);
        }
        s
    }

    pub fn apply(&mut self, version: i64, ev: TaskEvent) {
        self.version = version;
        match &ev {
            TaskEvent::TaskCreated { title, body, factory, .. } => {
                self.title = title.clone();
                self.body = body.clone();
                self.factory = factory.clone();
                self.status = TaskStatus::Open;
            }
            TaskEvent::StageEntered { stage, feedback } => {
                if feedback.is_some() {
                    self.rework_loops += 1;
                }
                self.status = TaskStatus::Running { stage: stage.clone() };
            }
            TaskEvent::StageExited { stage, outcome, summary, .. } => self.results.push(StageResult {
                stage: stage.clone(),
                outcome: *outcome,
                summary: summary.clone(),
            }),
            TaskEvent::GateRequested { gate, stage } => {
                self.status = TaskStatus::AwaitingGate { gate: gate.clone(), stage: stage.clone() }
            }
            TaskEvent::GateDecided { .. } => {}
            TaskEvent::TaskBlocked { stage, reason } => {
                self.blocked_stage = stage.clone();
                self.status = TaskStatus::Blocked { reason: reason.clone() }
            }
            TaskEvent::TaskResumed { .. } => self.status = TaskStatus::Open,
            TaskEvent::TaskCompleted { .. } => self.status = TaskStatus::Done,
            TaskEvent::TaskCancelled { .. } => self.status = TaskStatus::Cancelled,
        }
        self.last = Some(ev);
    }

    /// Most recent stage that finished, if any.
    pub fn last_stage(&self) -> Option<&str> {
        self.results.last().map(|r| r.stage.as_str())
    }

    /// Stage currently (or most recently) running.
    pub fn last_stage_entered(&self) -> Option<String> {
        match &self.status {
            TaskStatus::Running { stage } => Some(stage.clone()),
            _ => self.last_stage().map(str::to_owned),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Approve,
    Reject,
}

/// Find the last `VERDICT: APPROVE|REJECT` line in an agent's final message
/// (tolerates surrounding markdown like `**` or backticks).
pub fn parse_verdict(text: &str) -> Option<Verdict> {
    text.lines().rev().find_map(|line| {
        let l = line.trim().trim_matches(|c| c == '*' || c == '`' || c == '#').trim().to_ascii_uppercase();
        match l.strip_prefix("VERDICT:")?.trim().trim_matches(|c| c == '*' || c == '`').trim() {
            "APPROVE" => Some(Verdict::Approve),
            "REJECT" => Some(Verdict::Reject),
            _ => None,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_tag_matches_event_type() {
        let e = TaskEvent::StageEntered { stage: "spec".into(), feedback: None };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["type"], e.event_type());
        assert_eq!(serde_json::from_value::<TaskEvent>(v).unwrap(), e);
    }

    #[test]
    fn old_events_without_new_fields_decode() {
        let v = serde_json::json!({"type": "StageExited", "stage": "x", "outcome": "succeeded"});
        assert!(matches!(
            serde_json::from_value::<TaskEvent>(v).unwrap(),
            TaskEvent::StageExited { summary: None, session: None, .. }
        ));
    }

    #[test]
    fn fold_tracks_status_and_rework() {
        let s = TaskState::fold([
            (1, TaskEvent::TaskCreated { title: "t".into(), body: None, source: TaskSource::Cli, factory: Some("f".into()) }),
            (2, TaskEvent::StageEntered { stage: "impl".into(), feedback: None }),
            (3, TaskEvent::StageExited { stage: "impl".into(), outcome: StageOutcome::Succeeded, summary: None, session: None }),
            (4, TaskEvent::StageEntered { stage: "impl".into(), feedback: Some("fix".into()) }),
        ]);
        assert_eq!((s.version, s.rework_loops), (4, 1));
        assert_eq!(s.status, TaskStatus::Running { stage: "impl".into() });
        assert_eq!(s.last_stage(), Some("impl"));
    }

    #[test]
    fn verdict_parsing() {
        assert_eq!(parse_verdict("looks good\nVERDICT: APPROVE"), Some(Verdict::Approve));
        assert_eq!(parse_verdict("**Verdict: reject**\n"), Some(Verdict::Reject));
        assert_eq!(parse_verdict("VERDICT: REJECT\nlater\n`VERDICT: APPROVE`"), Some(Verdict::Approve));
        assert_eq!(parse_verdict("no verdict here"), None);
        assert_eq!(parse_verdict("VERDICT: maybe"), None);
    }
}
