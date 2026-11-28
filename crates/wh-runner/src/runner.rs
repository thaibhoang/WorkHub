use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use uuid::Uuid;
use wh_core::{
    Actor, BenchmarkRunId, EventMeta, FailureClass, NewEvent, Question, Sandbox, SessionId, TaskId, parse_questions,
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
    /// Pause instead of ending the stage when the agent's final message ends with
    /// `QUESTION:` lines. Only for callers that can [`Runner::resume`] it.
    pub pause_on_question: bool,
}

/// A stage waiting for the user: its worktree (and sandbox home) are kept, nothing
/// is committed and there is no `StageExited`. Everything [`Runner::resume`] needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paused {
    /// The session that asked (the last turn).
    pub session: SessionId,
    /// The first turn; names `worktrees/<id>` and `sandbox-home/<id>`.
    pub home_session: SessionId,
    pub runtime_session_id: String,
    pub branch: String,
    /// Paths of a merge's conflicts, checked for markers before committing.
    pub conflicts: Vec<String>,
    pub questions: Vec<Question>,
}

/// Result of [`Runner::resume`].
#[derive(Debug, Clone)]
pub enum Resumed {
    /// The turn ran: the stage ended, or paused again.
    Ran(RunReport),
    /// The worktree, sandbox home or conversation is gone. There is no `StageExited`
    /// and what is left on disk stays. `session` is the failed session, if one was
    /// recorded.
    Lost { reason: String, session: Option<SessionId> },
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
    /// Outcome recorded on `StageExited` (includes the verdict). `None` = the stage
    /// has not ended (paused).
    pub stage_outcome: Option<StageOutcome>,
    /// Set when the stage is waiting for the user's answer.
    pub paused: Option<Paused>,
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
    /// With `pause_on_question`, an attempt that asks ends the run without `StageExited`.
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
            let (report, end) = self.attempt(&spec, &mut rec, attempt, retry_of, &mut on_signal).await?;
            if let End::Paused(paused) = end {
                attempts.push(report);
                return Ok(RunReport { attempts, stage_outcome: None, paused: Some(paused) });
            }
            let retry = attempt <= spec.max_retries
                && matches!(report.outcome.status, OutcomeStatus::Failed { retryable: true, .. });
            if !retry {
                let outcome = exit_stage(&spec, &mut rec, &report.outcome).await?;
                attempts.push(report);
                return Ok(RunReport { attempts, stage_outcome: Some(outcome), paused: None });
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

    /// Continue a paused stage with the user's `answer`: one new session in the same
    /// worktree (and sandbox home), never retried, with the full timeout and budget of
    /// `spec`. The stage is committed and gets `StageExited` only if it really ends.
    /// No `StageEntered` is appended, so the caller must make sure it resumes only once.
    pub async fn resume(
        &self,
        spec: RunSpec,
        paused: &Paused,
        answer: &str,
        mut on_signal: impl FnMut(&Signal) + Send,
    ) -> anyhow::Result<Resumed> {
        let home = paused.home_session.to_string();
        let path = self.config.data_dir.join("worktrees").join(&home);
        let lost = |reason: String| Ok(Resumed::Lost { reason, session: None });
        let worktree = match Worktree::open(&spec.repo, &path, &paused.branch).await {
            Ok(w) => w,
            Err(e) => return lost(format!("{e:#}")),
        };
        // Checked here because `docker::prepare` would create an empty one.
        let home_dir = self.config.data_dir.join("sandbox-home").join(&home);
        if spec.sandbox.docker().is_some() && !home_dir.is_dir() {
            return lost(format!("sandbox home {} is gone", home_dir.display()));
        }
        let mut rec = Recorder::new(&self.store, SessionId::new(), spec.task_id, spec.factory_version.clone());
        let turn = Turn {
            // The fixed prefix also keeps an answer starting with `-` from being read as a flag.
            prompt: format!("The user answered your question(s):\n\n{answer}"),
            conflicts: paused.conflicts.clone(),
            home_session: paused.home_session,
            mode: SessionMode::Resume(paused.runtime_session_id.clone()),
            attempt: 1,
            retry_of: None,
            resume_of: Some(paused.session),
        };
        let (report, end) = self.turn(&spec, &mut rec, turn, Ok(worktree), &mut on_signal).await?;
        let (stage_outcome, paused) = match end {
            End::Lost(reason) => return Ok(Resumed::Lost { reason, session: Some(report.session_id) }),
            End::Paused(paused) => (None, Some(paused)),
            End::Finished => (Some(exit_stage(&spec, &mut rec, &report.outcome).await?), None),
        };
        Ok(Resumed::Ran(RunReport { attempts: vec![report], stage_outcome, paused }))
    }

    /// The first turn of an attempt: a fresh worktree (and merge), then [`Self::turn`].
    async fn attempt(
        &self,
        spec: &RunSpec,
        s: &mut Recorder<'_>,
        attempt: u32,
        retry_of: Option<SessionId>,
        on_signal: &mut (impl FnMut(&Signal) + Send),
    ) -> anyhow::Result<(SessionReport, End)> {
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
                    conflicts = files.into_iter().map(|f| f.path).collect();
                }
                Err(e) => {
                    if let Err(e) = w.remove().await {
                        tracing::warn!("failed to remove worktree {}: {e:#}", w.path.display());
                    }
                    setup = Err(e.context(format!("merging {}/{}", m.remote, m.base)));
                }
            }
        }
        let turn = Turn {
            prompt,
            conflicts,
            home_session: session_id,
            mode: SessionMode::Persist,
            attempt,
            retry_of,
            resume_of: None,
        };
        self.turn(spec, s, turn, setup, on_signal).await
    }

    /// Record and run one session in the prepared worktree, then keep it (paused or
    /// lost) or commit and clean up. `StageEntered`/`StageExited` are the caller's.
    async fn turn(
        &self,
        spec: &RunSpec,
        s: &mut Recorder<'_>,
        turn: Turn,
        setup: anyhow::Result<Worktree>,
        on_signal: &mut (impl FnMut(&Signal) + Send),
    ) -> anyhow::Result<(SessionReport, End)> {
        let session_id = s.session_id;
        let Turn { prompt, conflicts, home_session, mode, attempt, retry_of, resume_of } = turn;
        let resuming = matches!(mode, SessionMode::Resume(_));
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
            resume_of,
            benchmark: spec.benchmark,
        })
        .await?;

        // A turn that never got to run the agent.
        let not_started = |outcome| {
            let report = SessionReport { session_id, attempt, outcome, workdir: None, branch: None };
            Ok((report, End::Finished))
        };
        let worktree = match setup {
            Ok(w) => w,
            Err(e) => {
                let outcome = Outcome::failed(FailureClass::EnvSetup, format!("{e:#}"), false, Duration::ZERO);
                s.finish(&outcome, None, None).await?;
                return not_started(outcome);
            }
        };
        let container = match spec.sandbox.docker() {
            None => None,
            Some(cfg) => {
                match docker::prepare(session_id, home_session, cfg, &worktree.path, &self.config.data_dir).await {
                    Ok(c) => Some(c),
                    Err(e) => {
                        let msg = format!("docker sandbox: {e:#}");
                        let outcome = Outcome::failed(FailureClass::EnvSetup, msg, false, Duration::ZERO);
                        s.finish(&outcome, None, None).await?;
                        if let Err(e) = worktree.remove().await {
                            tracing::warn!("failed to remove worktree {}: {e:#}", worktree.path.display());
                        }
                        remove_home(&self.config.data_dir, home_session).await;
                        return not_started(outcome);
                    }
                }
            }
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
            session: mode,
        };

        let (tx, mut rx) = mpsc::unbounded_channel();
        let fut = self.runtime.run(&req, tx);
        tokio::pin!(fut);
        let mut runtime_session_id = None;
        let mut observe = |sig: &Signal| {
            if let Signal::Ready { runtime_session_id: Some(id), .. } = sig {
                runtime_session_id = Some(id.clone());
            }
            on_signal(sig);
        };
        let mut outcome = loop {
            tokio::select! {
                outcome = &mut fut => break outcome,
                Some(sig) = rx.recv() => {
                    observe(&sig);
                    s.signal(sig).await?;
                }
            }
        };
        while let Ok(sig) = rx.try_recv() {
            observe(&sig);
            s.signal(sig).await?;
        }

        // Asking comes before the marker check and the verdict: the work is unfinished.
        let questions = match (&outcome.status, outcome.summary.as_deref()) {
            (OutcomeStatus::Succeeded, Some(text)) if spec.pause_on_question => parse_questions(text),
            _ => Vec::new(),
        };
        let end = match (&outcome.status, runtime_session_id) {
            (OutcomeStatus::Failed { class: FailureClass::SessionLost, message, .. }, _) if resuming => {
                End::Lost(message.clone())
            }
            (_, Some(runtime_session_id)) if !questions.is_empty() => End::Paused(Paused {
                session: session_id,
                home_session,
                runtime_session_id,
                branch: worktree.branch.clone(),
                conflicts: conflicts.clone(),
                questions,
            }),
            _ => End::Finished,
        };
        if req.container.is_some() {
            // The throwaway home may hold a copy of the user's credentials. A home that
            // is kept loses only that copy (`docker::prepare` makes it again).
            match end {
                End::Finished => remove_home(&self.config.data_dir, home_session).await,
                End::Paused(_) | End::Lost(_) => remove_credentials(&self.config.data_dir, home_session).await,
            }
        }
        let transcript = transcript_path.exists().then(|| transcript_path.display().to_string());
        if !matches!(end, End::Finished) {
            s.finish(&outcome, transcript, None).await?;
            let workdir = Some(worktree.path);
            let report = SessionReport { session_id, attempt, outcome, workdir, branch: Some(worktree.branch) };
            return Ok((report, end));
        }

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
        s.finish(&outcome, transcript, commit).await?;

        let workdir = if spec.keep_worktree {
            Some(worktree.path.clone())
        } else {
            if let Err(e) = worktree.remove().await {
                tracing::warn!("failed to remove worktree {}: {e:#}", worktree.path.display());
            }
            None
        };
        let report = SessionReport { session_id, attempt, outcome, workdir, branch: Some(worktree.branch) };
        Ok((report, end))
    }
}

