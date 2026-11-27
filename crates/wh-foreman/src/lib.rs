//! The Foreman: a process manager that drives tasks through the factory pipeline.
//!
//! It consumes task events, folds the task's state, asks the pure [`decide`]
//! function what to do next, and acts (dispatch a stage to the runner, request a
//! human gate, complete or block the task). Every action is appended with
//! `ExpectedVersion::Exact(state.version)`, so redelivered events never cause a
//! second dispatch.

pub mod cleanup;
mod decide;
mod prompt;

use std::sync::Arc;

use anyhow::Context;

use tokio::sync::{Notify, Semaphore};
use wh_core::{
    Actor, BenchmarkRunId, EventMeta, FailureClass, NewEvent, RecordedEvent, Sandbox, SessionId, TaskId,
    session::SessionEvent,
    task::{TaskEvent, TaskState},
};
use wh_eventstore::{EventHandler, EventStore, ExpectedVersion, StoreError};
use wh_factory::Factory;
use wh_runner::{AgentRuntime, Merge, RunSpec, Runner};

pub use decide::{Action, decide};
pub use prompt::build_prompt;

/// How stage runs are executed.
#[derive(Clone)]
pub enum Dispatch {
    /// Await the run inside the event handler (deterministic; used by tests and `--inline`).
    Inline,
    /// Spawn runs in the background, at most `n` at a time.
    Spawn(Arc<Semaphore>),
}

/// Which tasks a foreman drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Regular tasks of the factory (never benchmark cases).
    Live,
    /// Only the cases of one benchmark run. Human gates are approved automatically.
    Benchmark(BenchmarkRunId),
}

pub struct Foreman<R> {
    store: EventStore,
    factory: Arc<Factory>,
    runner: Arc<Runner<R>>,
    dispatch: Dispatch,
    scope: Scope,
    name: String,
    gh: wh_github::Gh,
    /// Woken when a task finishes, so [`cleanup::run_janitor`] sweeps right away.
    janitor: Option<Arc<Notify>>,
}

/// Reviewer recorded on gates a benchmark foreman approves.
pub const BENCHMARK_REVIEWER: &str = "benchmark";

