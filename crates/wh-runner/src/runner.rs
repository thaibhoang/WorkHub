use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use anyhow::Context;
use tokio::sync::mpsc;
use uuid::Uuid;
use wh_core::{
    Actor, BenchmarkRunId, EventMeta, FailureClass, NewEvent, Sandbox, SessionId, TaskId,
    session::SessionEvent,
    task::{StageOutcome, TaskEvent, Verdict, parse_verdict},
};
use wh_eventstore::{EventStore, ExpectedVersion};

use crate::{
    docker,
    runtime::{AgentRequest, AgentRuntime, Outcome, OutcomeStatus, SessionMode, Signal},
    worktree::Worktree,
};

#[derive(Debug, Clone)]
pub struct RunnerConfig {
    /// Root for worktrees (`<data_dir>/worktrees/<session>`) and transcripts.
    pub data_dir: PathBuf,
}

/// A branch to merge into the work branch before the agent starts.
#[derive(Debug, Clone)]
pub struct Merge {
    pub remote: String,
    pub base: String,
}

/// What to run. Agent/stage are free-form until factory-as-code (phase 2) defines them.
#[derive(Debug, Clone)]
pub struct RunSpec {
    pub task_id: TaskId,
    pub agent: String,
    pub stage: String,
    pub prompt: String,
    pub repo: PathBuf,
    pub base_ref: String,
    pub model: Option<String>,
    pub budget_usd: Option<f64>,
    pub permission_mode: Option<String>,
    pub allowed_tools: Vec<String>,
    /// Built-in tool set (`None` = the runtime's default).
    pub tools: Option<Vec<String>>,
    pub with_user_mcp: bool,
    pub timeout: Duration,
    pub keep_worktree: bool,
    /// Extra attempts after a *retryable* failure (timeout, unknown). 2 = at most 3 runs.
    pub max_retries: u32,
    /// Wait before retry n is `retry_backoff * n`.
    pub retry_backoff: Duration,
    /// Branch to work on (shared by all stages of a task). `None` = a fresh branch per session.
    pub branch: Option<String>,
    /// Commit the agent's changes on the branch when the session succeeds.
    pub commit_on_success: bool,
    /// Stage outcome comes from a `VERDICT: APPROVE|REJECT` line in the agent's final message.
    pub verdict_required: bool,
    /// Append `StageEntered` only if the task stream is at this version. Lets the
    /// foreman dispatch exactly once even when events are redelivered.
    pub task_expected_version: Option<i64>,
    /// Feedback being addressed (rework); recorded on `StageEntered`.
    pub feedback: Option<String>,
    /// Stamped on every event's metadata.
    pub factory_version: Option<String>,
    /// Benchmark run the session belongs to (recorded on `AgentDispatched`).
    pub benchmark: Option<BenchmarkRunId>,
    /// Where the agent runs: directly in the worktree, or in a container over it.
    pub sandbox: Sandbox,
    /// Extra environment for the agent process.
    pub env: BTreeMap<String, String>,
    /// Fetch and merge this before the agent runs. The agent resolves conflicts and
    /// WorkHub commits the merge, so it needs no network or git write access.
    pub merge: Option<Merge>,
}

#[derive(Debug, Clone)]
pub struct SessionReport {
    pub session_id: SessionId,
    pub attempt: u32,
    pub outcome: Outcome,
    pub workdir: Option<PathBuf>,
    pub branch: Option<String>,
}

/// All attempts for one stage run, in order; the last one is final.
#[derive(Debug, Clone)]
pub struct RunReport {
    pub attempts: Vec<SessionReport>,
    /// Outcome recorded on `StageExited` (includes the verdict).
    pub stage_outcome: StageOutcome,
}

impl RunReport {
    pub fn last(&self) -> &SessionReport {
        self.attempts.last().expect("at least one attempt")
    }

    pub fn total_cost_usd(&self) -> f64 {
        self.attempts.iter().map(|a| a.outcome.cost_usd).sum()
    }
}

pub struct Runner<R> {
    store: EventStore,
    runtime: R,
    config: RunnerConfig,
}

impl<R: AgentRuntime> Runner<R> {
    pub fn new(store: EventStore, runtime: R, config: RunnerConfig) -> Self {
        Self { store, runtime, config }
    }

