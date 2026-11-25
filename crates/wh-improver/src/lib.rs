//! Self-improvement: observe the factory's metrics, let an agent diagnose a
//! pattern and propose a change to the factory definition, and track the
//! proposal through human review, rollout and before/after measurement.
//!
//! - **Observe** ([`observe`]): per-version / per-stage metrics, failures,
//!   scorer findings, review and human feedback, past proposals, example transcripts.
//! - **Propose**: [`ImproverService`] runs the improver agent (Claude Code) in a
//!   git worktree of the repo holding the factory, on branch `wh/proposal/<id>`.
//!   It may only edit files in the factory's directory. WorkHub validates the
//!   changed factory, commits it and records `ProposalCreated` with the diff and
//!   the factory version the change produces.
//! - **Review / Rollout** ([`commands`]): approve or reject (with a comment the
//!   improver reads next time), then apply = cherry-pick onto the factory repo.
//!   A proposal whose version shows up as the loaded factory is detected as rolled out.
//! - **Measure**: `wh_metrics::proposals::version_stats` compares versions.
//!
//! Runs are requested by `ImprovementRequested` events (human, schedule or
//! trigger; automatic ones have deterministic ids) and claimed with
//! `ImprovementStarted` at `Exact(1)`, so each request runs at most once.

pub mod commands;
pub mod observe;
pub mod prompt;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use anyhow::Context;
use tokio::sync::Semaphore;
use wh_core::{
    Actor, EventMeta, NewEvent, ProposalId, RecordedEvent,
    proposal::{ProposalEvent, ProposalStatus},
};
use wh_eventstore::{EventHandler, EventStore, ExpectedVersion, StoreError};
use wh_factory::{Factory, Improvement};
use wh_runner::{AgentRequest, AgentRuntime, OutcomeStatus, worktree::Worktree};

pub use observe::{Observation, render};
pub use prompt::{Answer, Fired, Workspace, build_prompt, evaluate_triggers, parse_answer};

pub(crate) mod git {
    use std::path::Path;

    use anyhow::{Context, bail};

    pub async fn run(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
        let out = tokio::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .await
            .context("failed to run git")?;
        if !out.status.success() {
            bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_owned())
    }
}

#[derive(Clone)]
pub enum Dispatch {
    /// Run inside the caller (tests, `wh improve`).
    Inline,
    /// Run in the background, at most `n` at a time.
    Spawn(Arc<Semaphore>),
}

#[derive(Debug, Clone)]
pub struct ImproverConfig {
    /// Worktrees under `<data_dir>/worktrees`, transcripts under `<data_dir>/transcripts`.
    pub data_dir: PathBuf,
}

/// What one improver run produced.
#[derive(Debug, Clone, PartialEq)]
pub enum RunResult {
    Proposed { title: String, files: Vec<String>, factory_version: String, cost_usd: f64 },
    NoProposal { reason: String, cost_usd: f64 },
    Failed { message: String, cost_usd: f64 },
    /// Not a pending request of this factory (already claimed, or another factory's).
    NotPending,
}

pub struct ImproverService<R> {
    inner: Arc<Inner<R>>,
    dispatch: Dispatch,
    name: String,
}

impl<R> Clone for ImproverService<R> {
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone(), dispatch: self.dispatch.clone(), name: self.name.clone() }
    }
}

struct Inner<R> {
    store: EventStore,
    factory: Arc<Factory>,
    runtime: R,
    config: ImproverConfig,
}

