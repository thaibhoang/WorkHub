use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    event::DomainEvent,
    ids::{ProposalId, SessionId},
    session::TokenUsage,
};

/// Events on a `proposal-<id>` stream: one self-improvement run and, if it
/// produced a change, the proposal's review and rollout.
///
/// `ImprovementRequested` is the only event a non-improver may write (a human,
/// or the improver's own schedule/trigger with a deterministic id appended with
/// `NoStream`). The improver claims a request by appending `ImprovementStarted`
/// at `Exact(1)`, so a request runs at most once.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
#[allow(clippy::large_enum_variant)] // decoded one at a time; boxing would only complicate the serde shape
pub enum ProposalEvent {
    ImprovementRequested {
        /// Factory name whose improver should run.
        factory: String,
        /// Why: "manual", "schedule", or a trigger description.
        trigger: String,
        /// Optional human hint about what to look at.
        #[serde(default)]
        focus: Option<String>,
    },
    ImprovementStarted {
        /// `factory_version` the observation (and the proposal's diff) is based on.
        base_factory_version: String,
        window_days: u32,
        /// Finished sessions in the observation window.
        sessions_observed: u64,
    },
    /// The improver changed the factory: a commit on `branch` in the factory repo.
    ProposalCreated {
        title: String,
        rationale: String,
        /// Quantified expectation, e.g. "implement fail rate 30% → <10%".
        expected_impact: Option<String>,
        /// Sessions cited as evidence.
        evidence: Vec<SessionId>,
        /// Changed paths, relative to the factory repo root.
        files: Vec<String>,
        diff: String,
        branch: String,
        commit: String,
        /// Version of the factory after the change (what rollout should produce).
        proposed_factory_version: String,
        /// Git repo holding the factory and the factory file within it (for apply).
        factory_repo: String,
        factory_file: String,
        cost_usd: f64,
        usage: TokenUsage,
        duration_ms: u64,
        transcript: Option<String>,
    },
    /// The improver ran but decided no change is warranted.
    ImprovementSkipped {
        reason: String,
        cost_usd: f64,
        duration_ms: u64,
        transcript: Option<String>,
    },
    ImprovementFailed {
        message: String,
        cost_usd: f64,
        duration_ms: u64,
        transcript: Option<String>,
    },
    ProposalDecided {
        approved: bool,
        reviewer: String,
        comment: Option<String>,
    },
    /// The change is live: the factory now loads with `factory_version`.
    ProposalRolledOut {
        factory_version: String,
        /// Commit on the factory repo's branch (None when detected, not applied by WorkHub).
        commit: Option<String>,
    },
}

impl DomainEvent for ProposalEvent {
    fn event_type(&self) -> &'static str {
        match self {
            Self::ImprovementRequested { .. } => "ImprovementRequested",
            Self::ImprovementStarted { .. } => "ImprovementStarted",
            Self::ProposalCreated { .. } => "ProposalCreated",
            Self::ImprovementSkipped { .. } => "ImprovementSkipped",
            Self::ImprovementFailed { .. } => "ImprovementFailed",
            Self::ProposalDecided { .. } => "ProposalDecided",
            Self::ProposalRolledOut { .. } => "ProposalRolledOut",
        }
    }
}

/// Where a proposal stream is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProposalStatus {
    #[default]
    Requested,
    Running,
    /// Awaiting a human decision.
    Open,
    NoProposal,
    Failed,
    Approved,
    Rejected,
    RolledOut,
}

impl ProposalStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Running => "running",
            Self::Open => "open",
            Self::NoProposal => "no_proposal",
            Self::Failed => "failed",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::RolledOut => "rolled_out",
        }
    }
}

/// Proposal aggregate, folded from its stream.
#[derive(Debug, Clone, Default)]
pub struct ProposalState {
    pub version: i64,
    pub factory: String,
    pub status: ProposalStatus,
    pub base_factory_version: Option<String>,
    pub proposed_factory_version: Option<String>,
    pub commit: Option<String>,
    pub branch: Option<String>,
    pub factory_repo: Option<String>,
    pub factory_file: Option<String>,
    pub title: Option<String>,
    pub files: Vec<String>,
    /// Event id of the first event (cause of later ones).
    pub first_event: Option<Uuid>,
}