impl<R: AgentRuntime + 'static> Foreman<R> {
    pub fn new(store: EventStore, factory: Factory, runner: Runner<R>, dispatch: Dispatch) -> Self {
        let name = format!("foreman:{}", factory.name);
        Self {
            store,
            factory: Arc::new(factory),
            runner: Arc::new(runner),
            dispatch,
            scope: Scope::Live,
            name,
            gh: wh_github::Gh::default(),
            janitor: None,
        }
    }

    /// Notify `wake` whenever one of this foreman's tasks is done or cancelled.
    pub fn with_janitor(mut self, wake: Arc<Notify>) -> Self {
        self.janitor = Some(wake);
        self
    }

    /// Use another `gh` binary for shipping (tests use a fake one).
    pub fn with_gh(mut self, gh: wh_github::Gh) -> Self {
        self.gh = gh;
        self
    }

    /// A foreman for the cases of one benchmark run, driven with [`Self::drive`].
    pub fn for_benchmark(store: EventStore, factory: Factory, runner: Runner<R>, run: BenchmarkRunId) -> Self {
        let name = format!("foreman:{}:bench-{run}", factory.name);
        Self {
            store,
            factory: Arc::new(factory),
            runner: Arc::new(runner),
            dispatch: Dispatch::Inline,
            scope: Scope::Benchmark(run),
            name,
            gh: wh_github::Gh::default(),
            janitor: None,
        }
    }

    /// Whether this foreman drives `state`'s task.
    fn owns(&self, state: &TaskState) -> bool {
        if state.factory.as_deref() != Some(self.factory.name.as_str()) {
            return false;
        }
        match self.scope {
            Scope::Live => state.benchmark.is_none(),
            Scope::Benchmark(run) => state.benchmark.as_ref().is_some_and(|b| b.run == run),
        }
    }

    /// Drive one task inline until it needs nothing more from the foreman
    /// (done, blocked, cancelled, or waiting on a human gate in live scope).
    /// Requires `Dispatch::Inline`.
    pub async fn drive(&mut self, task: TaskId) -> anyhow::Result<TaskState> {
        anyhow::ensure!(matches!(self.dispatch, Dispatch::Inline), "drive requires Dispatch::Inline");
        loop {
            let events = self.store.read_stream(&task.stream_id()).await?;
            let head = events.last().with_context(|| format!("task {task} has no events"))?;
            let before = head.stream_version;
            self.handle(head).await?;
            let state = self.load(task).await?;
            if state.version == before {
                return Ok(state);
            }
        }
    }

    pub fn task_branch(task: TaskId) -> String {
        wh_core::task::task_branch(task)
    }

    /// Call once at startup, before subscribing. A stage that was running when the
    /// previous foreman process died has no runner any more: mark its orphaned
    /// sessions failed and block the task so a human can `wh task resume` it.
    /// Returns the tasks that were blocked.
    pub async fn recover_interrupted(&self) -> anyhow::Result<Vec<TaskId>> {
        const REASON: &str = "interrupted: the foreman stopped while this stage was running";
        let version = Some(self.factory.version.clone());
        recover_interrupted(&self.store, REASON, version, |s| self.owns(s)).await
    }

    fn meta(&self, task: TaskId, cause: Option<&RecordedEvent>) -> EventMeta {
        let mut meta = EventMeta::new(task.0, Actor::System { component: "foreman".into() });
        meta.causation_id = cause.map(|c| c.event_id);
        meta.factory_version = Some(self.factory.version.clone());
        meta
    }

    async fn load(&self, task: TaskId) -> anyhow::Result<TaskState> {
        let events = self.store.read_stream(&task.stream_id()).await?;
        let mut state = TaskState::default();
        for e in &events {
            state.apply(e.stream_version, e.decode::<TaskEvent>()?);
        }
        Ok(state)
    }

    async fn append(&self, task: TaskId, state: &TaskState, cause: &RecordedEvent, ev: TaskEvent) -> anyhow::Result<()> {
        let meta = self.meta(task, Some(cause));
        match self
            .store
            .append(&task.stream_id(), ExpectedVersion::Exact(state.version), vec![NewEvent::from_domain(&ev, meta)?])
            .await
        {
            Ok(_) => Ok(()),
            Err(StoreError::Conflict { .. }) => Ok(()), // someone acted first; the newer event will be handled
            Err(e) => Err(e.into()),
        }
    }

    fn run_spec(&self, task: TaskId, state: &TaskState, stage_id: &str, feedback: Option<String>) -> RunSpec {
        let f = &self.factory;
        let stage = f.stage(stage_id).expect("decide only returns known stages");
        let agent = f.agent(stage);
        let bench = state.benchmark.as_ref();
        RunSpec {
            task_id: task,
            agent: agent.name.clone(),
            stage: stage.id.clone(),
            prompt: build_prompt(agent, stage, state, feedback.as_deref()),
            repo: bench.and_then(|b| b.repo.clone()).map_or_else(|| f.repo.clone(), Into::into),
            base_ref: bench.and_then(|b| b.base_ref.clone()).unwrap_or_else(|| f.base_ref.clone()),
            model: agent.model.clone(),
            budget_usd: Some(agent.budget_usd),
            permission_mode: Some(agent.permission_mode.clone()),
            allowed_tools: agent.allowed_tools.clone(),
            tools: agent.tools.clone(),
            with_user_mcp: false,
            timeout: agent.timeout,
            keep_worktree: false,
            max_retries: agent.max_retries,
            retry_backoff: std::time::Duration::from_secs(10),
            branch: Some(Self::task_branch(task)),
            commit_on_success: true,
            verdict_required: stage.requires_verdict(),
            task_expected_version: Some(state.version),
            feedback,
            factory_version: Some(f.version.clone()),
            benchmark: bench.map(|b| b.run),
            sandbox: agent.sandbox.clone(),
            env: agent.env.clone(),
            merge: None,
        }
    }

    async fn run_stage(&self, spec: RunSpec) -> anyhow::Result<()> {
        let (task, stage) = (spec.task_id, spec.stage.clone());
        tracing::info!(%task, stage, "dispatching stage");
        let runner = self.runner.clone();
        let run = async move {
            match runner.run(spec, |_| {}).await {
                Ok(r) => {
                    tracing::info!(%task, stage, outcome = r.stage_outcome.as_str(), cost = r.total_cost_usd(), "stage finished");
                    Ok(())
                }
                Err(e) if is_conflict(&e) => Ok(()), // already dispatched by an earlier delivery
                Err(e) => Err(e),
            }
        };
        match &self.dispatch {
            Dispatch::Inline => run.await,
            Dispatch::Spawn(limit) => {
                let limit = limit.clone();
                tokio::spawn(async move {
                    let _permit = limit.acquire_owned().await;
                    if let Err(e) = run.await {
                        tracing::error!(%task, "stage run failed: {e:#}");
                    }
                });
                Ok(())
            }
        }
    }
}

