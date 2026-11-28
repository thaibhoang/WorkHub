//! Autopilot: an independent agent that stands in for the human on tasks started
//! in autopilot mode (`TaskCreated.autopilot` / `TaskActivated.autopilot`).
//!
//! [`AutopilotService`] consumes task events. When an autopilot task of its factory
//! stops at a gate (`GateRequested`) or its agent asks questions (`AgentAsked`), it
//! builds a prompt from the task, the stage outputs and the task branch's diff,
//! runs a model with no tools, and records the decision on the task stream as a
//! human would: `GateDecided` / `UserAnswered` by [`AUTOPILOT_REVIEWER`]. The
//! foreman then carries on as usual.
//!
//! Exactly-once: each request is claimed with `AutopilotStarted` appended with
//! `ExpectedVersion::NoStream` on `autopilot-<id>`, where the id is derived from
//! (task, version of the request event), so a redelivered event never pays for a
//! second decision. The run ends with `AutopilotDecided` or `AutopilotFailed` on the
//! same stream (with its cost). A failure leaves the gate or questions for a human.

pub mod prompt;

use std::{path::PathBuf, sync::Arc, time::Instant};

use anyhow::Context;
use tokio::sync::Semaphore;
use wh_core::{
    Actor, AutopilotId, EventMeta, NewEvent, RecordedEvent, TaskId,
    autopilot::{AutopilotEvent, AutopilotKind},
    task::{AUTOPILOT_REVIEWER, CONFLICT_STAGE, TaskEvent, TaskState, TaskStatus, task_branch},
};
use wh_eventstore::{EventHandler, EventStore, ExpectedVersion, StoreError};
use wh_factory::Factory;
use wh_runner::{AgentRequest, AgentRuntime, OutcomeStatus, SessionMode};

pub use prompt::{GateDecision, Request, TaskContext, build_prompt, parse_answers, parse_gate_decision};

/// How decisions are executed.
#[derive(Clone)]
pub enum Dispatch {
    /// Await the decision inside the event handler (deterministic; used by tests).
    Inline,
    /// Decide in the background, at most `n` at a time.
    Spawn(Arc<Semaphore>),
}

/// What happened to one request.
#[derive(Debug, Clone, PartialEq)]
pub enum Handled {
    /// `approve`, `reject` or `answered` was recorded on the task.
    Decided { decision: String, cost_usd: f64 },
    /// No decision; the task waits for a human.
    Failed { message: String, cost_usd: f64 },
    /// Not an autopilot request of this factory, no longer pending, or already claimed.
    Skipped,
}

pub struct AutopilotService<R> {
    inner: Arc<Inner<R>>,
    dispatch: Dispatch,
    name: String,
}

struct Inner<R> {
    store: EventStore,
    factory: Arc<Factory>,
    runtime: R,
    /// Transcripts go to `<data_dir>/transcripts/autopilot-<id>.jsonl`.
    data_dir: PathBuf,
}

/// The request the task is waiting on.
struct Pending {
    request: Request,
    /// Stream version of its `GateRequested` / `AgentAsked`.
    version: i64,
    event_id: uuid::Uuid,
}

