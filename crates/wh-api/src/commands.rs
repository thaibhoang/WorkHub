//! Human commands (create, gate decision, answer, resume, cancel). Each one validates
//! against the task's folded state and appends exactly one event with
//! `ExpectedVersion::Exact`, so a decision made on stale state is rejected
//! instead of silently overriding a newer one. Shared by the CLI and the API.

use wh_core::{
    Actor, Answer, EventMeta, NewEvent, SessionId, TaskId,
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
    /// Parent task: the new task becomes its subtask.
    pub parent: Option<TaskId>,
    /// Start right away instead of creating a draft that waits for `activate`.
    pub start: bool,
    /// Start in autopilot mode (needs `start`; drafts choose it in `activate`).
    pub autopilot: bool,
}

pub async fn create_task(store: &EventStore, actor: Actor, t: NewTask) -> Result<TaskId> {
    let title = t.title.trim().to_owned();
    if title.is_empty() {
        return Err(CommandError::InvalidState("title must not be empty".into()));
    }
    let body = t.body.filter(|b| !b.trim().is_empty());
    if t.autopilot && !t.start {
        return Err(CommandError::InvalidState("autopilot is chosen when the task starts: start it now, or pick it when starting the draft".into()));
    }
    if t.autopilot && t.factory.is_none() {
        return Err(CommandError::InvalidState("autopilot needs a factory pipeline".into()));
    }
    if let Some(p) = t.parent {
        let parent = load_task(store, p).await.map_err(|e| match e {
            CommandError::NotFound(_) => CommandError::InvalidState(format!("parent task {p} not found")),
            e => e,
        })?;
        if parent.benchmark.is_some() {
            return Err(CommandError::InvalidState("a benchmark case cannot have subtasks".into()));
        }
    }
    // A brand-new id has no descendants, so creating under a parent can never close a cycle.
    let id = TaskId::new();
    let ev = TaskEvent::TaskCreated {
        title,
        body,
        source: t.source,
        factory: t.factory,
        benchmark: None,
        external_ref: None,
        parent: t.parent,
        draft: !t.start,
        autopilot: t.autopilot,
    };
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
    // The status stays at the gate until the foreman acts, so a repeat would pass `Exact`.
    if state.gate_decided {
        return Err(CommandError::Conflict);
    }
    let reviewer = actor_name(&actor);
    let ev = TaskEvent::GateDecided { gate: gate.clone(), stage, approved, reviewer, comment };
    append(store, task, ExpectedVersion::Exact(state.version), actor, ev).await?;
    Ok(gate)
}

