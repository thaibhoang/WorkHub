# Agent questions, part 4a: stored pause, dialogue and `wh task answer` (size M)

## Split of part 4 (XL as a whole; in this order, each ships on its own)
- **4a. State and answer** (this spec): events keep everything needed to resume; the task remembers the conversation; `commands::answer`, `wh task answer`.
- **4b. Foreman loop**: ask → wait → answer → resume, guards, limit and benchmark auto-answer, real CLI check, pause **on**.
- **4c. Lost fallback**: a lost pause restarts the stage with its conversation in the prompt.

## Goal
After a foreman restart, the task stream alone says how to resume a paused stage, and a human can answer from the CLI.

## Non-goals
Foreman/runner behaviour (`pause_on_question` stays **false**), guards, API endpoint, UI, agent prompts, migrations. Until 4b nothing in production asks.

## Assumptions
1. The resume data is **stored** on `AgentAsked`, not rebuilt: merge conflict paths are in no other event.
2. `Runner::resume` writes no `StageEntered`, so 4b claims a resume with a new event `StageResumed{stage}` appended at `Exact`. Its shape is added now.
3. `answers` has exactly one entry per pending question. Each has an option, free text, or both (one question only); an option must be one of the offered strings.
4. CLI, one question: `wh task answer <task> [--option N] ["text"]`, N is 1-based, at least one of the two. Several questions: one positional value per question, in order; a value equal to an option (exact text) counts as that option, anything else is free text; `--option` is refused.
5. No `--factory` flag: validation needs only `TaskState`.
6. `answer` takes an optional `asked: Option<SessionId>` (the question set being answered). A mismatch is `Conflict`, so a later API/UI cannot answer newer questions by mistake. The CLI passes `None`.

## Design
- `wh-core/src/task.rs`:
  - `ResumeHandle { home_session: SessionId, runtime_session_id: String, branch: String, conflicts: Vec<String> }`.
  - `AgentAsked` gets `#[serde(default)] resume: Option<ResumeHandle>`.
  - New `TaskEvent::StageResumed { stage }`: status stays/becomes `Running{stage}`.
  - `TaskState` gets `paused: Option<Pause { stage, session, resume }>` and `dialogue: Vec<Exchange { questions, answers, user }>`. `AgentAsked` sets `paused`; `UserAnswered` pushes an exchange built from `pending_questions`. Both are cleared by `StageEntered`; `paused` also by `StageExited`. They **survive** `TaskBlocked`/`TaskResumed` (4b discards the leftover worktree).
  - `pending_questions` and `question_rounds` behave as today.
- `wh-core/src/question.rs`: pure `format_answers(&[Question], &[Answer]) -> String` (numbered question, then the answer), the text 4b passes to `Runner::resume`.
- `wh-api/src/commands.rs`: `answer(store, actor, task, asked, answers) -> Result<()>`: status must be `AwaitingInput` (else `InvalidState`), validate (assumptions 3 and 6), append `UserAnswered{stage, answers, user: actor_name}` with `Exact(state.version)`. A second answer fails because the status is no longer waiting; a race gives `Conflict` (409).
- `wh-cli/src/main.rs`: `TaskCmd::Answer`; mapping arguments → `Vec<Answer>` is a pure, unit-tested function. `task show` prints pending questions with numbered options and the answer command.
- Exhaustive matches: `wh-foreman/src/decide.rs` (`StageResumed` → `None`; `..` on `AgentAsked`), `wh-metrics/src/tasks.rs` (`status = 'running'`, as for `UserAnswered`), CLI timeline, anything else the compiler reports.
- Docs: `docs/architecture.md` events list (near "Hỏi đáp") and §5.3: replace "Ghi chú cho phần 4" with the notes below.

## Notes for 4b / 4c (copy into architecture.md; not implemented now)
- **Verify first** with the real CLI (haiku): resume cost/`num_turns` per run or cumulative, and the `No conversation found` text. Record in §5.3.
- Runner reports `Paused` → foreman appends `AgentAsked` with `Exact` on a freshly loaded state, only if still `Running` that stage.
- `decide`: last event `UserAnswered` → `ResumeStage`; foreman appends `StageResumed` before spawning `Runner::resume`. Pause stays on while `question_rounds <= max_questions`.
- Over the limit, and benchmark tasks: append `AgentAsked` + an automatic `UserAnswered` in one append (reminder "decide yourself and record your assumptions" when over the limit). Part 5 must not count these as human interventions.
- `recover_interrupted` must also treat head `StageResumed` as running.
- Janitor: protect `worktrees/<home_session>` and `sandbox-home/<home_session>` while `paused` is set and status is `AwaitingInput` or `Running` (count the latter as running). Discard a leftover pause before running a stage anew, as it holds the branch.
- Scorer: add `#[serde(default)] paused: bool` to `AgentSessionCompleted` and skip those (no race with `AgentAsked`); skip `session_lost`; give the judge the first turn's prompt for resumed sessions.
- Until 4c, `Resumed::Lost` → `TaskBlocked`. 4c: discard the old worktree/home (uncommitted work is lost), run a fresh session with `dialogue` and the original feedback and merge, with `StageEntered{continuation: true}` (`#[serde(default)]`) keeping rounds and dialogue, so at most one fallback per answer.

## Boundaries
No migrations; no edits to applied ones. Old `AgentAsked` events must still decode. No change to `wh-runner`, `wh-scorer`, foreman actions, factory files or agent prompts.

## Plan
1. Events, fold, `format_answers`, exhaustive matches. Check: `cargo test -p wh-core && cargo build`.
2. `commands::answer` with tests (append `AgentAsked` directly). Check: `cargo test -p wh-api answer`.
3. `wh task answer`, `task show`. Check: `cargo test -p wh-cli answer`.
4. Docs. Check: `cargo test && cargo clippy --all-targets -- -D warnings`.

## Acceptance criteria
- `cargo test -p wh-core question`: `AgentAsked` JSON without `resume` decodes; ask → answer → `StageResumed` leaves `Running`, `paused` set, one exchange; ask again adds a second; `StageEntered` clears both; blocked keeps them; `format_answers` covers option, text and both. (Name the new tests with `question`.)
- `cargo test -p wh-api answer`: a valid answer appends `UserAnswered` by the actor; not waiting, answering twice, wrong count, empty answer and unknown option are refused with nothing appended; a wrong `asked` is `Conflict`.
- `cargo test -p wh-cli answer`: argument mapping for one and several questions, including `--option` out of range or used with several.
- `cargo run --bin wh -- task answer <task not waiting> "x"` exits non-zero with "not waiting".
- `cargo test` and `cargo clippy --all-targets -- -D warnings` pass.