impl<R: AgentRuntime + 'static> AutopilotService<R> {
    pub fn new(store: EventStore, factory: Factory, runtime: R, data_dir: PathBuf, dispatch: Dispatch) -> Self {
        let name = format!("autopilot:{}", factory.name);
        Self { inner: Arc::new(Inner { store, factory: Arc::new(factory), runtime, data_dir }), dispatch, name }
    }

    /// Consumer name (checkpoint key).
    pub fn consumer_name(&self) -> &str {
        &self.name
    }

    /// On the very first start, skip the history: [`Self::recover`] finds the
    /// requests that are still pending, so replaying old events is pointless.
    pub async fn start_from_head_if_new(&self) -> anyhow::Result<()> {
        let store = &self.inner.store;
        if store.load_checkpoint(&self.name).await? == 0 {
            store.save_checkpoint(&self.name, store.head_seq().await?).await?;
        }
        Ok(())
    }

    /// Call once at startup, before subscribing: finish decisions the previous
    /// process left half-way (it died after claiming), then decide every pending
    /// request of an autopilot task that has no claim yet (e.g. events it never
    /// handled). Returns what was handled, per task.
    pub async fn recover(&self) -> anyhow::Result<Vec<(TaskId, Handled)>> {
        let inner = &self.inner;
        let mut out = Vec::new();
        for (stream, version, head) in inner.store.stream_heads(&format!("{}-", AutopilotId::STREAM_PREFIX)).await? {
            if head != "AutopilotStarted" || version != 1 {
                continue;
            }
            let Some(id) = AutopilotId::from_stream_id(&stream) else { continue };
            let Some(first) = inner.store.read_stream(&stream).await?.into_iter().next() else { continue };
            let Ok(AutopilotEvent::AutopilotStarted { task_id, request_version, .. }) = first.decode() else { continue };
            let handled = inner.consider(task_id, request_version, true).await?;
            if handled == Handled::Skipped {
                // Not ours, or the task moved on: close the claim.
                let ev = AutopilotEvent::AutopilotFailed {
                    message: "interrupted: the autopilot stopped while deciding".into(),
                    cost_usd: 0.0,
                    duration_ms: 0,
                    transcript: None,
                };
                let meta = inner.meta(task_id, Some(first.event_id));
                let _ = inner.store.append(&id.stream_id(), ExpectedVersion::Exact(1), vec![NewEvent::from_domain(&ev, meta)?]).await;
            }
            out.push((task_id, handled));
        }
        const WAITING_HEADS: [&str; 6] =
            ["GateRequested", "AgentAsked", "PrReviewed", "PrChecksCompleted", "PrConflicted", "TaskMoved"];
        for (stream, _, head) in inner.store.stream_heads(&format!("{}-", TaskId::STREAM_PREFIX)).await? {
            if !WAITING_HEADS.contains(&head.as_str()) {
                continue;
            }
            let Some(task) = TaskId::from_stream_id(&stream) else { continue };
            let (state, pending) = inner.load(task).await?;
            if let Some(p) = pending.filter(|_| inner.owns(&state)) {
                let handled = inner.consider(task, p.version, false).await?;
                if handled != Handled::Skipped {
                    out.push((task, handled));
                }
            }
        }
        Ok(out)
    }

    /// Decide what `task` is waiting on now (inline; tests and tools).
    pub async fn decide_now(&self, task: TaskId) -> anyhow::Result<Handled> {
        let (_, pending) = self.inner.load(task).await?;
        match pending {
            Some(p) => self.inner.consider(task, p.version, false).await,
            None => Ok(Handled::Skipped),
        }
    }
}

