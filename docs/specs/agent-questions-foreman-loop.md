# Agent questions, part 4b-2: the foreman's ask → wait → answer → resume loop

Size: M (about 5 files, mostly `wh-foreman`). No split.

## Before approving (reviewer): verify the real CLI
The spec stage could not run `claude` (needs approval). Run this and put the results in the gate comment:

    cd "$(mktemp -d)"
    claude -p "Remember the word kiwi. Reply OK." --model claude-haiku-5-5 --output-format json
    claude -p "Which word?" --resume <session_id> --model claude-haiku-5-5 --output-format json
    claude -p "hi" --resume 00000000-0000-0000-0000-000000000000 --model claude-haiku-5-5 --output-format json

Record: `total_cost_usd` and `num_turns` of turns 1 and 2 (per run or cumulative?), and the exact error of call 3 (expected to contain `No conversation found`).

## Goal / Non-goals
Goal: a pipeline stage that asks pauses the task (`AwaitingInput`), and an answer resumes it, surviving restarts.
Non-goals: lost-session fallback (4c), API/UI (5–6), agent prompts, `wh run` pausing, autopilot code.

## Assumptions
1. **Pause is on when `question_rounds <= max_questions`**, not `<` as the task says. With `<`, an exhausted agent never returns `Paused`, so the "decide yourself" reply could never be sent. So: rounds `< max` → a human answers; `== max` (and under the limit for benchmarks) → automatic answer; `> max` → pause off, the stage ends normally.
2. Benchmarks get the same "decide yourself and record your assumptions" text on every question, within the same limit.
3. Automatic answers: actor `System{foreman}` and `user = "foreman:auto"` (a `pub const AUTO_ANSWER_USER` in `wh-core`, for part 5 to exclude).
4. **Verification gates enabling.** Pause is enabled only if the results above are in the gate comment or the implementer can run them. Otherwise everything is built with `const PAUSE_ENABLED: bool = false`, tests opt in with `Foreman::with_pause(true)`, and the summary and §5.3 say it is unverified and off.
5. Autopilot (PR #21) is not merged (no `wh-autopilot` crate): the end-to-end test answers as a human only.
6. The 4b-1 guards are present (`Runner::discard` in `run_stage`; `recover_interrupted` treats `StageResumed` as running).
7. A restart after `StageResumed` but before the turn ends (including while waiting for a parallel slot) blocks the task, as 4b-1 does.

## Design
No new events, projections or migrations.

- **`wh-core/src/task.rs`**: `TaskState.resume_pending: bool`. Set by `UserAnswered` only when it moves `AwaitingInput` → `Running`. Cleared by `StageResumed`, `StageEntered`, `StageExited`, `TaskBlocked`, `TaskCancelled`, `TaskCompleted`.
- **`wh-foreman/src/decide.rs`**: `Action::ResumeStage { stage }`, returned before the match on `state.last` whenever `resume_pending` and status is `Running{stage}`. A later PR or `TaskMoved` event therefore does not lose the answer.
- **`wh-foreman/src/lib.rs`**:
  - `run_spec`: `pause_on_question` per assumption 1, using the stage agent's `max_questions`.
  - After `runner.run` or `resume` returns `paused`: `record_pause` reloads the state and, only if still `Running` that stage, appends `AgentAsked{stage, session, questions, resume: Some(ResumeHandle from Paused)}` with `Exact`. On conflict it reloads and retries (giving up would leave the task `Running` forever). If the answer is automatic, `AgentAsked` + `UserAnswered` (one `Answer` per question) go in **one** append.
  - `ResumeStage`: append `StageResumed` with `Exact(state.version)` first; on conflict do nothing. Then dispatch like `run_stage` (permit, drain guard, task database with the same env) and call `Runner::resume`.
  - Everything `resume` needs comes from events: `state.paused` (session, handle), `state.dialogue.last()` (questions and answers, since `pending_questions` is gone) → `format_answers`. Do not call `discard` here.
  - `Resumed::Lost`, a missing handle, or `Err` from `resume` → `TaskBlocked{stage, reason: "cannot resume the paused conversation: …"}`, same reload-and-retry rule. Human `resume` then discards the leftover and reruns (existing path).
  - Resume spec: `feedback` and `merge` are `None` (conflicts travel in the handle). Check `exit_stage` and the turn do not need them.
- **Runner**: change only if verification contradicts it. A different lost string → update the match in `claude_code.rs` and its test. Cumulative cost/turns → the resumed session records the difference from the conversation's earlier sessions, with new `#[serde(default)]` totals on `ResumeHandle`/`Paused` (integer micro-USD, to keep `Eq`).
- **Docs**: §5.3 (and the "đang làm" paragraph near line 97) drop "pause tắt", record the verification and assumption 1; CLAUDE.md `wh-foreman` bullet.

## Boundaries
Applied migrations and existing event shapes stay (new fields need `#[serde(default)]`). `commands::answer`, `recover_interrupted`, janitor and scorer rules are unchanged. No changes to `factory/`, `web/`, API. `wh run` keeps `pause_on_question: false`.

## Plan
1. Run or read the verification; fix the runner if needed. Check: `cargo test -p wh-runner`.
2. `resume_pending` + `ResumeStage` in `decide` (update the existing `UserAnswered` decide test). Until slice 3 the arm may be a no-op. Check: `cargo test -p wh-core && cargo test -p wh-foreman --lib`.
3. Pause, resume, auto-answer and Lost in the foreman, plus `crates/wh-foreman/tests/questions.rs` (fake runtime ending with `QUESTION:` lines and reporting a runtime session id, as the runner's pause tests do; `wh-api` as a dev-dependency for `commands::answer` if there is no cycle). Check: `cargo test -p wh-foreman --test questions`.
4. Docs; then `cargo clippy --all-targets -- -D warnings && cargo test`.

## Acceptance criteria
Tests in `cargo test -p wh-foreman --test questions <name>`:
- `ask_answer_resume`: `AgentAsked` (with handle) → `AwaitingInput`, no `StageExited`; answer → one `StageResumed` → commit and one `StageExited`; the prompt holds the answer; the pipeline moves on.
- `no_double_resume`: a second answer is refused; redelivering `UserAnswered` runs no second turn.
- `restart`: a new `Foreman` over the same store leaves `AwaitingInput` alone, and with head `UserAnswered` resumes from events only.
- `limit`: with `max_questions: 1`, the 2nd question gets an automatic `foreman:auto` answer in the same append; a 3rd ends the stage.
- `benchmark_auto_answers`: `drive()` finishes without a human.
- `lost_blocks`: worktree removed before the answer → `TaskBlocked`; `TaskResumed` reruns the stage.

Also:
- `cargo test -p wh-foreman --lib resume_pending`: `ResumeStage` still returned after an interleaved event; none once blocked or cancelled.
- §5.3 states the verification result, or that it was not done and pause is off.
