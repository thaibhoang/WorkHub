use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    event::DomainEvent,
    ids::{ScoreId, SessionId, TaskId},
    session::TokenUsage,
};

/// Events on a `score-<id>` stream: one scorer (LLM-as-judge) grading one agent
/// session. `ScoreStarted` is appended with `ExpectedVersion::NoStream` on a
/// deterministic stream id, so a redelivered session event never grades twice.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ScoreEvent {
    ScoreStarted {
        scorer: String,
        target_session: SessionId,
        task_id: TaskId,
        /// Agent / stage of the graded session (denormalised for read models).
        agent: String,
        stage: String,
        model: Option<String>,
        pass_threshold: f64,
        /// Requested by a human (`wh score session`) rather than the automatic consumer.
        #[serde(default)]
        manual: bool,
    },
    ScoreRecorded {
        /// 0.0 ..= 1.0
        score: f64,
        pass: bool,
        /// Short machine-friendly label from the judge (e.g. "redundant_tests").
        classification: Option<String>,
        rationale: String,
        cost_usd: f64,
        usage: TokenUsage,
        duration_ms: u64,
        transcript: Option<String>,
    },
    /// The judge could not produce a score (runtime failure or unparseable answer).
    /// Its cost still counts toward scorer cost.
    ScoreFailed {
        message: String,
        cost_usd: f64,
        duration_ms: u64,
        transcript: Option<String>,
    },
}

impl DomainEvent for ScoreEvent {
    fn event_type(&self) -> &'static str {
        match self {
            Self::ScoreStarted { .. } => "ScoreStarted",
            Self::ScoreRecorded { .. } => "ScoreRecorded",
            Self::ScoreFailed { .. } => "ScoreFailed",
        }
    }
}

impl ScoreId {
    /// Stable id for the automatic grading of `session` by `scorer`.
    pub fn for_session(session: SessionId, scorer: &str) -> Self {
        Self(Uuid::new_v5(&session.0, scorer.as_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_ids() {
        let s = SessionId::new();
        assert_eq!(ScoreId::for_session(s, "a"), ScoreId::for_session(s, "a"));
        assert_ne!(ScoreId::for_session(s, "a"), ScoreId::for_session(s, "b"));
        assert_ne!(ScoreId::for_session(s, "a"), ScoreId::for_session(SessionId::new(), "a"));
    }

    #[test]
    fn payload_tag_matches_event_type() {
        let e = ScoreEvent::ScoreFailed { message: "x".into(), cost_usd: 0.0, duration_ms: 1, transcript: None };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["type"], e.event_type());
    }
}
