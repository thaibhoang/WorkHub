//! Human commands on proposals: request an improver run, decide, apply. Each
//! validates against the folded proposal state and appends with
//! `ExpectedVersion::Exact`, so a decision on stale state is rejected (409).
//! Shared by the CLI and the API.

use std::path::Path;

use wh_core::{
    Actor, EventMeta, NewEvent, ProposalId,
    proposal::{ProposalEvent, ProposalState, ProposalStatus},
};
use wh_eventstore::{EventStore, ExpectedVersion, StoreError};
use wh_factory::Factory;

use crate::git;

#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    #[error("proposal {0} not found")]
    NotFound(ProposalId),
    #[error("{0}")]
    InvalidState(String),
    #[error("the proposal changed meanwhile; reload and try again")]
    Conflict,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<StoreError> for CommandError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::Conflict { .. } => Self::Conflict,
            e => Self::Other(e.into()),
        }
    }
}

pub type Result<T> = std::result::Result<T, CommandError>;

pub async fn load(store: &EventStore, id: ProposalId) -> Result<ProposalState> {
    let events = store.read_stream(&id.stream_id()).await?;
    if events.is_empty() {
        return Err(CommandError::NotFound(id));
    }
    let mut s = ProposalState::default();
    for e in &events {
        s.apply(e.stream_version, e.event_id, e.decode().map_err(anyhow::Error::from)?);
    }
    Ok(s)
}

/// Ask the improver of `factory` to run (picked up by `wh improver` / `wh serve --foreman`).
pub async fn request(store: &EventStore, actor: Actor, factory: &str, focus: Option<String>) -> Result<ProposalId> {
    let id = ProposalId::new();
    let ev = ProposalEvent::ImprovementRequested {
        factory: factory.to_owned(),
        trigger: "manual".into(),
        focus: focus.filter(|f| !f.trim().is_empty()),
    };
    append(store, id, ExpectedVersion::NoStream, EventMeta::new(id.0, actor), ev).await?;
    Ok(id)
}

/// Approve or reject an open proposal. Rejecting needs a comment: the improver
/// reads it next time so it does not repeat the idea.
pub async fn decide(
    store: &EventStore,
    actor: Actor,
    id: ProposalId,
    approved: bool,
    comment: Option<String>,
) -> Result<()> {
    let comment = comment.filter(|c| !c.trim().is_empty());
    if !approved && comment.is_none() {
        return Err(CommandError::InvalidState("rejecting a proposal needs a comment for the improver".into()));
    }
    let s = load(store, id).await?;
    if s.status != ProposalStatus::Open {
        return Err(CommandError::InvalidState(format!("proposal is {}, not open", s.status.as_str())));
    }
    let ev = ProposalEvent::ProposalDecided { approved, reviewer: actor_name(&actor), comment };
    let meta = EventMeta::new(id.0, actor);
    append(store, id, ExpectedVersion::Exact(s.version), meta, ev).await
}