    /// Run a stage for a task: one agent session, retried on retryable failures up to
    /// `spec.max_retries` times. Each attempt is its own session (fresh worktree) linked
    /// by `retry_of`. The task stream gets one `StageEntered` and one final `StageExited`.
    /// Agent failures are recorded and returned; `Err` means WorkHub itself failed.
    pub async fn run(&self, spec: RunSpec, mut on_signal: impl FnMut(&Signal) + Send) -> anyhow::Result<RunReport> {
        let mut attempts = Vec::new();
        let mut retry_of = None;
        for attempt in 1..=spec.max_retries + 1 {
            let session_id = SessionId::new();
            let mut rec = Recorder::new(&self.store, session_id, spec.task_id, spec.factory_version.clone());
            if attempt == 1 {
                let expected = spec.task_expected_version.map_or(ExpectedVersion::Any, ExpectedVersion::Exact);
                let entered = TaskEvent::StageEntered { stage: spec.stage.clone(), feedback: spec.feedback.clone() };
                rec.task(entered, expected).await?;
            }
            let report = self.attempt(&spec, &mut rec, attempt, retry_of, &mut on_signal).await?;
            let retry = attempt <= spec.max_retries
                && matches!(report.outcome.status, OutcomeStatus::Failed { retryable: true, .. });
            if !retry {
                let (outcome, summary) = stage_result(&report.outcome, spec.verdict_required);
                let exited =
                    TaskEvent::StageExited { stage: spec.stage.clone(), outcome, summary, session: Some(session_id) };
                rec.task(exited, ExpectedVersion::Any).await?;
                attempts.push(report);
                return Ok(RunReport { attempts, stage_outcome: outcome });
            }
            if let OutcomeStatus::Failed { class, message, .. } = &report.outcome.status {
                tracing::warn!(attempt, max = spec.max_retries + 1, class = class.as_str(), "{message}; retrying");
            }
            retry_of = Some(session_id);
            attempts.push(report);
            tokio::time::sleep(spec.retry_backoff * attempt).await;
        }
        unreachable!("the last attempt never retries")
    }