impl<R: AgentRuntime + 'static> ImproverService<R> {
    /// Fails if the factory has no `improvement:` section.
    pub fn new(
        store: EventStore,
        factory: Factory,
        runtime: R,
        config: ImproverConfig,
        dispatch: Dispatch,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(factory.improvement.is_some(), "factory '{}' has no `improvement:` section", factory.name);
        let name = format!("improver:{}", factory.name);
        Ok(Self { inner: Arc::new(Inner { store, factory: Arc::new(factory), runtime, config }), dispatch, name })
    }

    pub fn factory(&self) -> &Factory {
        &self.inner.factory
    }

    /// What the improver would see now (free; no agent run).
    pub async fn observe(&self) -> anyhow::Result<Observation> {
        self.inner.observe().await
    }

    /// Request a run and execute it now. Returns the proposal id and result.
    pub async fn run_now(&self, actor: Actor, focus: Option<String>) -> anyhow::Result<(ProposalId, RunResult)> {
        let id = commands::request(&self.inner.store, actor, &self.inner.factory.name, focus).await?;
        let r = self.inner.run(id).await?;
        wh_metrics::catch_up_all(&self.inner.store).await?;
        Ok((id, r))
    }

    /// Execute a pending request now.
    pub async fn run(&self, id: ProposalId) -> anyhow::Result<RunResult> {
        self.inner.run(id).await
    }

    /// Run `id` according to the dispatch mode.
    pub async fn dispatch(&self, id: ProposalId) {
        let inner = self.inner.clone();
        let job = async move {
            match inner.run(id).await {
                Ok(RunResult::NotPending) => {}
                Ok(r) => tracing::info!(proposal = %id, result = ?r, "improver run finished"),
                Err(e) => tracing::error!(proposal = %id, "improver run failed: {e:#}"),
            }
            if let Err(e) = wh_metrics::catch_up_all(&inner.store).await {
                tracing::warn!("projection catch-up after improver run: {e:#}");
            }
        };
        match &self.dispatch {
            Dispatch::Inline => job.await,
            Dispatch::Spawn(limit) => {
                let limit = limit.clone();
                tokio::spawn(async move {
                    let _permit = limit.acquire_owned().await;
                    job.await
                });
            }
        }
    }

    /// Call once at startup. Runs that were in progress when the previous process
    /// died are marked failed; returns (that count, requests still waiting to run).
    pub async fn recover(&self) -> anyhow::Result<(usize, Vec<ProposalId>)> {
        let store = &self.inner.store;
        let (mut failed, mut pending) = (0, Vec::new());
        for (stream, version, head) in store.stream_heads(&format!("{}-", ProposalId::STREAM_PREFIX)).await? {
            if head != "ImprovementStarted" && head != "ImprovementRequested" {
                continue;
            }
            let Some(id) = ProposalId::from_stream_id(&stream) else { continue };
            let Some(first) = store.read_stream(&stream).await?.into_iter().next() else { continue };
            match first.decode::<ProposalEvent>()? {
                ProposalEvent::ImprovementRequested { factory, .. } if factory == self.inner.factory.name => {}
                _ => continue,
            }
            if head == "ImprovementRequested" {
                pending.push(id);
                continue;
            }
            let ev = ProposalEvent::ImprovementFailed {
                message: "interrupted: the improver stopped while running".into(),
                cost_usd: 0.0,
                duration_ms: 0,
                transcript: None,
            };
            if self.inner.append(id, ExpectedVersion::Exact(version), Some(first.event_id), ev).await.is_ok() {
                failed += 1;
            }
        }
        Ok((failed, pending))
    }

    /// Mark approved proposals as rolled out once the loaded factory has their version
    /// (e.g. the branch was merged by hand).
    pub async fn detect_rollouts(&self) -> anyhow::Result<Vec<ProposalId>> {
        let f = &self.inner.factory;
        let ids: Vec<uuid::Uuid> = sqlx::query_scalar(
            "SELECT proposal_id FROM proposals_view WHERE factory = $1 AND status = 'approved' \
             AND proposed_factory_version = $2",
        )
        .bind(&f.name)
        .bind(&f.version)
        .fetch_all(self.inner.store.pool())
        .await?;
        let mut out = Vec::new();
        for id in ids.into_iter().map(ProposalId) {
            let s = commands::load(&self.inner.store, id).await?;
            if s.status != ProposalStatus::Approved {
                continue;
            }
            let ev = ProposalEvent::ProposalRolledOut { factory_version: f.version.clone(), commit: None };
            if self.inner.append(id, ExpectedVersion::Exact(s.version), s.first_event, ev).await.is_ok() {
                out.push(id);
            }
        }
        if !out.is_empty() {
            wh_metrics::catch_up_all(&self.inner.store).await?;
        }
        Ok(out)
    }

    /// Evaluate triggers and the schedule; request runs that are due. Returns the
    /// new requests (they run when the consumer sees them).
    pub async fn tick(&self) -> anyhow::Result<Vec<(ProposalId, String)>> {
        let inner = &self.inner;
        let (f, im) = (&inner.factory, inner.improvement());
        let pool = inner.store.pool();
        wh_metrics::catch_up_all(&inner.store).await?;
        self.detect_rollouts().await?;

        let mut queued: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM proposals_view WHERE factory = $1 AND status IN ('requested', 'running', 'open')",
        )
        .bind(&f.name)
        .fetch_one(pool)
        .await?;
        let mut out = Vec::new();
        let room = |q: i64| q < im.max_open_proposals as i64;

        if !im.triggers.is_empty() && room(queued) {
            let (stages, scorers) = observe::current_metrics(pool, &f.name, &f.version, im.window_days).await?;
            for fired in evaluate_triggers(&im.triggers, &stages, &scorers, im.min_sessions as i64) {
                if !room(queued) {
                    break;
                }
                // Once per trigger per factory version: a rollout re-arms it.
                let id = ProposalId::for_key(&f.name, &format!("trigger:{}@{}", fired.trigger.key(), f.version));
                let why = format!("trigger: {}", fired.describe());
                if inner.request_auto(id, &why).await? {
                    queued += 1;
                    out.push((id, why));
                }
            }
        }

        if let Some(hours) = im.interval_hours.filter(|_| room(queued)) {
            let new_sessions: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM session_facts s JOIN tasks_view t ON t.task_id = s.task_id \
                 WHERE t.factory = $1 AND t.benchmark_run IS NULL AND s.status IN ('succeeded', 'failed') AND s.ended_at > COALESCE( \
                   (SELECT max(requested_at) FROM proposals_view WHERE factory = $1), '-infinity')",
            )
            .bind(&f.name)
            .fetch_one(pool)
            .await?;
            if new_sessions >= im.min_sessions as i64 {
                let slot = prompt::schedule_slot(chrono::Utc::now().timestamp(), hours);
                let id = ProposalId::for_key(&f.name, &format!("schedule:{hours}h:{slot}"));
                let why = format!("schedule: every {hours}h, {new_sessions} new sessions");
                if inner.request_auto(id, &why).await? {
                    out.push((id, why));
                }
            }
        }
        if !out.is_empty() {
            wh_metrics::catch_up_all(&inner.store).await?;
        }
        Ok(out)
    }
}