/// Block every task matching `owned` whose stream ends at `StageEntered` (its runner
/// is gone) and fail its unfinished sessions with `reason`. Returns the blocked tasks.
pub async fn recover_interrupted(
    store: &EventStore,
    reason: &str,
    factory_version: Option<String>,
    owned: impl Fn(&TaskState) -> bool,
) -> anyhow::Result<Vec<TaskId>> {
    let meta = |task: TaskId| {
        let mut meta = EventMeta::new(task.0, Actor::System { component: "foreman".into() });
        meta.factory_version = factory_version.clone();
        meta
    };
    let mut blocked = Vec::new();
    for (stream, _, head) in store.stream_heads(&format!("{}-", TaskId::STREAM_PREFIX)).await? {
        let Some(task) = TaskId::from_stream_id(&stream) else { continue };
        if head != "StageEntered" {
            continue;
        }
        let mut state = TaskState::default();
        for e in store.read_stream(&stream).await? {
            state.apply(e.stream_version, e.decode::<TaskEvent>()?);
        }
        if !owned(&state) {
            continue;
        }
        let ev = TaskEvent::TaskBlocked { stage: state.last_stage_entered(), reason: reason.into() };
        let rec = store
            .append(&stream, ExpectedVersion::Exact(state.version), vec![NewEvent::from_domain(&ev, meta(task))?])
            .await;
        if rec.is_ok() {
            blocked.push(task);
        }
    }
    if blocked.is_empty() {
        return Ok(blocked);
    }
    for (stream, version, head) in store.stream_heads(&format!("{}-", SessionId::STREAM_PREFIX)).await? {
        if matches!(head.as_str(), "AgentSessionCompleted" | "AgentSessionFailed") {
            continue;
        }
        let events = store.read_stream(&stream).await?;
        let Some(SessionEvent::AgentDispatched { task_id, .. }) = events.first().and_then(|e| e.decode().ok()) else {
            continue;
        };
        if !blocked.contains(&task_id) {
            continue;
        }
        let ev = SessionEvent::AgentSessionFailed {
            failure_class: FailureClass::Infra,
            message: reason.into(),
            retryable: true,
            num_turns: 0,
            duration_ms: 0,
            transcript: None,
        };
        let _ = store.append(&stream, ExpectedVersion::Exact(version), vec![NewEvent::from_domain(&ev, meta(task_id))?]).await;
        // A container outlives the `docker run` client of a crashed process.
        let in_container = events.iter().any(|e| {
            matches!(e.decode(), Ok(SessionEvent::AgentSessionStarted { sandbox: Some(Sandbox::Docker(_)), .. }))
        });
        if let (true, Some(session)) = (in_container, SessionId::from_stream_id(&stream)) {
            wh_runner::docker::remove(std::path::Path::new("docker"), &wh_runner::docker::container_name(session)).await;
        }
    }
    Ok(blocked)
}