    async fn attempt(
        &self,
        spec: &RunSpec,
        s: &mut Recorder<'_>,
        attempt: u32,
        retry_of: Option<SessionId>,
        on_signal: &mut (impl FnMut(&Signal) + Send),
    ) -> anyhow::Result<SessionReport> {
        let session_id = s.session_id;
        let branch = spec.branch.clone().unwrap_or_else(|| {
            let suffix = &session_id.0.simple().to_string()[20..];
            format!("wh/{}/{suffix}", spec.agent)
        });
        let path = self.config.data_dir.join("worktrees").join(session_id.to_string());
        // The worktree and the merge come first, so that the prompt recorded on
        // `AgentDispatched` (and sent to the agent) describes the merge result.
        let mut prompt = spec.prompt.clone();
        let mut conflicts = Vec::new();
        let mut setup = Worktree::create(&spec.repo, &spec.base_ref, &path, &branch).await;
        if let (Ok(w), Some(m)) = (&setup, &spec.merge) {
            match w.merge(&m.remote, &m.base).await {
                Ok(files) => {
                    prompt.push_str("\n\n");
                    prompt.push_str(&w.merge_note(&m.base, &files).await);
                    conflicts = files;
                }
                Err(e) => {
                    if let Err(e) = w.remove().await {
                        tracing::warn!("failed to remove worktree {}: {e:#}", w.path.display());
                    }
                    setup = Err(e.context(format!("merging {}/{}", m.remote, m.base)));
                }
            }
        }
        s.session(SessionEvent::AgentDispatched {
            task_id: spec.task_id,
            agent: spec.agent.clone(),
            stage: spec.stage.clone(),
            runtime: self.runtime.name().to_owned(),
            model: spec.model.clone(),
            prompt: prompt.clone(),
            budget_usd: spec.budget_usd,
            timeout_secs: spec.timeout.as_secs(),
            attempt,
            retry_of,
            benchmark: spec.benchmark,
        })
        .await?;

        let worktree = match setup {
            Ok(w) => w,
            Err(e) => {
                let outcome = Outcome::failed(FailureClass::EnvSetup, format!("{e:#}"), false, Duration::ZERO);
                s.finish(&outcome, None, None).await?;
                return Ok(SessionReport { session_id, attempt, outcome, workdir: None, branch: None });
            }
        };
        let container = match spec.sandbox.docker() {
            None => None,
            Some(cfg) => match docker::prepare(session_id, cfg, &worktree.path, &self.config.data_dir).await {
                Ok(c) => Some(c),
                Err(e) => {
                    let msg = format!("docker sandbox: {e:#}");
                    let outcome = Outcome::failed(FailureClass::EnvSetup, msg, false, Duration::ZERO);
                    s.finish(&outcome, None, None).await?;
                    if let Err(e) = worktree.remove().await {
                        tracing::warn!("failed to remove worktree {}: {e:#}", worktree.path.display());
                    }
                    remove_home(&self.config.data_dir, session_id).await;
                    return Ok(SessionReport { session_id, attempt, outcome, workdir: None, branch: None });
                }
            },
        };
        s.session(SessionEvent::AgentSessionStarted {
            workdir: worktree.path.display().to_string(),
            branch: worktree.branch.clone(),
            base_ref: spec.base_ref.clone(),
            sandbox: Some(spec.sandbox.clone()),
        })
        .await?;

        let transcripts = self.config.data_dir.join("transcripts");
        tokio::fs::create_dir_all(&transcripts).await.context("create transcripts dir")?;
        let transcript_path = transcripts.join(format!("{session_id}.jsonl"));
        let req = AgentRequest {
            prompt,
            workdir: worktree.path.clone(),
            transcript_path: transcript_path.clone(),
            model: spec.model.clone(),
            budget_usd: spec.budget_usd,
            permission_mode: spec.permission_mode.clone(),
            allowed_tools: spec.allowed_tools.clone(),
            with_user_mcp: spec.with_user_mcp,
            timeout: spec.timeout,
            tools: spec.tools.clone(),
            container,
            env: spec.env.clone(),
            session: SessionMode::Ephemeral,
        };

        let (tx, mut rx) = mpsc::unbounded_channel();
        let fut = self.runtime.run(&req, tx);
        tokio::pin!(fut);
        let outcome = loop {
            tokio::select! {
                outcome = &mut fut => break outcome,
                Some(sig) = rx.recv() => {
                    on_signal(&sig);
                    s.signal(sig).await?;
                }
            }
        };
        while let Ok(sig) = rx.try_recv() {
            on_signal(&sig);
            s.signal(sig).await?;
        }
        if req.container.is_some() {
            // The throwaway home may hold a copy of the user's credentials.
            remove_home(&self.config.data_dir, session_id).await;
        }

        let mut outcome = outcome;
        let mut commit = None;
        if outcome.status == OutcomeStatus::Succeeded {
            let left = worktree.with_markers(&conflicts).await;
            if !left.is_empty() {
                outcome.status = OutcomeStatus::Failed {
                    class: FailureClass::AgentGaveUp,
                    message: format!("conflict markers left in: {}", left.join(", ")),
                    retryable: false,
                };
            }
        }
        if spec.commit_on_success && outcome.status == OutcomeStatus::Succeeded {
            let msg = format!("wh({}): {} [session {session_id}]", spec.stage, spec.agent);
            match worktree.commit_all(&msg).await {
                Ok(sha) => commit = sha,
                Err(e) => {
                    outcome.status = OutcomeStatus::Failed {
                        class: FailureClass::Infra,
                        message: format!("commit failed: {e:#}"),
                        retryable: false,
                    }
                }
            }
        }
        let transcript = transcript_path.exists().then(|| transcript_path.display().to_string());
        s.finish(&outcome, transcript, commit).await?;

        let workdir = if spec.keep_worktree {
            Some(worktree.path.clone())
        } else {
            if let Err(e) = worktree.remove().await {
                tracing::warn!("failed to remove worktree {}: {e:#}", worktree.path.display());
            }
            None
        };
        Ok(SessionReport { session_id, attempt, outcome, workdir, branch: Some(worktree.branch) })
    }
}

