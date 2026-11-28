use wh_core::task::{CONFLICT_STAGE, SHIP_STAGE, StageOutcome, TaskEvent, TaskState, TaskStatus};
use wh_factory::Factory;

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    RunStage { stage: String, feedback: Option<String> },
    /// Hand the user's answers to the paused `stage`.
    ResumeStage { stage: String },
    /// Run `stage` after WorkHub merged the PR's base into the task branch.
    ResolveConflict { stage: String, feedback: String },
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
    // `AwaitingInput` waits for a human, whatever else lands on the stream.
    if matches!(state.status, TaskStatus::Done | TaskStatus::Cancelled | TaskStatus::AwaitingInput { .. }) {
        return None;
    }
    // Not the last event: others may land between the answer and the resume.
    if let TaskStatus::Running { stage } = &state.status
        && state.resume_pending
    {
        return Some(match f.stage(stage) {
            Some(_) => Action::ResumeStage { stage: stage.clone() },
            None => block(Some(stage), format!("stage '{stage}' is not in factory '{}'", f.name)),
        });
    }
    match state.last.as_ref()? {
        // A draft waits for a human to activate it.
        TaskEvent::TaskCreated { draft: true, .. } => None,
        TaskEvent::TaskCreated { .. } | TaskEvent::TaskActivated {} => Some(run(&f.first_stage().id, None)),
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
        // Must come before the generic arm: the pseudo stage has no next stage (it
        // would ship the PR unmerged) and cannot be re-run on rejection.
        TaskEvent::GateDecided { stage, approved, comment, .. } if stage == CONFLICT_STAGE && f.stage(stage).is_none() => {
            let comment = comment.as_deref();
            Some(if *approved { resolve_conflict(state, f, comment) } else { conflict_rejected(comment) })
        }
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
            if state.conflict.is_some() && f.github.is_some() && (target == CONFLICT_STAGE || target == rework_stage(f)) {
                return Some(resolve_conflict(state, f, comment.as_deref()));
            }
            Some(match f.stage(target) {
                Some(_) => run(target, comment.clone()),
                None => block(None, format!("cannot resume: unknown stage '{target}'")),
            })
        }
        // A pending merge conflict takes priority over other PR feedback, which is
        // handed to the same rework.
        TaskEvent::PrConflicted { .. } | TaskEvent::PrReviewed { .. } | TaskEvent::PrChecksCompleted { .. }
            if state.conflict.is_some() && waits_on_conflict(state) =>
        {
            match state.conflict.as_ref()?.decision {
                _ if state.status == TaskStatus::Shipped => Some(on_conflict(state, f)),
                // A poll landed before the foreman handled the gate decision.
                Some(true) => Some(resolve_conflict(state, f, None)),
                Some(false) => Some(conflict_rejected(None)),
                None => None,
            }
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
        | TaskEvent::AgentAsked { .. }
        | TaskEvent::UserAnswered { .. }
        | TaskEvent::StageResumed { .. }
        | TaskEvent::GateRequested { .. }
        | TaskEvent::TaskBlocked { .. }
        | TaskEvent::TaskCompleted { .. }
        | TaskEvent::TaskCancelled { .. }
        | TaskEvent::TaskShipped { .. }
        | TaskEvent::PrReviewed { .. }
        | TaskEvent::PrChecksCompleted { .. }
        | TaskEvent::PrConflicted { .. }
        | TaskEvent::PrMerged { .. }
        | TaskEvent::PrClosed { .. }
        | TaskEvent::TaskMoved { .. } => None,
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

fn waits_on_conflict(state: &TaskState) -> bool {
    match &state.status {
        TaskStatus::Shipped => true,
        TaskStatus::AwaitingGate { stage, .. } => stage == CONFLICT_STAGE,
        _ => false,
    }
}

/// A conflict was just found: ask a human first, if the factory wants that.
fn on_conflict(state: &TaskState, f: &Factory) -> Action {
    match f.github.as_ref().and_then(|g| g.conflict_gate.as_ref()) {
        // No point asking when the loop limit would block the rework anyway.
        Some(gate) if state.pr_rework_loops < f.max_rework_loops => {
            Action::RequestGate { gate: gate.clone(), stage: CONFLICT_STAGE.into() }
        }
        _ => resolve_conflict(state, f, None),
    }
}

fn conflict_rejected(comment: Option<&str>) -> Action {
    let comment = comment.map(|c| format!(": {c}")).unwrap_or_default();
    Action::Block {
        stage: Some(CONFLICT_STAGE.into()),
        reason: format!("resolving the merge conflict was rejected{comment}"),
    }
}

/// Merge the base and let the rework stage resolve, within the PR rework limit.
fn resolve_conflict(state: &TaskState, f: &Factory, comment: Option<&str>) -> Action {
    let base = state.conflict.as_ref().map_or("the base branch", |c| c.base.as_str());
    let mut feedback = format!(
        "The pull request conflicts with its base branch `{base}`. WorkHub merges `{base}` into the task branch: \
         resolve every conflict, keeping the intent of both sides. Do not commit; WorkHub commits the merge."
    );
    for more in state.pr_feedback.iter().map(String::as_str).chain(comment) {
        feedback.push_str("\n\n");
        feedback.push_str(more);
    }
    match pr_rework(state, f, feedback) {
        Action::RunStage { stage, feedback } => Action::ResolveConflict { stage, feedback: feedback.unwrap_or_default() },
        other => other,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use wh_core::task::CHANGES_REQUESTED;

    fn factory(github: &str) -> Factory {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "a").unwrap();
        let yaml = format!(
            "name: f\nrepo: .\nmax_rework_loops: 1\nstages:\n  - {{ id: impl, agent: a }}\nagents:\n  a: {{ prompt: a.md }}\ngithub: {github}\n"
        );
        let path = dir.path().join("factory.yaml");
        std::fs::write(&path, yaml).unwrap();
        Factory::load(&path).unwrap()
    }

    fn shipped(commit: &str) -> TaskEvent {
        TaskEvent::TaskShipped {
            repo: "a/b".into(),
            pr_number: 1,
            pr_url: "u".into(),
            branch: "b".into(),
            commit: commit.into(),
            created: false,
        }
    }

    fn conflicted() -> TaskEvent {
        TaskEvent::PrConflicted { base: "main".into(), commit: "c1".into() }
    }

    fn gate_requested() -> TaskEvent {
        TaskEvent::GateRequested { gate: "g".into(), stage: CONFLICT_STAGE.into() }
    }

    fn decided(approved: bool) -> TaskEvent {
        TaskEvent::GateDecided {
            gate: "g".into(),
            stage: CONFLICT_STAGE.into(),
            approved,
            reviewer: "r".into(),
            comment: None,
        }
    }

    fn state(events: Vec<TaskEvent>) -> TaskState {
        TaskState::fold((1..).zip(events))
    }

    fn resolves(a: Option<Action>) -> String {
        match a {
            Some(Action::ResolveConflict { stage, feedback }) if stage == "impl" => feedback,
            other => panic!("expected ResolveConflict, got {other:?}"),
        }
    }

    fn blocks(a: Option<Action>, at: &str) {
        assert!(matches!(&a, Some(Action::Block { stage: Some(s), .. }) if s == at), "{a:?}");
    }

    #[test]
    fn drafts_run_only_once_activated() {
        use wh_core::{TaskId, task::TaskSource};
        let f = factory("null");
        let created = |draft| TaskEvent::TaskCreated {
            title: "t".into(),
            body: None,
            source: TaskSource::Cli,
            factory: Some("f".into()),
            benchmark: None,
            external_ref: None,
            parent: None,
            draft,
        };
        let moved = || TaskEvent::TaskMoved { parent: Some(TaskId::new()) };
        let runs = |a: Option<Action>| matches!(a, Some(Action::RunStage { stage, feedback: None }) if stage == "impl");
        assert_eq!(decide(&state(vec![created(true)]), &f), None);
        assert_eq!(decide(&state(vec![created(true), moved()]), &f), None);
        assert!(runs(decide(&state(vec![created(true), TaskEvent::TaskActivated {}]), &f)));
        assert!(runs(decide(&state(vec![created(false)]), &f)));
        // A move landing before the foreman acted must not swallow the start.
        assert!(runs(decide(&state(vec![created(false), moved()]), &f)));
        assert!(runs(decide(&state(vec![created(true), TaskEvent::TaskActivated {}, moved()]), &f)));
        let entered = TaskEvent::StageEntered { stage: "impl".into(), feedback: None };
        assert_eq!(decide(&state(vec![created(false), entered, moved()]), &f), None);
    }

    #[test]
    fn conflict_is_gated_or_resolved_right_away() {
        let gated = factory("{ conflict_gate: g }");
        let s = state(vec![shipped("c1"), conflicted()]);
        let request = Action::RequestGate { gate: "g".into(), stage: CONFLICT_STAGE.into() };
        assert_eq!(decide(&s, &gated), Some(request));
        assert!(resolves(decide(&s, &factory("{}"))).contains("`main`"));

        let approved = state(vec![shipped("c1"), conflicted(), gate_requested(), decided(true)]);
        resolves(decide(&approved, &gated));
        let rejected = state(vec![shipped("c1"), conflicted(), gate_requested(), decided(false)]);
        blocks(decide(&rejected, &gated), CONFLICT_STAGE);
    }

    #[test]
    fn conflict_during_a_rework_is_ignored() {
        let entered = TaskEvent::StageEntered { stage: "impl".into(), feedback: None };
        let s = state(vec![shipped("c1"), entered, conflicted()]);
        assert_eq!(decide(&s, &factory("{ conflict_gate: g }")), None);
    }

    #[test]
    fn conflict_respects_the_pr_rework_limit() {
        let entered = TaskEvent::StageEntered { stage: "impl".into(), feedback: Some("x".into()) };
        let s = state(vec![shipped("c0"), entered, shipped("c1"), conflicted()]);
        blocks(decide(&s, &factory("{ conflict_gate: g }")), "impl");
        blocks(decide(&s, &factory("{}")), "impl");
    }

    #[test]
    fn conflict_and_review_share_one_rework_and_resume_counts_as_approval() {
        let review = TaskEvent::PrReviewed {
            review_id: "1".into(),
            reviewer: "bob".into(),
            state: CHANGES_REQUESTED.into(),
            body: "rename it".into(),
            comments: vec![],
            submitted_at: None,
        };
        let f = factory("{}");
        assert!(resolves(decide(&state(vec![shipped("c1"), review, conflicted()]), &f)).contains("rename it"));

        let blocked = TaskEvent::TaskBlocked { stage: Some(CONFLICT_STAGE.into()), reason: "no".into() };
        let resumed = TaskEvent::TaskResumed { stage: None, comment: Some("go on".into()) };
        let s = state(vec![shipped("c1"), conflicted(), gate_requested(), decided(false), blocked, resumed]);
        assert!(resolves(decide(&s, &factory("{ conflict_gate: g }"))).contains("go on"));
    }

    #[test]
    fn poll_landing_before_the_gate_decision_is_handled_keeps_the_decision() {
        let f = factory("{ conflict_gate: g }");
        let poll_after = |approved| {
            let review = TaskEvent::PrReviewed {
                review_id: "1".into(),
                reviewer: "bob".into(),
                state: CHANGES_REQUESTED.into(),
                body: "rename it".into(),
                comments: vec![],
                submitted_at: None,
            };
            state(vec![shipped("c1"), conflicted(), gate_requested(), decided(approved), review])
        };
        assert!(resolves(decide(&poll_after(true), &f)).contains("rename it"));
        blocks(decide(&poll_after(false), &f), CONFLICT_STAGE);
    }

    #[test]
    fn awaiting_input_does_nothing() {
        let f = factory("{ repo: a/b }");
        let asked =
            TaskEvent::AgentAsked { stage: "impl".into(), session: wh_core::SessionId::new(), questions: vec![], resume: None };
        let mut events = vec![TaskEvent::StageEntered { stage: "impl".into(), feedback: None }, asked];
        assert_eq!(decide(&state(events.clone()), &f), None);
        events.push(conflicted());
        assert_eq!(decide(&state(events.clone()), &f), None);
    }

    #[test]
    fn resume_pending_survives_later_events_until_the_task_stops() {
        let f = factory("{ repo: a/b }");
        let stage = f.first_stage().id.clone();
        let mut events = vec![
            TaskEvent::StageEntered { stage: stage.clone(), feedback: None },
            TaskEvent::AgentAsked { stage: stage.clone(), session: wh_core::SessionId::new(), questions: vec![], resume: None },
            TaskEvent::UserAnswered { stage: stage.clone(), answers: vec![], user: "u".into() },
        ];
        let resume = Some(Action::ResumeStage { stage: stage.clone() });
        assert_eq!(decide(&state(events.clone()), &f), resume);
        events.extend([conflicted(), TaskEvent::TaskMoved { parent: None }]);
        assert_eq!(decide(&state(events.clone()), &f), resume);
        for end in [
            TaskEvent::StageResumed { stage: stage.clone() },
            TaskEvent::TaskBlocked { stage: Some(stage.clone()), reason: "x".into() },
            TaskEvent::TaskCancelled { reason: "x".into() },
        ] {
            let mut ended = events.clone();
            ended.push(end);
            assert_eq!(decide(&state(ended), &f), None);
        }
    }
}
