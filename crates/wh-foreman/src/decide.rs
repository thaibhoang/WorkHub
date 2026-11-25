use wh_core::task::{SHIP_STAGE, StageOutcome, TaskEvent, TaskState, TaskStatus};
use wh_factory::Factory;

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    RunStage { stage: String, feedback: Option<String> },
    RequestGate { gate: String, stage: String },
    Complete,
    /// Push the task branch and open / update its pull request.
    Ship,
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
                    None => advance(state, f, stage),
                },
                StageOutcome::Skipped => advance(state, f, stage),
                StageOutcome::Rejected => match &st.on_reject {
                    Some(target) => rework(state, f, target, summary.clone().unwrap_or_default()),
                    None => block(Some(stage), "stage rejected but has no on_reject target".into()),
                },
                StageOutcome::Failed => {
                    block(Some(stage), summary.clone().unwrap_or_else(|| format!("stage '{stage}' failed")))
                }
            })
        }
        TaskEvent::GateDecided { stage, approved, comment, .. } if stage == SHIP_STAGE && f.stage(stage).is_none() => Some(if *approved {
            Action::Ship
        } else {
            let target = rework_stage(f);
            rework(state, f, &target, comment.clone().unwrap_or_else(|| "Rejected before shipping.".into()))
        }),
        TaskEvent::GateDecided { stage, approved, comment, .. } => Some(if *approved {
            advance(state, f, stage)
        } else {
            rework(state, f, stage, comment.clone().unwrap_or_else(|| "Rejected at gate.".into()))
        }),
        TaskEvent::TaskResumed { stage, comment } => {
            let target = stage.clone().or_else(|| blocked_stage(state)).or_else(|| state.last_stage().map(str::to_owned));
            let target = target.as_deref().unwrap_or(&f.first_stage().id);
            if target == SHIP_STAGE && f.stage(target).is_none() && ships(state, f) {
                return Some(Action::Ship);
            }
            Some(match f.stage(target) {
                Some(_) => run(target, comment.clone()),
                None => block(None, format!("cannot resume: unknown stage '{target}'")),
            })
        }
        // PR feedback causes rework only while the task waits on its PR (the fold
        // ignores feedback during a rework, and CI results of an older push). All
        // feedback of one poll is handed over together.
        TaskEvent::PrReviewed { .. } | TaskEvent::PrChecksCompleted { .. }
            if state.status == TaskStatus::Shipped && !state.pr_feedback.is_empty() =>
        {
            Some(pr_rework(state, f, state.pr_feedback.join("\n")))
        }
        TaskEvent::StageEntered { .. }
        | TaskEvent::GateRequested { .. }
        | TaskEvent::TaskBlocked { .. }
        | TaskEvent::TaskCompleted { .. }
        | TaskEvent::TaskCancelled { .. }
        | TaskEvent::TaskShipped { .. }
        | TaskEvent::PrReviewed { .. }
        | TaskEvent::PrChecksCompleted { .. }
        | TaskEvent::PrMerged { .. }
        | TaskEvent::PrClosed { .. } => None,
    }
}

/// Whether finished tasks of this factory ship to GitHub (benchmarks never push).
fn ships(state: &TaskState, f: &Factory) -> bool {
    f.github.is_some() && state.benchmark.is_none()
}

/// After the last stage: ship (behind the ship gate, if any) or complete locally.
fn finish(state: &TaskState, f: &Factory) -> Action {
    match &f.github {
        Some(g) if ships(state, f) => match &g.ship_gate {
            Some(gate) => Action::RequestGate { gate: gate.clone(), stage: SHIP_STAGE.into() },
            None => Action::Ship,
        },
        _ => Action::Complete,
    }
}

fn rework_stage(f: &Factory) -> String {
    f.github.as_ref().map_or_else(|| f.first_stage().id.clone(), |g| g.rework_stage.clone())
}

fn pr_rework(state: &TaskState, f: &Factory, feedback: String) -> Action {
    let target = rework_stage(f);
    if state.pr_rework_loops >= f.max_rework_loops {
        return Action::Block {
            stage: Some(target),
            reason: format!("PR rework limit reached ({} loops); the PR still needs changes", f.max_rework_loops),
        };
    }
    run(&target, Some(feedback))
}

fn run(stage: &str, feedback: Option<String>) -> Action {
    Action::RunStage { stage: stage.to_owned(), feedback }
}

fn block(stage: Option<&String>, reason: String) -> Action {
    Action::Block { stage: stage.cloned(), reason }
}

fn advance(state: &TaskState, f: &Factory, stage: &str) -> Action {
    match f.next_stage(stage) {
        Some(next) => run(&next.id, None),
        None => finish(state, f),
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