/// Answer the questions the task is waiting on: `answers[i]` answers question `i`.
/// `asked` names the session whose questions are being answered, if the caller
/// knows it, so newer questions are never answered by mistake.
pub async fn answer(
    store: &EventStore,
    actor: Actor,
    task: TaskId,
    asked: Option<SessionId>,
    answers: Vec<Answer>,
) -> Result<()> {
    let state = load_task(store, task).await?;
    let (TaskStatus::AwaitingInput { .. }, Some(pending)) = (&state.status, &state.pending_questions) else {
        return Err(CommandError::InvalidState(format!(
            "task is not waiting for an answer ({})",
            status_name(&state.status)
        )));
    };
    if asked.is_some_and(|s| s != pending.session) {
        return Err(CommandError::Conflict);
    }
    let answers = wh_core::check_answers(&pending.questions, answers).map_err(CommandError::InvalidState)?;
    let ev = TaskEvent::UserAnswered { stage: pending.stage.clone(), answers, user: actor_name(&actor) };
    append(store, task, ExpectedVersion::Exact(state.version), actor, ev).await
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

/// Start a draft task (the foreman then runs its first stage). With `subtree`,
/// also every draft below it, parents first. With `autopilot`, the autopilot agent
/// decides their gates and answers their agents' questions instead of a human.
/// Returns the tasks started.
pub async fn activate(store: &EventStore, actor: Actor, task: TaskId, subtree: bool, autopilot: bool) -> Result<Vec<TaskId>> {
    let state = load_task(store, task).await?;
    if autopilot && state.factory.is_none() && !subtree {
        return Err(CommandError::InvalidState("autopilot needs a factory pipeline".into()));
    }
    let mut started = Vec::new();
    match state.status {
        TaskStatus::Draft => {
            let ev = TaskEvent::TaskActivated { autopilot: autopilot && state.factory.is_some() };
            append(store, task, ExpectedVersion::Exact(state.version), actor.clone(), ev).await?;
            started.push(task);
        }
        _ if subtree => {}
        _ => {
            return Err(CommandError::InvalidState(format!(
                "only draft tasks can be started (task is {})",
                status_name(&state.status)
            )));
        }
    }
    if subtree {
        wh_metrics::catch_up_all(store).await?;
        for id in wh_metrics::queries::descendants(store.pool(), task.0).await?.into_iter().map(TaskId) {
            let s = load_task(store, id).await?;
            if s.status == TaskStatus::Draft {
                let ev = TaskEvent::TaskActivated { autopilot: autopilot && s.factory.is_some() };
                append(store, id, ExpectedVersion::Exact(s.version), actor.clone(), ev).await?;
                started.push(id);
            }
        }
    }
    Ok(started)
}

/// Advisory lock key serializing tree moves (so two concurrent moves cannot
/// together close a cycle that each check alone would allow).
const TREE_LOCK: i64 = 0x7768_7472_6565; // "whtree"

/// Move a task under `parent` (None = top level). Refuses to make a task its own
/// ancestor, so tasks always form a tree.
pub async fn move_task(store: &EventStore, actor: Actor, task: TaskId, parent: Option<TaskId>) -> Result<()> {
    let mut conn = store.pool().acquire().await.map_err(anyhow::Error::from)?;
    sqlx::query("SELECT pg_advisory_lock($1)").bind(TREE_LOCK).execute(&mut *conn).await.map_err(anyhow::Error::from)?;
    let res = move_locked(store, actor, task, parent).await;
    let unlock = sqlx::query("SELECT pg_advisory_unlock($1)").bind(TREE_LOCK).execute(&mut *conn).await;
    if unlock.is_err() {
        // Closing the connection releases the lock.
        let _ = conn.detach();
    }
    res
}

async fn move_locked(store: &EventStore, actor: Actor, task: TaskId, parent: Option<TaskId>) -> Result<()> {
    let state = load_task(store, task).await?;
    if state.benchmark.is_some() {
        return Err(CommandError::InvalidState("benchmark cases cannot be moved".into()));
    }
    if state.parent == parent {
        return Ok(());
    }
    if let Some(p) = parent {
        // Walk up from the new parent on the event streams (not the read model): reaching
        // `task` means the move would make it its own ancestor.
        let mut cur = Some(p);
        while let Some(c) = cur {
            if c == task {
                return Err(CommandError::InvalidState(if c == p {
                    "a task cannot be its own parent".into()
                } else {
                    format!("cannot move the task under {p}: that task is below it, and tasks must form a tree")
                }));
            }
            let s = load_task(store, c).await.map_err(|e| match e {
                CommandError::NotFound(_) => CommandError::InvalidState(format!("parent task {c} not found")),
                e => e,
            })?;
            if c == p && s.benchmark.is_some() {
                return Err(CommandError::InvalidState("a benchmark case cannot have subtasks".into()));
            }
            cur = s.parent;
        }
    }
    append(store, task, ExpectedVersion::Exact(state.version), actor, TaskEvent::TaskMoved { parent }).await
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
        TaskStatus::Draft => "draft",
        TaskStatus::Open => "open",
        TaskStatus::Running { .. } => "running",
        TaskStatus::AwaitingGate { .. } => "awaiting_gate",
        TaskStatus::AwaitingInput { .. } => "awaiting_input",
        TaskStatus::Blocked { .. } => "blocked",
        TaskStatus::Shipped => "shipped",
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