impl<R: AgentRuntime> Inner<R> {
    fn improvement(&self) -> &Improvement {
        self.factory.improvement.as_ref().expect("checked in ImproverService::new")
    }

    fn meta(&self, id: ProposalId, cause: Option<uuid::Uuid>) -> EventMeta {
        let mut meta = EventMeta::new(id.0, Actor::System { component: "improver".into() });
        meta.causation_id = cause;
        meta.factory_version = Some(self.factory.version.clone());
        meta
    }

    async fn append(
        &self,
        id: ProposalId,
        expected: ExpectedVersion,
        cause: Option<uuid::Uuid>,
        ev: ProposalEvent,
    ) -> Result<Vec<RecordedEvent>, StoreError> {
        let new = NewEvent::from_domain(&ev, self.meta(id, cause))?;
        self.store.append(&id.stream_id(), expected, vec![new]).await
    }

    /// Append an automatic request; false if that id was already requested.
    async fn request_auto(&self, id: ProposalId, trigger: &str) -> anyhow::Result<bool> {
        let ev = ProposalEvent::ImprovementRequested {
            factory: self.factory.name.clone(),
            trigger: trigger.to_owned(),
            focus: None,
        };
        match self.append(id, ExpectedVersion::NoStream, None, ev).await {
            Ok(_) => Ok(true),
            Err(StoreError::Conflict { .. }) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    async fn observe(&self) -> anyhow::Result<Observation> {
        let im = self.improvement();
        observe::gather(self.store.pool(), &self.factory.name, &self.factory.version, im.window_days).await
    }

    async fn run(&self, id: ProposalId) -> anyhow::Result<RunResult> {
        let events = self.store.read_stream(&id.stream_id()).await?;
        let [first] = events.as_slice() else { return Ok(RunResult::NotPending) };
        let ProposalEvent::ImprovementRequested { factory, focus, .. } = first.decode()? else {
            return Ok(RunResult::NotPending);
        };
        if factory != self.factory.name {
            return Ok(RunResult::NotPending);
        }
        let im = self.improvement();
        let obs = self.observe().await?;
        let started = ProposalEvent::ImprovementStarted {
            base_factory_version: self.factory.version.clone(),
            window_days: im.window_days,
            sessions_observed: obs.sessions(),
        };
        let rec = match self.append(id, ExpectedVersion::Exact(1), Some(first.event_id), started).await {
            Ok(r) => r,
            Err(StoreError::Conflict { .. }) => return Ok(RunResult::NotPending),
            Err(e) => return Err(e.into()),
        };
        let t0 = Instant::now();
        let (event, result) = match self.execute(id, &obs, focus.as_deref(), t0).await {
            Ok(r) => r,
            Err(e) => failed(format!("{e:#}"), 0.0, t0, None),
        };
        self.append(id, ExpectedVersion::Exact(2), rec.last().map(|r| r.event_id), event).await?;
        Ok(result)
    }

    /// Run the agent in a worktree and turn what it did into the final event.
    async fn execute(
        &self,
        id: ProposalId,
        obs: &Observation,
        focus: Option<&str>,
        t0: Instant,
    ) -> anyhow::Result<(ProposalEvent, RunResult)> {
        let im = self.improvement();
        let factory_dir = self.factory.path.parent().context("factory file has no parent directory")?;
        let root = PathBuf::from(git::run(factory_dir, &["rev-parse", "--show-toplevel"]).await?);
        let root = std::fs::canonicalize(&root).with_context(|| format!("canonicalize {}", root.display()))?;
        let rel_file = self
            .factory
            .path
            .strip_prefix(&root)
            .context("factory file is not inside its git repository")?
            .to_path_buf();
        let ws = Workspace {
            factory_file: slash(&rel_file),
            factory_dir: rel_file.parent().map(slash).unwrap_or_default(),
        };

        let branch = format!("wh/proposal/{id}");
        let wt_path = self.config.data_dir.join("worktrees").join(format!("proposal-{id}"));
        let wt = Worktree::create(&root, "HEAD", &wt_path, &branch).await?;
        let transcripts = self.config.data_dir.join("transcripts");
        tokio::fs::create_dir_all(&transcripts).await.context("create transcripts dir")?;
        let transcript_path = transcripts.join(format!("proposal-{id}.jsonl"));

        let req = AgentRequest {
            prompt: build_prompt(im, &ws, &render(obs), focus),
            workdir: wt.path.clone(),
            transcript_path: transcript_path.clone(),
            model: Some(im.model.clone()),
            budget_usd: Some(im.budget_usd),
            permission_mode: Some("acceptEdits".into()),
            allowed_tools: im.allowed_tools.clone(),
            with_user_mcp: false,
            timeout: im.timeout,
            tools: None,
            container: None,
        };
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let o = self.runtime.run(&req, tx).await;
        let transcript = transcript_path.exists().then(|| transcript_path.display().to_string());

        let r = self.collect(&wt, &root, &ws, o, transcript, t0).await;
        if let Err(e) = wt.remove().await {
            tracing::warn!("remove improver worktree: {e:#}");
        }
        let proposed = matches!(&r, Ok((ProposalEvent::ProposalCreated { .. }, _)));
        if !proposed {
            let _ = git::run(&root, &["branch", "-D", &branch]).await;
        }
        r
    }

    async fn collect(
        &self,
        wt: &Worktree,
        root: &Path,
        ws: &Workspace,
        o: wh_runner::Outcome,
        transcript: Option<String>,
        t0: Instant,
    ) -> anyhow::Result<(ProposalEvent, RunResult)> {
        let cost = o.cost_usd;
        if let OutcomeStatus::Failed { class, message, .. } = &o.status {
            return Ok(failed(format!("improver failed [{}]: {message}", class.as_str()), cost, t0, transcript));
        }
        git::run(&wt.path, &["add", "-A"]).await?;
        let files: Vec<String> = git::run(&wt.path, &["diff", "--cached", "--name-only"])
            .await?
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(str::to_owned)
            .collect();
        let answer = parse_answer(o.summary.as_deref().unwrap_or(""));
        let (title, rationale, expected_impact, evidence) = match (files.is_empty(), answer) {
            (true, Ok(Answer::NoProposal { reason })) => {
                let ev = ProposalEvent::ImprovementSkipped {
                    reason: reason.clone(),
                    cost_usd: cost,
                    duration_ms: ms(t0),
                    transcript,
                };
                return Ok((ev, RunResult::NoProposal { reason, cost_usd: cost }));
            }
            (true, Ok(Answer::Proposal { .. })) => {
                return Ok(failed("the improver described a change but edited no files".into(), cost, t0, transcript));
            }
            (_, Err(e)) => return Ok(failed(e, cost, t0, transcript)),
            (false, Ok(Answer::NoProposal { .. })) => {
                let msg = format!("the improver answered no_proposal but edited {}", files.join(", "));
                return Ok(failed(msg, cost, t0, transcript));
            }
            (false, Ok(Answer::Proposal { title, rationale, expected_impact, evidence })) => {
                (title, rationale, expected_impact, evidence)
            }
        };
        let outside = prompt::outside_dir(&files, &ws.factory_dir);
        if !outside.is_empty() {
            let msg = format!("the improver edited files outside {}/: {}", ws.factory_dir, outside.join(", "));
            return Ok(failed(msg, cost, t0, transcript));
        }
        let proposed = match Factory::load(&wt.path.join(&ws.factory_file)) {
            Ok(f) => f,
            Err(e) => return Ok(failed(format!("factory is invalid after the change: {e:#}"), cost, t0, transcript)),
        };
        if proposed.version == self.factory.version {
            let msg = "the change does not affect the factory definition (factory_version unchanged)".into();
            return Ok(failed(msg, cost, t0, transcript));
        }
        let message = format!("proposal: {title}\n\n{rationale}\n");
        let commit = wt.commit_all(&message).await?.context("nothing to commit")?;
        let diff = git::run(&wt.path, &["show", "--format=", "--stat", "--patch", &commit]).await?;
        let ev = ProposalEvent::ProposalCreated {
            title: title.clone(),
            rationale,
            expected_impact,
            evidence,
            files: files.clone(),
            diff,
            branch: wt.branch.clone(),
            commit,
            proposed_factory_version: proposed.version.clone(),
            factory_repo: root.display().to_string(),
            factory_file: ws.factory_file.clone(),
            cost_usd: cost,
            usage: o.usage,
            duration_ms: ms(t0),
            transcript,
        };
        Ok((ev, RunResult::Proposed { title, files, factory_version: proposed.version, cost_usd: cost }))
    }
}

fn failed(message: String, cost_usd: f64, t0: Instant, transcript: Option<String>) -> (ProposalEvent, RunResult) {
    let ev = ProposalEvent::ImprovementFailed { message: message.clone(), cost_usd, duration_ms: ms(t0), transcript };
    (ev, RunResult::Failed { message, cost_usd })
}

fn ms(t0: Instant) -> u64 {
    t0.elapsed().as_millis() as u64
}

fn slash(p: &Path) -> String {
    p.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/")
}

impl<R: AgentRuntime + 'static> EventHandler for ImproverService<R> {
    fn name(&self) -> &str {
        &self.name
    }

    async fn handle(&mut self, ev: &RecordedEvent) -> anyhow::Result<()> {
        if ev.event_type != "ImprovementRequested" {
            return Ok(());
        }
        let Some(id) = ProposalId::from_stream_id(&ev.stream_id) else { return Ok(()) };
        match ev.decode::<ProposalEvent>()? {
            ProposalEvent::ImprovementRequested { factory, .. } if factory == self.inner.factory.name => {
                self.dispatch(id).await;
            }
            _ => {}
        }
        Ok(())
    }
}
