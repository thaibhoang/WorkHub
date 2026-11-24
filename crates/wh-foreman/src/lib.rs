//! The Foreman: a process manager that drives tasks through the factory pipeline.
//!
//! It consumes task events, folds the task's state, asks the pure [`decide`]
//! function what to do next, and acts (dispatch a stage to the runner, request a
//! human gate, complete or block the task). Every action is appended with
//! `ExpectedVersion::Exact(state.version)`, so redelivered events never cause a
//! second dispatch.

mod decide;
mod prompt;

use std::sync::Arc;

use tokio::sync::Semaphore;
use wh_core::{
    Actor, EventMeta, FailureClass, NewEvent, RecordedEvent, SessionId, TaskId,
    session::SessionEvent,
    task::{TaskEvent, TaskState},
};
use wh_eventstore::{EventHandler, EventStore, ExpectedVersion, StoreError};
use wh_factory::Factory;
use wh_runner::{AgentRuntime, RunSpec, Runner};

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

pub struct Foreman<R> {
    store: EventStore,
    factory: Arc<Factory>,
    runner: Arc<Runner<R>>,
    dispatch: Dispatch,
    name: String,
}

impl<R: AgentRuntime + 'static> Foreman<R> {
    pub fn new(store: EventStore, factory: Factory, runner: Runner<R>, dispatch: Dispatch) -> Self {
        let name = format!("foreman:{}", factory.name);
        Self { store, factory: Arc::new(factory), runner: Arc::new(runner), dispatch, name }
    }

    pub fn task_branch(task: TaskId) -> String {
        format!("wh/task/{}", &task.0.simple().to_string()[20..])
    }

    /// Call once at startup, before subscribing. A stage that was running when the
    /// previous foreman process died has no runner any more: mark its orphaned
    /// sessions failed and block the task so a human can `wh task resume` it.
    /// Returns the tasks that were blocked.
    pub async fn recover_interrupted(&self) -> anyhow::Result<Vec<TaskId>> {
        const REASON: &str = "interrupted: the foreman stopped while this stage was running";
        let mut blocked = Vec::new();
        for (stream, _, head) in self.store.stream_heads(&format!("{}-", TaskId::STREAM_PREFIX)).await? {
            let Some(task) = TaskId::from_stream_id(&stream) else { continue };
            if head != "StageEntered" {
                continue;
            }
            let state = self.load(task).await?;
            if state.factory.as_deref() != Some(self.factory.name.as_str()) {
                continue;
            }
            let ev = TaskEvent::TaskBlocked { stage: state.last_stage_entered(), reason: REASON.into() };
            let meta = self.meta(task, None);
            let rec = self
                .store
                .append(&stream, ExpectedVersion::Exact(state.version), vec![NewEvent::from_domain(&ev, meta)?])
                .await;
            if rec.is_ok() {
                blocked.push(task);
            }
        }
        if blocked.is_empty() {
            return Ok(blocked);
        }
        for (stream, version, head) in self.store.stream_heads(&format!("{}-", SessionId::STREAM_PREFIX)).await? {
            if matches!(head.as_str(), "AgentSessionCompleted" | "AgentSessionFailed") {
                continue;
            }
            let events = self.store.read_stream(&stream).await?;
            let Some(SessionEvent::AgentDispatched { task_id, .. }) = events.first().and_then(|e| e.decode().ok())
            else {
                continue;
            };
            if !blocked.contains(&task_id) {
                continue;
            }
            let ev = SessionEvent::AgentSessionFailed {
                failure_class: FailureClass::Infra,
                message: REASON.into(),
                retryable: true,
                num_turns: 0,
                duration_ms: 0,
                transcript: None,
            };
            let meta = self.meta(task_id, None);
            let _ = self.store.append(&stream, ExpectedVersion::Exact(version), vec![NewEvent::from_domain(&ev, meta)?]).await;
        }
        Ok(blocked)
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
        RunSpec {
            task_id: task,
            agent: agent.name.clone(),
            stage: stage.id.clone(),
            prompt: build_prompt(agent, stage, state, feedback.as_deref()),
            repo: f.repo.clone(),
            base_ref: f.base_ref.clone(),
            model: agent.model.clone(),
            budget_usd: Some(agent.budget_usd),
            permission_mode: Some(agent.permission_mode.clone()),
            allowed_tools: agent.allowed_tools.clone(),
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
        if ev.stream_version != state.version || state.factory.as_deref() != Some(self.factory.name.as_str()) {
            return Ok(());
        }
        let Some(action) = decide(&state, &self.factory) else {
            return Ok(());
        };
        match action {
            Action::RunStage { stage, feedback } => self.run_stage(self.run_spec(task, &state, &stage, feedback)).await,
            Action::RequestGate { gate, stage } => {
                self.append(task, &state, ev, TaskEvent::GateRequested { gate, stage }).await
            }
            Action::Complete => {
                let branch = Some(Self::task_branch(task));
                self.append(task, &state, ev, TaskEvent::TaskCompleted { branch }).await
            }
            Action::Block { stage, reason } => {
                tracing::warn!(%task, reason, "task blocked");
                self.append(task, &state, ev, TaskEvent::TaskBlocked { stage, reason }).await
            }
        }
    }
}
