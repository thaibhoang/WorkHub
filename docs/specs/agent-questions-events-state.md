# Agent questions, part 2: events, `AwaitingInput`, `max_questions`

Size: L by file count (most edits are one line). Not split further: it is already part 2 of 6 (`agent-questions-resume-foundation.md`).

## Goal / Non-goals
Goal: the task log, fold, read models, API and factory config understand "an agent asked, the user answered".
Non-goals: nothing emits the new events in production; no runner pause, answer command or endpoint, `wh task answer`, prompt change, Inbox, metrics, recovery/janitor change.

## Assumptions
1. `max_questions` limits asking **rounds** (`AgentAsked` events) per stage entry, not questions. It is only loaded and exposed here (enforced in part 4).
2. Every `StageEntered` (rework and resume too) resets the round count.
3. `answers[i]` answers `questions[i]`. `Answer { option: Option<String> /* option text, not index */, text: Option<String> }`, both `#[serde(default)]`. No validation here (part 4's command).
4. `user` is a plain `String`, like `GateDecided.reviewer`.
5. After `UserAnswered` the status is `Running{stage}` (the stage never exited), so a second answer is rejected by status.
6. `resume_of` stays on the event only: no `session_facts` column. **No migration**: `tasks_view.status` is TEXT.
7. `max_questions: 0` is a load error (it cannot disable asking).
8. `TaskStateDto` gets status and stage only; questions are visible in the event timeline until part 5.
9. The web gets only the status type and badge label, so the new status does not render as "Queued".

## Design
- `wh-core/src/task.rs`:
  - `TaskEvent::AgentAsked { stage: String, session: SessionId, questions: Vec<Question> }`, `TaskEvent::UserAnswered { stage: String, answers: Vec<Answer>, user: String }`, `Answer` (exported), `event_type` arms.
  - `TaskStatus::AwaitingInput { stage }`.
  - `TaskState`: `pending_questions: Option<PendingQuestions { stage, session, questions }>`, `question_rounds: u32`.
  - Fold: `AgentAsked` sets pending, increments rounds, status `AwaitingInput`. `UserAnswered` acts only while `AwaitingInput`: clears pending, status `Running{stage}`. `StageEntered` resets both fields. Any other transition out of the wait (`TaskBlocked`, `TaskCancelled`, `PrMerged`, `PrClosed`) clears pending. PR feedback events do not change the status.
  - `last_stage_entered` returns the stage for `AwaitingInput` too.
- `wh-core/src/session.rs`: `AgentDispatched.resume_of: Option<SessionId>` with `#[serde(default)]`. Set `None` in `wh-runner/src/runner.rs` and in the test literals (`wh-api/tests/api.rs`, `wh-foreman/tests/{pipeline,cleanup}.rs`, `wh-improver/tests/improver.rs`).
- `wh-factory/src/lib.rs`: `Settings.max_questions: Option<u32>`; `Agent.max_questions: u32` = agent, else defaults, else 3 (`DEFAULT_MAX_QUESTIONS`); `ensure!` > 0 for both, like `timeout_min`.
- `wh-foreman/src/decide.rs`: return `None` while `AwaitingInput` (next to the `Done`/`Cancelled` guard); add both events to the `None` arm.
- `wh-metrics/src/tasks.rs`: `AgentAsked` → `status = 'awaiting_input'`; `UserAnswered` → `status = 'running'`; both through `guarded`, `current_stage` untouched.
- `wh-api`: `commands::status_name` → `"awaiting_input"`; `dto.rs` `TaskStateDto.stage` filled for it.
- `wh-cli/src/main.rs`: timeline detail for both events (questions' first lines; user + answers).
- `web/src/lib/api.ts`, `web/src/components/status.tsx`: `awaiting_input`, label "Needs answer".
- `docs/architecture.md`: §3 lifecycle (any agent stage ⇄ AwaitingInput), §4.2 events and `resume_of`, `max_questions` in the factory section. Mark part 2 done in the foundation spec's Split.

## Boundaries
No edits to migrations, agent prompts, `factory/factory.yaml`, runner behaviour, `recover_interrupted`, cleanup. Existing events keep decoding.

Note for part 4: a PR poll can append after `UserAnswered` before the foreman handles it (head-only rule), so resume must not depend on `state.last` being `UserAnswered`; compare the gate arm for `PrConflicted`.

## Plan
1. Events, state, fold, `decide`, and every exhaustive match (projection, status name, DTO, CLI) so the workspace compiles; unit tests for serde round-trip, fold and `decide`. Check: `cargo test -p wh-core question && cargo test -p wh-foreman awaiting_input && cargo build`.
2. `resume_of` with an old-payload decode test. Check: `cargo test -p wh-core resume_of && cargo build --all-targets`.
3. `max_questions` and its load test. Check: `cargo test -p wh-factory max_questions`.
4. Projection test in `wh-metrics/tests/tasks_projection.rs`; web; docs. Check: `cargo test -p wh-metrics && (cd web && npm install && npm run build)`.

## Acceptance criteria
- `cargo test -p wh-core question`: tests named `question_*` show both events round-trip with `type` = `event_type()`; fold goes Running → AwaitingInput (pending set, rounds 1) → Running (pending cleared); a second ask gives rounds 2; `StageEntered` resets to 0; cancel clears pending.
- `cargo test -p wh-core resume_of`: an `AgentDispatched` payload without the field decodes to `None`.
- `cargo test -p wh-foreman awaiting_input`: `decide` is `None` after `AgentAsked`, after a PR event while awaiting, and after `UserAnswered`.
- `cargo test -p wh-factory max_questions`: unset → 3; defaults apply; agent overrides; 0 (either level) and an unknown key fail.
- `cargo test -p wh-metrics awaiting_input`: row shows `awaiting_input` with `current_stage` kept, then `running`; `rework_loops` unchanged.
- `cargo test` passes, `cargo clippy --all-targets -- -D warnings` is clean, `npm run build` succeeds in `web/`.
