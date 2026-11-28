# Agent questions, part 3: runner pause/resume (size M)

**Warning:** part 2 (`AgentAsked`, `UserAnswered`, `AwaitingInput`, `resume_of`, `max_questions`) is **not** in the code at `26be0d1`. This spec does not need it and must not add it.

## Goal
A stage whose agent ends with a `QUESTION:` block can pause (worktree and sandbox home kept, no commit, no `StageExited`) and later be resumed by the runner, which commits and exits the stage only when it really ends.

## Non-goals
Foreman `decide`, `wh task answer`, API/UI, agent prompts, new events or migrations, janitor/`recover_interrupted`/scorer awareness of paused stages.

## Assumptions
1. Pausing is opt-in: `RunSpec.pause_on_question`, **false** in foreman and CLI until part 4 (nothing could continue a paused stage). Off = today's behaviour.
2. All runner sessions (pipeline and `wh run`) become `Persist`. Host session files then accumulate under `~/.claude`; accepted. Judge, improver and smoke check stay `Ephemeral`.
3. Pause only when the turn succeeded, questions parse and a runtime session id was received; otherwise the stage ends as today. Asking wins over `verdict_required`.
4. A paused turn is a normal `AgentSessionCompleted` (`commit: None`, summary holds the block) with its usage. If part 2 lands first, also set `resume_of` on the resumed `AgentDispatched`.
5. A resume turn is never retried (the answer must not be replayed on a half-edited tree). Any failure other than "lost" ends the stage as `Failed` and cleans up.
6. A resume turn may ask again (pauses again). Limiting the count is part 4.
7. Each resume turn gets the full `timeout` and `budget_usd` of the `RunSpec`.

## Design
- `runner.rs`:
  - `RunReport.paused: Option<Paused>`; `stage_outcome` becomes `Option<StageOutcome>` (`None` when paused or lost). Update the foreman log and other readers.
  - `Paused` (serde, so part 4 can store it): `session` (last turn), `home_session` (first turn; names `worktrees/<id>` and `sandbox-home/<id>`), `runtime_session_id`, `branch`, `conflicts: Vec<String>` (unfinished merge paths; adapt `with_markers` as needed), `questions: Vec<Question>`.
  - `Runner::resume(spec, paused, answer, on_signal) -> Result<Resumed>`, where `Resumed` is `Ran(RunReport)` or `Lost { reason, session: Option<SessionId> }`. It appends no `StageEntered` and does not merge again. It records a new session whose prompt is `"The user answered your question(s):\n\n{answer}"`, with `SessionMode::Resume` in the same cwd.
  - Share the run/commit/finish code between `attempt` and `resume`. A pause skips commit, `remove_home` and `worktree.remove`. For Docker, delete only the copied `.claude/.credentials.json` (`prepare` copies it again on resume).
- `worktree.rs`: `Worktree::open(repo, path, branch)`; error if the directory is gone or is not a worktree on `branch`.
- `docker.rs`: `prepare` takes the home session separately from the container session (new container name, old home). On resume a missing home dir is `Lost`, checked before `prepare` creates it.
- Lost:
  - Missing worktree or home: `Lost { session: None }`, no events.
  - CLI reports the conversation missing: new `FailureClass::SessionLost` (`wh-core/src/failure.rs`, `session_lost`), `retryable: false`. The session is recorded as failed and the result is `Lost`.
  - Either way there is no `StageExited`, and worktree and home are left alone (part 4 decides the fallback).
- `claude_code.rs`: for `Resume` only, map the "no conversation found" error (result message or stderr; take the exact text from the real CLI) to `SessionLost`.
- Cost/turns: check with the real CLI. If per invocation, record as is. If cumulative, add the previous totals to `Paused` and subtract them (saturating) so each session records only its own turn.
- Call sites: `wh-foreman/src/lib.rs`, `wh-cli/src/main.rs`, runner and scorer tests.
- Docs: `docs/architecture.md` §5.3 (pause/resume, verified cost/id behaviour). Add "Notes for part 4": the janitor currently deletes the sandbox home and worktree of finished sessions; `recover_interrupted` would block a paused stage; scorers would grade paused turns.

## Boundaries
No migrations, no new or changed events (beyond the new `failure_class` value), no change to `parse_questions`, `parse_verdict`, factory.yaml, agent prompts or foreman `decide`.

## Plan
1. Verify the real CLI (haiku): persist, `--resume` in the same cwd, resume a bogus id. Note session id, cost/`num_turns` semantics and error text in §5.3. If it cannot be run, say so in the summary and in §5.3, assume per invocation and match "No conversation found". Check: docs diff.
2. `SessionLost` and its mapping. Check: `cargo test -p wh-runner claude_code`.
3. `Persist` and pause (`pause_on_question`, `Paused`). Check: `cargo test -p wh-runner pause`.
4. `Worktree::open`, home session in `prepare`, `Runner::resume`, lost cases (fake runtime, temp repo). Check: `cargo test -p wh-runner resume`.
5. Docs. Check: `cargo test && cargo clippy --all-targets -- -D warnings`.

## Acceptance criteria
- `cargo test -p wh-runner pause`: asking with the flag on keeps the worktree with uncommitted changes, task stream has `StageEntered` and no `StageExited`; flag off behaves as before; requests use `Persist`.
- `cargo test -p wh-runner resume`: same cwd, `Resume(id)`, answer `"-x"` yields a prompt starting with the prefix; one commit and one `StageExited` at the end; asking again pauses with the same `home_session`; retryable failure runs once.
- `cargo test -p wh-runner resume_lost`: removed worktree, removed sandbox home, and a runtime `SessionLost` each give `Lost`, no retry, no `StageExited`.
- `cargo test -p wh-runner docker`: resumed container has a new name and the first session's home mount.
- `cargo test` passes; `cargo clippy --all-targets -- -D warnings` is clean.
