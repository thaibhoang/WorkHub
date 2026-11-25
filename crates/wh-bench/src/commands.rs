//! Commands on benchmark runs, shared by the CLI and the API: request a run
//! (picked up by `wh bench worker` / `wh serve --foreman`), or request the
//! base/candidate pair that evaluates an improver proposal.

use std::path::PathBuf;

use wh_core::{
    Actor, BenchmarkRunId, EventMeta, NewEvent, ProposalId,
    benchmark::BenchmarkEvent,
};
use wh_eventstore::{EventStore, ExpectedVersion, StoreError};

use crate::suite::Suite;

#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<StoreError> for CommandError {
    fn from(e: StoreError) -> Self {
        Self::Other(e.into())
    }
}

pub type Result<T> = std::result::Result<T, CommandError>;

/// What to benchmark.
#[derive(Debug, Clone)]
pub struct Request {
    pub suite_file: PathBuf,
    /// Factory variant: the suite's factory file at this git ref (None = working tree).
    pub git_ref: Option<String>,
    pub proposal: Option<ProposalId>,
    pub label: Option<String>,
    pub repeat: u32,
    /// Only these case ids (empty = all).
    pub cases: Vec<String>,
}

impl Request {
    /// Validate against the suite file and build the `BenchmarkRequested` event.
    pub fn to_event(&self) -> Result<(Suite, BenchmarkEvent)> {
        let suite = Suite::load(&self.suite_file).map_err(|e| CommandError::Invalid(format!("{e:#}")))?;
        suite.plan(&self.cases, self.repeat).map_err(|e| CommandError::Invalid(format!("{e:#}")))?;
        if let Some(r) = self.git_ref.as_ref().filter(|r| r.trim().is_empty() || r.starts_with('-')) {
            return Err(CommandError::Invalid(format!("invalid git ref '{r}'")));
        }
        let ev = BenchmarkEvent::BenchmarkRequested {
            suite: suite.name.clone(),
            suite_file: suite.path.display().to_string(),
            git_ref: self.git_ref.clone(),
            proposal: self.proposal,
            label: self.label.clone(),
            repeat: self.repeat,
            cases: self.cases.clone(),
        };
        Ok((suite, ev))
    }
}

/// Queue a run for the benchmark worker.
pub async fn request(store: &EventStore, actor: Actor, req: &Request) -> Result<BenchmarkRunId> {
    let (_, ev) = req.to_event()?;
    let id = BenchmarkRunId::new();
    let meta = EventMeta::new(id.0, actor);
    store.append(&id.stream_id(), ExpectedVersion::NoStream, vec![NewEvent::from_domain(&ev, meta).map_err(anyhow::Error::from)?]).await?;
    Ok(id)
}

/// Git refs that evaluate a proposal: its parent commit (the factory it was
/// based on) and its own commit.
pub async fn proposal_refs(store: &EventStore, proposal: ProposalId) -> Result<(String, String)> {
    let state = match wh_improver::commands::load(store, proposal).await {
        Ok(s) => s,
        Err(wh_improver::commands::CommandError::NotFound(_)) => {
            return Err(CommandError::NotFound(format!("proposal {proposal} not found")));
        }
        Err(e) => return Err(CommandError::Other(anyhow::anyhow!("{e}"))),
    };
    let commit = state
        .commit
        .ok_or_else(|| CommandError::Invalid(format!("proposal {proposal} has no commit ({})", state.status.as_str())))?;
    Ok((format!("{commit}^"), commit))
}

/// Queue the base and candidate runs for a proposal. Returns (base, candidate).
pub async fn request_proposal(
    store: &EventStore,
    actor: Actor,
    suite_file: PathBuf,
    proposal: ProposalId,
    repeat: u32,
    cases: Vec<String>,
) -> Result<(BenchmarkRunId, BenchmarkRunId)> {
    let (base, candidate) = proposal_refs(store, proposal).await?;
    let req = |git_ref: String, label: &str| Request {
        suite_file: suite_file.clone(),
        git_ref: Some(git_ref),
        proposal: Some(proposal),
        label: Some(label.into()),
        repeat,
        cases: cases.clone(),
    };
    let a = request(store, actor.clone(), &req(base, "base")).await?;
    let b = request(store, actor, &req(candidate, "proposal")).await?;
    Ok((a, b))
}