impl<R: AgentRuntime + 'static> Inner<R> {
    fn meta(&self, task: TaskId, cause: Option<uuid::Uuid>) -> EventMeta {
        let mut meta = EventMeta::new(task.0, Actor::System { component: AUTOPILOT_REVIEWER.into() });
        meta.causation_id = cause;
        meta.factory_version = Some(self.factory.version.clone());
        meta
    }

    fn owns(&self, state: &TaskState) -> bool {
        state.autopilot && state.benchmark.is_none() && state.factory.as_deref() == Some(self.factory.name.as_str())
    }

    /// The task's state and the request it is waiting on, if any.
    async fn load(&self, task: TaskId) -> anyhow::Result<(TaskState, Option<Pending>)> {
        let events = self.store.read_stream(&task.stream_id()).await?;
        let mut state = TaskState::default();
        let mut last_request = None;
        for e in &events {
            let ev = e.decode::<TaskEvent>()?;
            if matches!(ev, TaskEvent::GateRequested { .. } | TaskEvent::AgentAsked { .. }) {
                last_request = Some((e.stream_version, e.event_id));
            }
            state.apply(e.stream_version, ev);
        }
        let request = match (&state.status, &state.pending_questions) {
            (TaskStatus::AwaitingGate { gate, stage }, _) if !state.gate_decided => Some(Request::Gate {
                gate: gate.clone(),
                stage: stage.clone(),
                conflict_base: state.conflict.as_ref().filter(|_| stage == CONFLICT_STAGE).map(|c| c.base.clone()),
            }),
            (TaskStatus::AwaitingInput { .. }, Some(p)) => {
                Some(Request::Questions { stage: p.stage.clone(), questions: p.questions.clone() })
            }
            _ => None,
        };
        let pending = match (request, last_request) {
            (Some(request), Some((version, event_id))) => Some(Pending { request, version, event_id }),
            _ => None,
        };
        Ok((state, pending))
    }

    /// Decide the request at `request_version` of `task` if it is still pending.
    /// With `take_over`, a claim with no result (left by a dead process) is reused.
    async fn consider(&self, task: TaskId, request_version: i64, take_over: bool) -> anyhow::Result<Handled> {
        let (state, pending) = self.load(task).await?;
        let Some(pending) = pending.filter(|p| p.version == request_version && self.owns(&state)) else {
            return Ok(Handled::Skipped);
        };
        let id = AutopilotId::for_request(task, request_version);
        let stream = id.stream_id();
        let (kind, stage, gate) = match &pending.request {
            Request::Gate { gate, stage, .. } => (AutopilotKind::Gate, stage.clone(), Some(gate.clone())),
            Request::Questions { stage, .. } => (AutopilotKind::Questions, stage.clone(), None),
        };
        let started = AutopilotEvent::AutopilotStarted {
            task_id: task,
            kind,
            stage,
            gate,
            request_version,
            model: Some(self.factory.autopilot.model.clone()),
        };
        let claim = self
            .store
            .append(&stream, ExpectedVersion::NoStream, vec![NewEvent::from_domain(&started, self.meta(task, Some(pending.event_id)))?])
            .await;
        let started_id = match claim {
            Ok(r) => r.last().map(|r| r.event_id),
            Err(StoreError::Conflict { .. }) if take_over => {
                let events = self.store.read_stream(&stream).await?;
                if events.len() != 1 {
                    return Ok(Handled::Skipped);
                }
                Some(events[0].event_id)
            }
            Err(StoreError::Conflict { .. }) => return Ok(Handled::Skipped),
            Err(e) => return Err(e.into()),
        };

        let t0 = Instant::now();
        let (result, handled) = match self.run(task, &state, &pending, id).await {
            Ok(r) => r,
            Err(e) => {
                let message = format!("{e:#}");
                let duration_ms = t0.elapsed().as_millis() as u64;
                let ev = AutopilotEvent::AutopilotFailed { message: message.clone(), cost_usd: 0.0, duration_ms, transcript: None };
                (ev, Handled::Failed { message, cost_usd: 0.0 })
            }
        };
        match &handled {
            Handled::Decided { decision, .. } => tracing::info!(%task, decision, "autopilot decided"),
            Handled::Failed { message, .. } => tracing::warn!(%task, "autopilot could not decide, leaving it to a human: {message}"),
            Handled::Skipped => {}
        }
        self.store.append(&stream, ExpectedVersion::Exact(1), vec![NewEvent::from_domain(&result, self.meta(task, started_id))?]).await?;
        Ok(handled)
    }

    /// Run the model and record its decision on the task. `Err` only for
    /// WorkHub-side problems before the model runs.
    async fn run(
        &self,
        task: TaskId,
        state: &TaskState,
        pending: &Pending,
        id: AutopilotId,
    ) -> anyhow::Result<(AutopilotEvent, Handled)> {
        let t0 = Instant::now();
        let ctx = self.context(task, state).await;
        let workdir = self.data_dir.join("autopilot-work").join(id.to_string());
        let transcripts = self.data_dir.join("transcripts");
        tokio::fs::create_dir_all(&workdir).await.context("create autopilot workdir")?;
        tokio::fs::create_dir_all(&transcripts).await.context("create transcripts dir")?;
        let transcript_path = transcripts.join(format!("autopilot-{id}.jsonl"));
        let ap = &self.factory.autopilot;
        let req = AgentRequest {
            prompt: build_prompt(&ctx, &pending.request),
            workdir: workdir.clone(),
            transcript_path: transcript_path.clone(),
            model: Some(ap.model.clone()),
            budget_usd: Some(ap.budget_usd),
            permission_mode: None,
            allowed_tools: Vec::new(),
            with_user_mcp: false,
            timeout: ap.timeout,
            tools: Some(Vec::new()),
            container: None,
            env: Default::default(),
            session: SessionMode::Ephemeral,
        };
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let o = self.runtime.run(&req, tx).await;
        let _ = tokio::fs::remove_dir_all(&workdir).await;
        let transcript = transcript_path.exists().then(|| transcript_path.display().to_string());
        let cost_usd = o.cost_usd;
        let failed = |message: String| {
            let duration_ms = t0.elapsed().as_millis() as u64;
            let ev = AutopilotEvent::AutopilotFailed { message: message.clone(), cost_usd, duration_ms, transcript: transcript.clone() };
            Ok((ev, Handled::Failed { message, cost_usd }))
        };
        let text = match &o.status {
            OutcomeStatus::Failed { class, message, .. } => return failed(format!("autopilot run failed [{}]: {message}", class.as_str())),
            OutcomeStatus::Succeeded => o.summary.clone().unwrap_or_default(),
        };
        let (ev, decision, detail, rationale) = match &pending.request {
            Request::Gate { gate, stage, .. } => match parse_gate_decision(&text) {
                Ok(d) => {
                    let ev = TaskEvent::GateDecided {
                        gate: gate.clone(),
                        stage: stage.clone(),
                        approved: d.approved,
                        reviewer: AUTOPILOT_REVIEWER.into(),
                        comment: d.comment.clone(),
                    };
                    let decision = if d.approved { "approve" } else { "reject" };
                    (ev, decision, d.comment.unwrap_or_default(), d.rationale)
                }
                Err(e) => return failed(e),
            },
            Request::Questions { stage, questions } => match parse_answers(&text, questions) {
                Ok((answers, rationale)) => {
                    let detail = wh_core::format_answers(questions, &answers);
                    let ev = TaskEvent::UserAnswered { stage: stage.clone(), answers, user: AUTOPILOT_REVIEWER.into() };
                    (ev, "answered", detail, rationale)
                }
                Err(e) => return failed(e),
            },
        };
        if let Err(e) = self.record(task, pending, ev).await? {
            return failed(e);
        }
        let ev = AutopilotEvent::AutopilotDecided {
            decision: decision.into(),
            detail,
            rationale,
            cost_usd,
            usage: o.usage,
            duration_ms: t0.elapsed().as_millis() as u64,
            transcript,
        };
        Ok((ev, Handled::Decided { decision: decision.into(), cost_usd }))
    }

    /// Append the decision at the task's current version while the same request
    /// is still pending (PR polls may land meanwhile). `Ok(Err(why))` = not recorded.
    async fn record(&self, task: TaskId, pending: &Pending, ev: TaskEvent) -> anyhow::Result<Result<(), String>> {
        for _ in 0..5 {
            let (state, now) = self.load(task).await?;
            if !now.is_some_and(|p| p.version == pending.version) {
                return Ok(Err("the task moved on before the decision was recorded".into()));
            }
            let new = NewEvent::from_domain(&ev, self.meta(task, Some(pending.event_id)))?;
            match self.store.append(&task.stream_id(), ExpectedVersion::Exact(state.version), vec![new]).await {
                Ok(_) => return Ok(Ok(())),
                Err(StoreError::Conflict { .. }) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(Err("the task kept changing; the decision was not recorded".into()))
    }

    async fn context(&self, task: TaskId, state: &TaskState) -> TaskContext {
        let f = &self.factory;
        TaskContext {
            title: state.title.clone(),
            body: state.body.clone(),
            stages: f.stages.iter().map(|s| s.id.clone()).collect(),
            results: state.results.iter().map(|r| (r.stage.clone(), r.outcome.as_str().to_owned(), r.summary.clone())).collect(),
            rework_loops: state.rework_loops.max(state.pr_rework_loops),
            max_rework_loops: f.max_rework_loops,
            diff: branch_diff(&f.repo, &f.base_ref, &task_branch(task)).await,
            dialogue: state.dialogue.iter().map(|x| (x.questions.clone(), x.answers.clone(), x.user.clone())).collect(),
            guidelines: f.autopilot.guidelines.clone(),
        }
    }
}

/// Stat + patch of `branch` against its merge base with `base` (None when the
/// branch does not exist yet).
async fn branch_diff(repo: &std::path::Path, base: &str, branch: &str) -> Option<String> {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["diff", "--stat", "--patch", &format!("{base}...{branch}"), "--"])
        .output()
        .await
        .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

impl<R: AgentRuntime + 'static> EventHandler for AutopilotService<R> {
    fn name(&self) -> &str {
        &self.name
    }

    async fn handle(&mut self, ev: &RecordedEvent) -> anyhow::Result<()> {
        if !matches!(ev.event_type.as_str(), "GateRequested" | "AgentAsked") {
            return Ok(());
        }
        let Some(task) = TaskId::from_stream_id(&ev.stream_id) else { return Ok(()) };
        let (inner, version) = (self.inner.clone(), ev.stream_version);
        let run = async move {
            if let Err(e) = inner.consider(task, version, false).await {
                tracing::error!(%task, "autopilot failed: {e:#}");
            }
        };
        match &self.dispatch {
            Dispatch::Inline => run.await,
            Dispatch::Spawn(limit) => {
                let limit = limit.clone();
                tokio::spawn(async move {
                    let _permit = limit.acquire_owned().await;
                    run.await
                });
            }
        }
        Ok(())
    }
}