impl ProposalState {
    pub fn fold(events: impl IntoIterator<Item = (i64, Uuid, ProposalEvent)>) -> Self {
        let mut s = Self::default();
        for (version, id, ev) in events {
            s.apply(version, id, ev);
        }
        s
    }

    pub fn apply(&mut self, version: i64, event_id: Uuid, ev: ProposalEvent) {
        self.version = version;
        self.first_event.get_or_insert(event_id);
        match ev {
            ProposalEvent::ImprovementRequested { factory, .. } => {
                self.factory = factory;
                self.status = ProposalStatus::Requested;
            }
            ProposalEvent::ImprovementStarted { base_factory_version, .. } => {
                self.base_factory_version = Some(base_factory_version);
                self.status = ProposalStatus::Running;
            }
            ProposalEvent::ProposalCreated {
                title, files, branch, commit, proposed_factory_version, factory_repo, factory_file, ..
            } => {
                self.title = Some(title);
                self.files = files;
                self.branch = Some(branch);
                self.commit = Some(commit);
                self.proposed_factory_version = Some(proposed_factory_version);
                self.factory_repo = Some(factory_repo);
                self.factory_file = Some(factory_file);
                self.status = ProposalStatus::Open;
            }
            ProposalEvent::ImprovementSkipped { .. } => self.status = ProposalStatus::NoProposal,
            ProposalEvent::ImprovementFailed { .. } => self.status = ProposalStatus::Failed,
            ProposalEvent::ProposalDecided { approved, .. } => {
                self.status = if approved { ProposalStatus::Approved } else { ProposalStatus::Rejected }
            }
            ProposalEvent::ProposalRolledOut { .. } => self.status = ProposalStatus::RolledOut,
        }
    }
}

impl ProposalId {
    /// Stable id for an automatic improver run, e.g. `("workhub-dev", "schedule:20412")`:
    /// two processes evaluating the same schedule slot / trigger produce the same id.
    pub fn for_key(factory: &str, key: &str) -> Self {
        let ns = Uuid::new_v5(&Uuid::NAMESPACE_OID, format!("workhub-improver:{factory}").as_bytes());
        Self(Uuid::new_v5(&ns, key.as_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_tag_matches_event_type() {
        let e = ProposalEvent::ProposalDecided { approved: true, reviewer: "me".into(), comment: None };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["type"], e.event_type());
        assert_eq!(serde_json::from_value::<ProposalEvent>(v).unwrap(), e);
    }

    #[test]
    fn deterministic_ids_per_factory_and_key() {
        assert_eq!(ProposalId::for_key("f", "schedule:1"), ProposalId::for_key("f", "schedule:1"));
        assert_ne!(ProposalId::for_key("f", "schedule:1"), ProposalId::for_key("f", "schedule:2"));
        assert_ne!(ProposalId::for_key("f", "schedule:1"), ProposalId::for_key("g", "schedule:1"));
    }

    #[test]
    fn fold_follows_lifecycle() {
        let id = Uuid::now_v7();
        let s = ProposalState::fold([
            (1, id, ProposalEvent::ImprovementRequested { factory: "f".into(), trigger: "manual".into(), focus: None }),
            (2, Uuid::now_v7(), ProposalEvent::ImprovementStarted {
                base_factory_version: "aaa".into(),
                window_days: 14,
                sessions_observed: 3,
            }),
            (3, Uuid::now_v7(), ProposalEvent::ProposalDecided { approved: false, reviewer: "me".into(), comment: None }),
        ]);
        assert_eq!((s.version, s.status, s.first_event), (3, ProposalStatus::Rejected, Some(id)));
        assert_eq!(s.base_factory_version.as_deref(), Some("aaa"));
    }
}
