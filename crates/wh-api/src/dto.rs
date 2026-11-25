//! JSON shapes returned by the API that are not plain read-model rows.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;
use wh_core::{
    EventMeta, RecordedEvent,
    task::{TaskState, TaskStatus},
};
use wh_factory::Factory;
use wh_metrics::{
    proposals::{ProposalRow, VersionStats},
    queries::{SessionDetail, TaskSummary},
    scores::ScoreRow,
};

/// One event of the log, as sent over REST and SSE.
#[derive(Debug, Clone, Serialize)]
pub struct EventDto {
    pub seq: i64,
    pub event_id: Uuid,
    pub stream_id: String,
    pub stream_version: i64,
    #[serde(rename = "type")]
    pub event_type: String,
    pub payload: Value,
    pub meta: EventMeta,
    pub recorded_at: DateTime<Utc>,
}

impl From<&RecordedEvent> for EventDto {
    fn from(e: &RecordedEvent) -> Self {
        Self {
            seq: e.global_seq,
            event_id: e.event_id,
            stream_id: e.stream_id.clone(),
            stream_version: e.stream_version,
            event_type: e.event_type.clone(),
            payload: e.payload.clone(),
            meta: e.meta.clone(),
            recorded_at: e.recorded_at,
        }
    }
}

/// The task aggregate (folded from its stream) — what actions are possible now.
#[derive(Debug, Clone, Serialize)]
pub struct TaskStateDto {
    pub version: i64,
    pub status: &'static str,
    pub stage: Option<String>,
    pub gate: Option<String>,
    pub blocked_reason: Option<String>,
    pub blocked_stage: Option<String>,
    pub body: Option<String>,
    pub rework_loops: u32,
    pub results: Vec<StageResultDto>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StageResultDto {
    pub stage: String,
    pub outcome: &'static str,
    pub summary: Option<String>,
}

impl From<&TaskState> for TaskStateDto {
    fn from(s: &TaskState) -> Self {
        let (stage, gate, blocked_reason) = match &s.status {
            TaskStatus::Running { stage } => (Some(stage.clone()), None, None),
            TaskStatus::AwaitingGate { gate, stage } => (Some(stage.clone()), Some(gate.clone()), None),
            TaskStatus::Blocked { reason } => (s.blocked_stage.clone(), None, Some(reason.clone())),
            _ => (None, None, None),
        };
        Self {
            version: s.version,
            status: crate::commands::status_name(&s.status),
            stage,
            gate,
            blocked_reason,
            blocked_stage: s.blocked_stage.clone(),
            body: s.body.clone(),
            rework_loops: s.rework_loops,
            results: s
                .results
                .iter()
                .map(|r| StageResultDto { stage: r.stage.clone(), outcome: r.outcome.as_str(), summary: r.summary.clone() })
                .collect(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct TaskDetail {
    /// `None` while the projection has not caught up with a brand-new task.
    pub task: Option<TaskSummary>,
    pub state: TaskStateDto,
    pub events: Vec<EventDto>,
    pub sessions: Vec<SessionDetail>,
    pub scores: Vec<ScoreRow>,
}

#[derive(Debug, Serialize)]
pub struct SessionDetailResponse {
    pub session: SessionDetail,
    pub events: Vec<EventDto>,
    pub scores: Vec<ScoreRow>,
}

#[derive(Debug, Serialize)]
pub struct ProposalDetail {
    /// `None` while the projection has not caught up.
    pub proposal: Option<ProposalRow>,
    pub events: Vec<EventDto>,
    /// Sessions the improver cited.
    pub evidence: Vec<SessionDetail>,
    /// Metrics of the version the proposal was based on, and of the version it rolled out as.
    pub before: Option<VersionStats>,
    pub after: Option<VersionStats>,
}

/// `improvement:` settings from factory.yaml.
#[derive(Debug, Clone, Serialize)]
pub struct ImprovementDto {
    pub model: String,
    pub budget_usd: f64,
    pub window_days: u32,
    pub min_sessions: u64,
    pub interval_hours: Option<u64>,
    pub max_open_proposals: u32,
    pub triggers: Vec<TriggerDto>,
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TriggerDto {
    pub metric: &'static str,
    pub subject: String,
    pub above: f64,
}

impl From<&wh_factory::Improvement> for ImprovementDto {
    fn from(i: &wh_factory::Improvement) -> Self {
        Self {
            model: i.model.clone(),
            budget_usd: i.budget_usd,
            window_days: i.window_days,
            min_sessions: i.min_sessions,
            interval_hours: i.interval_hours,
            max_open_proposals: i.max_open_proposals,
            triggers: i
                .triggers
                .iter()
                .map(|t| TriggerDto { metric: t.metric.as_str(), subject: t.subject.clone(), above: t.above })
                .collect(),
            prompt: i.prompt.clone(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ImproveBody {
    pub focus: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct FactoryDto {
    pub name: String,
    pub version: String,
    pub repo: String,
    pub base_ref: String,
    pub max_rework_loops: u32,
    pub stages: Vec<StageDto>,
    pub scorers: Vec<ScorerDto>,
    pub improvement: Option<ImprovementDto>,
}

/// A scorer definition from factory.yaml.
#[derive(Debug, Clone, Serialize)]
pub struct ScorerDto {
    pub id: String,
    pub target: String,
    pub description: Option<String>,
    pub model: String,
    pub pass_threshold: f64,
    pub sampling_rate: f64,
    pub budget_usd: f64,
    pub include_failed: bool,
    pub prompt: String,
}

impl From<&wh_factory::Scorer> for ScorerDto {
    fn from(s: &wh_factory::Scorer) -> Self {
        Self {
            id: s.id.clone(),
            target: s.target.clone(),
            description: s.description.clone(),
            model: s.model.clone(),
            pass_threshold: s.pass_threshold,
            sampling_rate: s.sampling_rate,
            budget_usd: s.budget_usd,
            include_failed: s.include_failed,
            prompt: s.prompt.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct StageDto {
    pub id: String,
    pub gate: Option<String>,
    pub on_reject: Option<String>,
    pub agent: AgentDto,
}

#[derive(Debug, Serialize)]
pub struct AgentDto {
    pub name: String,
    pub description: Option<String>,
    pub model: Option<String>,
    pub budget_usd: f64,
    pub timeout_min: u64,
    pub max_retries: u32,
    pub permission_mode: String,
    pub allowed_tools: Vec<String>,
    pub prompt: String,
}

impl From<&Factory> for FactoryDto {
    fn from(f: &Factory) -> Self {
        Self {
            name: f.name.clone(),
            version: f.version.clone(),
            repo: f.repo.display().to_string(),
            base_ref: f.base_ref.clone(),
            max_rework_loops: f.max_rework_loops,
            stages: f
                .stages
                .iter()
                .map(|st| {
                    let a = f.agent(st);
                    StageDto {
                        id: st.id.clone(),
                        gate: st.gate.clone(),
                        on_reject: st.on_reject.clone(),
                        agent: AgentDto {
                            name: a.name.clone(),
                            description: a.description.clone(),
                            model: a.model.clone(),
                            budget_usd: a.budget_usd,
                            timeout_min: a.timeout.as_secs() / 60,
                            max_retries: a.max_retries,
                            permission_mode: a.permission_mode.clone(),
                            allowed_tools: a.allowed_tools.clone(),
                            prompt: a.prompt.clone(),
                        },
                    }
                })
                .collect(),
            scorers: f.scorers.iter().map(ScorerDto::from).collect(),
            improvement: f.improvement.as_ref().map(ImprovementDto::from),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateTaskBody {
    pub title: String,
    pub body: Option<String>,
    /// Hand the task to the server's factory pipeline (default true when a factory is loaded).
    pub pipeline: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct GateBody {
    pub approved: bool,
    pub comment: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ResumeBody {
    pub stage: Option<String>,
    pub comment: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CancelBody {
    #[serde(default)]
    pub reason: String,
}
