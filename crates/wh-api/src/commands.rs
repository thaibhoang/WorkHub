//! Human commands (create, gate decision, resume, cancel). Each one validates
//! against the task's folded state and appends exactly one event with
//! `ExpectedVersion::Exact`, so a decision made on stale state is rejected
//! instead of silently overriding a newer one. Shared by the CLI and the API.

use wh_core::{
    Actor, EventMeta, NewEvent, TaskId,
    task::{TaskEvent, TaskSource, TaskState, TaskStatus},
};
use wh_eventstore::{EventStore, ExpectedVersion, StoreError};

#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    #[error("task {0} not found")]
    NotFound(TaskId),
    #[error("{0}")]
    InvalidState(String),
    #[error("the task changed while you were deciding; reload and try again")]
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

/// Fold a task's stream into its aggregate state.
pub async fn load_task(store: &EventStore, task: TaskId) -> Result<TaskState> {
    let events = store.read_stream(&task.stream_id()).await?;
    if events.is_empty() {
        return Err(CommandError::NotFound(task));
    }
    let mut state = TaskState::default();
    for e in &events {
        state.apply(e.stream_version, e.decode().map_err(anyhow::Error::from)?);
    }
    Ok(state)
}

pub struct NewTask {
    pub title: String,
    pub body: Option<String>,
    pub source: TaskSource,
    /// Factory name that should drive the task (None = manual only).
    pub factory: Option<String>,
}

pub async fn create_task(store: &EventStore, actor: Actor, t: NewTask) -> Result<TaskId> {
    let title = t.title.trim().to_owned();
    if title.is_empty() {
        return Err(CommandError::InvalidState("title must not be empty".into()));
    }
    let body = t.body.filter(|b| !b.trim().is_empty());
    let id = TaskId::new();
    let ev = TaskEvent::TaskCreated { title, body, source: t.source, factory: t.factory };
    append(store, id, ExpectedVersion::NoStream, actor, ev).await?;
    Ok(id)
}

/// Approve or reject the gate the task is waiting at. Returns the gate name.
pub async fn decide_gate(
    store: &EventStore,
    actor: Actor,
    task: TaskId,
    approved: bool,
    comment: Option<String>,
) -> Result<String> {
    let comment = comment.filter(|c| !c.trim().is_empty());
    if !approved && comment.is_none() {
        return Err(CommandError::InvalidState("rejecting a gate needs a comment for the agent".into()));
    }
    let state = load_task(store, task).await?;
    let TaskStatus::AwaitingGate { gate, stage } = state.status.clone() else {
        return Err(CommandError::InvalidState(format!("task is not waiting at a gate ({})", status_name(&state.status))));
    };
    let reviewer = actor_name(&actor);
    let ev = TaskEvent::GateDecided { gate: gate.clone(), stage, approved, reviewer, comment };
    append(store, task, ExpectedVersion::Exact(state.version), actor, ev).await?;
    Ok(gate)
}

/// Restart a blocked task, optionally at another stage, with guidance for the agent.
pub async fn resume(
    store: &EventStore,
    actor: Actor,
    task: TaskId,
    stage: Option<String>,
    comment: Option<String>,
) -> Result<()> {
    let state = load_task(store, task).await?;
    if !matches!(state.status, TaskStatus::Blocked { .. }) {
        return Err(CommandError::InvalidState(format!(
            "only blocked tasks can be resumed (task is {})",
            status_name(&state.status)
        )));
    }
    let stage = stage.filter(|s| !s.trim().is_empty());
    let comment = comment.filter(|c| !c.trim().is_empty());
    let ev = TaskEvent::TaskResumed { stage, comment };
    append(store, task, ExpectedVersion::Exact(state.version), actor, ev).await
}

pub async fn cancel(store: &EventStore, actor: Actor, task: TaskId, reason: String) -> Result<()> {
    let state = load_task(store, task).await?;
    if matches!(state.status, TaskStatus::Done | TaskStatus::Cancelled) {
        return Err(CommandError::InvalidState(format!("task is already {}", status_name(&state.status))));
    }
    let reason = if reason.trim().is_empty() { "cancelled".to_owned() } else { reason };
    append(store, task, ExpectedVersion::Exact(state.version), actor, TaskEvent::TaskCancelled { reason }).await
}

async fn append(store: &EventStore, task: TaskId, expected: ExpectedVersion, actor: Actor, ev: TaskEvent) -> Result<()> {
    let meta = EventMeta::new(task.0, actor);
    let new = NewEvent::from_domain(&ev, meta).map_err(anyhow::Error::from)?;
    store.append(&task.stream_id(), expected, vec![new]).await?;
    // Keep read models fresh for whoever issued the command.
    wh_metrics::catch_up_all(store).await?;
    Ok(())
}

pub fn status_name(s: &TaskStatus) -> &'static str {
    match s {
        TaskStatus::Open => "open",
        TaskStatus::Running { .. } => "running",
        TaskStatus::AwaitingGate { .. } => "awaiting_gate",
        TaskStatus::Blocked { .. } => "blocked",
        TaskStatus::Done => "done",
        TaskStatus::Cancelled => "cancelled",
    }
}

fn actor_name(a: &Actor) -> String {
    match a {
        Actor::Human { user } => user.clone(),
        Actor::Agent { session } => format!("agent:{session}"),
        Actor::System { component } => component.clone(),
    }
}

/// The local OS user (WorkHub is local-only; there is no login).
pub fn local_user() -> Actor {
    Actor::Human { user: std::env::var("USER").unwrap_or_else(|_| "unknown".into()) }
}