/// Roll out an approved proposal: cherry-pick its commit onto the factory repo's
/// current branch, reload the factory and record the new version. Returns it.
/// Running foremen keep the old definition until restarted.
pub async fn apply(store: &EventStore, actor: Actor, id: ProposalId) -> Result<String> {
    let s = load(store, id).await?;
    if s.status != ProposalStatus::Approved {
        return Err(CommandError::InvalidState(format!(
            "only approved proposals can be applied (proposal is {})",
            s.status.as_str()
        )));
    }
    let (Some(repo), Some(file), Some(commit)) = (&s.factory_repo, &s.factory_file, &s.commit) else {
        return Err(CommandError::InvalidState("proposal has no commit to apply".into()));
    };
    let repo = Path::new(repo);
    let factory_path = repo.join(file);
    let withdraw_hint = format!("withdraw it instead: wh proposal withdraw {id} --comment \"...\"");
    if !repo.is_dir() {
        return Err(CommandError::InvalidState(format!(
            "the repo of factory `{}` no longer exists at {}, so this proposal cannot be applied; {withdraw_hint}",
            s.factory,
            repo.display()
        )));
    }
    if !factory_path.is_file() {
        return Err(CommandError::InvalidState(format!(
            "factory file {} no longer exists, so this proposal cannot be applied; {withdraw_hint}",
            factory_path.display()
        )));
    }
    let commit_ref = format!("{commit}^{{commit}}");
    if git::run(repo, &["cat-file", "-e", &commit_ref]).await.is_err() {
        return Err(CommandError::InvalidState(format!(
            "commit {commit} (branch {}) is not in {} any more, so this proposal cannot be applied; {withdraw_hint}",
            s.branch.as_deref().unwrap_or("?"),
            repo.display()
        )));
    }
    let current = Factory::load(&factory_path).map_err(|e| CommandError::Other(e.context("load current factory")))?;
    let (version, applied) = if Some(&current.version) == s.proposed_factory_version.as_ref() {
        // Already live (merged by hand).
        (current.version, None)
    } else {
        let mut args = vec!["status", "--porcelain", "--"];
        args.extend(s.files.iter().map(String::as_str));
        let dirty = git::run(repo, &args).await?;
        if !dirty.is_empty() {
            return Err(CommandError::InvalidState(format!(
                "files the proposal changes have uncommitted edits in {}:\n{dirty}",
                repo.display()
            )));
        }
        if let Err(e) = git::run(repo, &["-c", "user.name=WorkHub", "-c", "user.email=workhub@localhost", "cherry-pick", commit]).await {
            let _ = git::run(repo, &["cherry-pick", "--abort"]).await;
            return Err(CommandError::InvalidState(format!(
                "could not apply {commit} cleanly ({e:#}); merge branch {} by hand",
                s.branch.as_deref().unwrap_or("?")
            )));
        }
        let head = git::run(repo, &["rev-parse", "HEAD"]).await?;
        let f = Factory::load(&factory_path).map_err(|e| CommandError::Other(e.context("load applied factory")))?;
        (f.version, Some(head))
    };
    let ev = ProposalEvent::ProposalRolledOut { factory_version: version.clone(), commit: applied };
    let mut meta = EventMeta::new(id.0, actor);
    meta.factory_version = Some(version.clone());
    append(store, id, ExpectedVersion::Exact(s.version), meta, ev).await?;
    Ok(version)
}

/// Close an approved proposal without rolling it out (its repo or commit is gone,
/// or it is no longer wanted). Needs a comment: the improver reads it next time.
pub async fn withdraw(store: &EventStore, actor: Actor, id: ProposalId, comment: String) -> Result<()> {
    if comment.trim().is_empty() {
        return Err(CommandError::InvalidState("withdrawing a proposal needs a comment for the improver".into()));
    }
    let s = load(store, id).await?;
    if s.status != ProposalStatus::Approved {
        return Err(CommandError::InvalidState(format!(
            "only approved proposals can be withdrawn (proposal is {}){}",
            s.status.as_str(),
            if s.status == ProposalStatus::Open { "; reject it instead" } else { "" }
        )));
    }
    let ev = ProposalEvent::ProposalWithdrawn { reviewer: actor_name(&actor), comment };
    let meta = EventMeta::new(id.0, actor);
    append(store, id, ExpectedVersion::Exact(s.version), meta, ev).await
}

pub(crate) async fn append(
    store: &EventStore,
    id: ProposalId,
    expected: ExpectedVersion,
    meta: EventMeta,
    ev: ProposalEvent,
) -> Result<()> {
    let new = NewEvent::from_domain(&ev, meta).map_err(anyhow::Error::from)?;
    store.append(&id.stream_id(), expected, vec![new]).await?;
    wh_metrics::catch_up_all(store).await?;
    Ok(())
}

fn actor_name(a: &Actor) -> String {
    match a {
        Actor::Human { user } => user.clone(),
        Actor::Agent { session } => format!("agent:{session}"),
        Actor::System { component } => component.clone(),
    }
}