/// Appends session events with optimistic concurrency (the runner is the only
/// writer of its session stream) and chains `causation_id`s.
struct Recorder<'a> {
    store: &'a EventStore,
    session_id: SessionId,
    stream_id: String,
    version: i64,
    task_id: TaskId,
    factory_version: Option<String>,
    last_event: Option<Uuid>,
}

impl<'a> Recorder<'a> {
    fn new(store: &'a EventStore, session: SessionId, task_id: TaskId, factory_version: Option<String>) -> Self {
        Self {
            store,
            session_id: session,
            stream_id: session.stream_id(),
            version: 0,
            task_id,
            factory_version,
            last_event: None,
        }
    }

    fn meta(&self) -> EventMeta {
        let mut meta = EventMeta::new(self.task_id.0, Actor::System { component: "runner".into() });
        meta.factory_version = self.factory_version.clone();
        match self.last_event {
            Some(id) => meta.caused_by(id),
            None => meta,
        }
    }

    async fn session(&mut self, ev: SessionEvent) -> anyhow::Result<()> {
        let expected = match self.version {
            0 => ExpectedVersion::NoStream,
            v => ExpectedVersion::Exact(v),
        };
        let rec = self.store.append(&self.stream_id, expected, vec![NewEvent::from_domain(&ev, self.meta())?]).await?;
        self.version += 1;
        self.last_event = rec.last().map(|r| r.event_id);
        Ok(())
    }

    async fn task(&mut self, ev: TaskEvent, expected: ExpectedVersion) -> anyhow::Result<()> {
        let rec = self
            .store
            .append(&self.task_id.stream_id(), expected, vec![NewEvent::from_domain(&ev, self.meta())?])
            .await?;
        self.last_event = rec.last().map(|r| r.event_id);
        Ok(())
    }

    async fn signal(&mut self, sig: Signal) -> anyhow::Result<()> {
        self.session(match sig {
            Signal::Ready { runtime_session_id, model } => SessionEvent::AgentRuntimeReady { runtime_session_id, model },
            Signal::ToolCalled { tool, ok, duration_ms } => SessionEvent::AgentToolCalled { tool, ok, duration_ms },
        })
        .await
    }

    async fn finish(&mut self, o: &Outcome, transcript: Option<String>, commit: Option<String>) -> anyhow::Result<()> {
        self.session(SessionEvent::AgentUsageReported { usage: o.usage, cost_usd: o.cost_usd }).await?;
        self.session(match &o.status {
            OutcomeStatus::Succeeded => SessionEvent::AgentSessionCompleted {
                num_turns: o.num_turns,
                duration_ms: o.duration_ms,
                summary: o.summary.clone(),
                transcript,
                commit,
            },
            OutcomeStatus::Failed { class, message, retryable } => SessionEvent::AgentSessionFailed {
                failure_class: *class,
                message: message.clone(),
                retryable: *retryable,
                num_turns: o.num_turns,
                duration_ms: o.duration_ms,
                transcript,
            },
        })
        .await
    }
}

async fn remove_home(data_dir: &std::path::Path, session: SessionId) {
    let dir = data_dir.join("sandbox-home").join(session.to_string());
    if let Err(e) = tokio::fs::remove_dir_all(&dir).await
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!("failed to remove sandbox home {}: {e}", dir.display());
    }
}

/// Map the final session outcome to the stage outcome recorded on the task.
fn stage_result(o: &Outcome, verdict_required: bool) -> (StageOutcome, Option<String>) {
    match &o.status {
        OutcomeStatus::Failed { class, message, .. } => (StageOutcome::Failed, Some(format!("[{}] {message}", class.as_str()))),
        OutcomeStatus::Succeeded if !verdict_required => (StageOutcome::Succeeded, o.summary.clone()),
        OutcomeStatus::Succeeded => match o.summary.as_deref().and_then(parse_verdict) {
            Some(Verdict::Approve) => (StageOutcome::Succeeded, o.summary.clone()),
            Some(Verdict::Reject) => (StageOutcome::Rejected, o.summary.clone()),
            None => (
                StageOutcome::Failed,
                Some(format!(
                    "agent did not end with a VERDICT line. Final message:\n{}",
                    o.summary.as_deref().unwrap_or("")
                )),
            ),
        },
    }
}