/// What differs between a first turn and a resumed one.
struct Turn {
    prompt: String,
    conflicts: Vec<String>,
    home_session: SessionId,
    mode: SessionMode,
    attempt: u32,
    retry_of: Option<SessionId>,
    resume_of: Option<SessionId>,
}

/// How a turn left the stage.
enum End {
    /// Committed and cleaned up, or failed: the stage can be exited.
    Finished,
    Paused(Paused),
    /// The conversation to resume is gone (reason).
    Lost(String),
}

/// Append the stage's `StageExited` for its final session.
async fn exit_stage(spec: &RunSpec, rec: &mut Recorder<'_>, o: &Outcome) -> anyhow::Result<StageOutcome> {
    let (outcome, summary) = stage_result(o, spec.verdict_required);
    let session = Some(rec.session_id);
    let exited = TaskEvent::StageExited { stage: spec.stage.clone(), outcome, summary, session };
    rec.task(exited, ExpectedVersion::Any).await?;
    Ok(outcome)
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

async fn remove_credentials(data_dir: &std::path::Path, session: SessionId) {
    let file = data_dir.join("sandbox-home").join(session.to_string()).join(".claude/.credentials.json");
    if let Err(e) = tokio::fs::remove_file(&file).await
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!("failed to remove {}: {e}", file.display());
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