fn is_conflict(e: &anyhow::Error) -> bool {
    matches!(e.downcast_ref::<StoreError>(), Some(StoreError::Conflict { .. }))
}

impl<R: AgentRuntime + 'static> EventHandler for Foreman<R> {
    fn name(&self) -> &str {
        &self.name
    }

    async fn handle(&mut self, ev: &RecordedEvent) -> anyhow::Result<()> {
        let Some(task) = TaskId::from_stream_id(&ev.stream_id) else {
            return Ok(());
        };
        let state = self.load(task).await?;
        // Only the newest event of a task triggers a decision; older (redelivered)
        // events are superseded.
        if ev.stream_version != state.version || !self.owns(&state) {
            return Ok(());
        }
        if let Some(wake) = &self.janitor
            && matches!(state.status, wh_core::task::TaskStatus::Done | wh_core::task::TaskStatus::Cancelled)
        {
            wake.notify_one();
        }
        let Some(action) = decide(&state, &self.factory) else {
            return Ok(());
        };
        match action {
            Action::RunStage { stage, feedback } => self.run_stage(self.run_spec(task, &state, &stage, feedback)).await,
            Action::ResolveConflict { stage, feedback } => {
                let (Some(g), Some(c)) = (&self.factory.github, &state.conflict) else {
                    return Ok(());
                };
                let mut spec = self.run_spec(task, &state, &stage, Some(feedback));
                spec.merge = Some(Merge { remote: g.remote.clone(), base: c.base.clone() });
                self.run_stage(spec).await
            }
            Action::RequestGate { gate, stage } if matches!(self.scope, Scope::Benchmark(_)) => {
                // Benchmarks measure the agents, not human latency: approve the gate
                // right away (still recorded, so the pipeline shape is unchanged).
                let requested = TaskEvent::GateRequested { gate: gate.clone(), stage: stage.clone() };
                let decided = TaskEvent::GateDecided {
                    gate,
                    stage,
                    approved: true,
                    reviewer: BENCHMARK_REVIEWER.into(),
                    comment: None,
                };
                let events = vec![
                    NewEvent::from_domain(&requested, self.meta(task, Some(ev)))?,
                    NewEvent::from_domain(&decided, self.meta(task, Some(ev)))?,
                ];
                match self.store.append(&task.stream_id(), ExpectedVersion::Exact(state.version), events).await {
                    Ok(_) | Err(StoreError::Conflict { .. }) => Ok(()),
                    Err(e) => Err(e.into()),
                }
            }
            Action::RequestGate { gate, stage } => {
                self.append(task, &state, ev, TaskEvent::GateRequested { gate, stage }).await
            }
            Action::Complete => {
                let branch = Some(Self::task_branch(task));
                self.append(task, &state, ev, TaskEvent::TaskCompleted { branch }).await
            }
            Action::Ship => {
                // Pushing is idempotent and the PR is looked up before being created,
                // so a redelivered event at worst pushes again and finds the same PR.
                let next = match wh_github::ship(&self.gh, &self.factory, task, &state).await {
                    Ok(shipped) => {
                        if let TaskEvent::TaskShipped { pr_url, .. } = &shipped {
                            tracing::info!(%task, pr_url, "shipped");
                        }
                        shipped
                    }
                    Err(e) => {
                        tracing::warn!(%task, "ship failed: {e}");
                        TaskEvent::TaskBlocked {
                            stage: Some(wh_core::task::SHIP_STAGE.into()),
                            reason: format!("ship failed {e}"),
                        }
                    }
                };
                self.append(task, &state, ev, next).await
            }
            Action::Block { stage, reason } => {
                tracing::warn!(%task, reason, "task blocked");
                self.append(task, &state, ev, TaskEvent::TaskBlocked { stage, reason }).await
            }
        }
    }
}
