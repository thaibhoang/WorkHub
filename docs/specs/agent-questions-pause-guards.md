# Agent questions, part 4b-1: guard a paused stage (size M)

## Split of 4b (L as a whole: 7+ source files; in this order)
1. **4b-1, guard a paused stage** (this spec): nothing destroys, blocks or grades a paused stage. Pause stays **off**.
2. **4b-2, foreman loop, pause on** (M): ask → wait → answer → resume, question limit, benchmark auto-answer, real CLI check.

Prerequisite (checked, present): `wh_api::commands::answer`, `TaskEvent::StageResumed`.

## Goal
Given the events of a paused or resumed stage, the janitor, scorer, startup recovery and a later fresh run of the task all behave correctly.

## Non-goals
`decide`, appending `AgentAsked`, calling `Runner::resume`, turning on `pause_on_question`, `Lost` fallback (4c), API/UI, agent prompts, migrations, metrics.

## Assumptions
1. A paused turn is marked on its session: `AgentSessionCompleted.paused`. Without it the janitor can delete the sandbox home between session completion and `AgentAsked`.
2. A blocked or cancelled task's leftover pause is **not** protected: normal cleanup rules apply (sandbox home at once, worktree after the grace period). Its uncommitted work can be lost.
3. Running any stage anew discards the leftover worktree and home, uncommitted work included.
4. The scorer skips paused turns and `session_lost` failures in the daemon only; `wh score session` still grades what it is told to.
5. The judge of a resumed session gets the first turn's prompt (following `resume_of`); transcript and commit remain the last turn's.

## Design
- `wh-core/src/session.rs`: `AgentSessionCompleted` gets `#[serde(default)] paused: bool`. Fix the struct literals in tests.
- `wh-runner/src/runner.rs`: set it when the turn ends `End::Paused`. New `Runner::discard(repo, home_session)`: remove `worktrees/<id>` (forced, whatever branch it is on; helper in `worktree.rs`) and `sandbox-home/<id>`. Missing is not an error.
- `wh-foreman/src/lib.rs`:
  - `recover_interrupted` also treats head `StageResumed` as running. Head `AgentAsked` (waiting) and `UserAnswered` (redelivered later) stay untouched.
  - Before `runner.run` (`RunStage`, `ResolveConflict`), if `state.paused` has a `resume` handle, call `discard` for its `home_session`; the old worktree holds the task branch. A failure is only logged (`Worktree::create` then reports it).
- `wh-foreman/src/cleanup.rs`, `sweep`: before removing a finished session's worktree or sandbox home, fold its task (from `AgentDispatched.task_id`). Keep both if the status is `Running` or `AwaitingInput` **and** either `paused.resume.home_session` is this session, or the status is `Running` and the session's head is a paused completion. A kept directory with status `Running` counts towards `running` (no cache eviction).
- `wh-scorer/src/lib.rs`: assumptions 4 and 5.
- `docs/architecture.md` §5.3: mark these guards done.

## Boundaries
No migrations. Old session events must decode. `pause_on_question` stays `false`. No change to `decide`, task events, factory files or prompts. `task_over` and branch cleanup rules are unchanged.

## Plan
1. `paused` flag in core and runner. Check: `cargo test -p wh-runner pause`.
2. `Runner::discard`, recovery, discard before a run. Check: `cargo test -p wh-runner discard && cargo test -p wh-foreman --test pipeline paused`.
3. Janitor protection. Check: `cargo test -p wh-foreman --test cleanup paused`.
4. Scorer. Check: `cargo test -p wh-scorer paused`.
5. Docs. Check: `cargo test && cargo clippy --all-targets -- -D warnings`.

## Acceptance criteria
- `cargo test -p wh-runner pause`: a paused turn records `paused: true`, a normal one `false`; old JSON without the field decodes.
- `cargo test -p wh-runner discard`: worktree and home are gone and the branch can be checked out again; a second call succeeds.
- `cargo test -p wh-foreman --test pipeline paused`: recovery leaves an `AwaitingInput` task alone and blocks a `StageResumed` head; a task blocked while paused, then resumed, runs its stage on the same branch.
- `cargo test -p wh-foreman --test cleanup paused`: with zero grace, directories survive while waiting, answered, resumed, and between paused completion and `AgentAsked`; they are removed once blocked, cancelled or exited.
- `cargo test -p wh-scorer paused`: paused and `session_lost` sessions get no `ScoreStarted`; a resumed session's judge prompt holds the first prompt.
- `cargo test` and `cargo clippy --all-targets -- -D warnings` pass.

## Notes for 4b-2 (design for the next task, not implemented here)
- `pause_on_question`: `true` for a new stage run (`StageEntered` resets rounds); on resume `question_rounds <= max_questions`.
- Runner returns `Paused` → reload; only if still `Running` that stage, append `AgentAsked` (with `ResumeHandle`) at `Exact`, retrying on conflict.
- Over the limit (`question_rounds + 1 > max_questions`) or benchmark: append `AgentAsked` + automatic `UserAnswered` ("decide yourself, record your assumptions") together, system actor, so part 5 can exclude them.
- `decide` must not rely on `state.last == UserAnswered`: a PR poll can land after the answer and strand the task. Add a `TaskState` flag set by `UserAnswered`, cleared by `StageResumed`/`StageEntered`/`StageExited`/`TaskBlocked`; flag + `Running` → `ResumeStage`.
- Claim with `StageResumed` at `Exact` before spawning (same permit, drain guard and task database as a run). Build `Paused` from `state.paused`, the answer from `format_answers(dialogue.last())`. No resume handle, unknown stage or `Resumed::Lost` → `TaskBlocked`.
- Verify with the real CLI (haiku): resume cost/`num_turns` per run or cumulative, and the `No conversation found` text. Record in §5.3, or state there that it was not run.
