use wh_core::task::{StageOutcome, TaskEvent, TaskState, TaskStatus};
use wh_factory::Factory;

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    RunStage { stage: String, feedback: Option<String> },
    RequestGate { gate: String, stage: String },
    Complete,
    Block { stage: Option<String>, reason: String },
}

/// Pure pipeline policy: given a task's state (and the event that produced it),
/// what should happen next? `None` = wait (a stage is running, a gate is open, or
/// the task is finished).
pub fn decide(state: &TaskState, f: &Factory) -> Option<Action> {
    if matches!(state.status, TaskStatus::Done | TaskStatus::Cancelled) {
        return None;
    }
    match state.last.as_ref()? {
        TaskEvent::TaskCreated { .. } => Some(run(&f.first_stage().id, None)),
        TaskEvent::StageExited { stage, outcome, summary, .. } => {
            let Some(st) = f.stage(stage) else {
                return Some(block(Some(stage), format!("stage '{stage}' is not in factory '{}'", f.name)));
            };
            Some(match outcome {
                StageOutcome::Succeeded => match &st.gate {
                    Some(gate) => Action::RequestGate { gate: gate.clone(), stage: stage.clone() },
                    None => advance(f, stage),
                },
                StageOutcome::Skipped => advance(f, stage),
                StageOutcome::Rejected => match &st.on_reject {
                    Some(target) => rework(state, f, target, summary.clone().unwrap_or_default()),
                    None => block(Some(stage), "stage rejected but has no on_reject target".into()),
                },
                StageOutcome::Failed => {
                    block(Some(stage), summary.clone().unwrap_or_else(|| format!("stage '{stage}' failed")))
                }
            })
        }
        TaskEvent::GateDecided { stage, approved, comment, .. } => Some(if *approved {
            advance(f, stage)
        } else {
            rework(state, f, stage, comment.clone().unwrap_or_else(|| "Rejected at gate.".into()))
        }),
        TaskEvent::TaskResumed { stage, comment } => {
            let target = stage.clone().or_else(|| blocked_stage(state)).or_else(|| state.last_stage().map(str::to_owned));
            let target = target.as_deref().unwrap_or(&f.first_stage().id);
            Some(match f.stage(target) {
                Some(_) => run(target, comment.clone()),
                None => block(None, format!("cannot resume: unknown stage '{target}'")),
            })
        }
        TaskEvent::StageEntered { .. }
        | TaskEvent::GateRequested { .. }
        | TaskEvent::TaskBlocked { .. }
        | TaskEvent::TaskCompleted { .. }
        | TaskEvent::TaskCancelled { .. } => None,
    }
}

fn run(stage: &str, feedback: Option<String>) -> Action {
    Action::RunStage { stage: stage.to_owned(), feedback }
}

fn block(stage: Option<&String>, reason: String) -> Action {
    Action::Block { stage: stage.cloned(), reason }
}

fn advance(f: &Factory, stage: &str) -> Action {
    match f.next_stage(stage) {
        Some(next) => run(&next.id, None),
        None => Action::Complete,
    }
}

fn rework(state: &TaskState, f: &Factory, target: &str, feedback: String) -> Action {
    if state.rework_loops >= f.max_rework_loops {
        return Action::Block {
            stage: Some(target.to_owned()),
            reason: format!("rework limit reached ({} loops)", f.max_rework_loops),
        };
    }
    run(target, Some(feedback))
}

/// Stage recorded on the most recent `TaskBlocked`, if any.
fn blocked_stage(state: &TaskState) -> Option<String> {
    state.blocked_stage.clone()
}
