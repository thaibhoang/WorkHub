# Agent questions, part 1: question block parser and resumable sessions

## Split (the full feature is XL; each part ships on its own, in this order)
1. **Question parser + resumable runtime** (this spec): parse `QUESTION:` lines; the runtime can persist and resume a session.
2. **Events and state**: `AgentAsked`, `UserAnswered`, `TaskStatus::AwaitingInput`, session link `resume_of`, fold, factory `max_questions` (default 3); projections and API tolerate the new events. **Done** (`agent-questions-events-state.md`).
3. **Runner pause/resume**: a stage that asks keeps its worktree and sandbox home, uncommitted, with no `StageExited`; a resume turn (own timeout, budget, usage) runs in the same cwd; commit only when the stage ends; pipeline sessions switch to `Persist`.
4. **Foreman loop**: pure `decide` for ask → wait → answer → resume, `wh_api::commands::answer` (`Exact`, 409), `wh task answer <task> [--option N] "..."`, protection from `recover_interrupted`/janitor, fallback to a fresh session with the stage's conversation, over-limit "decide yourself" reminder, benchmark auto-answer.
5. **API and metrics**: answer endpoint, per-stage conversation, Inbox, waiting time excluded from durations, each Q&A turn counted as a human intervention (new migration).
6. **UI, prompts, docs**: per-stage chat with option buttons and free text, `web/src/lib/api.ts`; `factory/agents/*.md` tell agents when and how to ask (only now, so nothing asks before the loop works); architecture.md and CLAUDE.md.

Notes for parts 3–4: Claude Code session files live under `$HOME/.claude` keyed by cwd (Docker: keep `sandbox-home/<session>` of the first turn). The answer is passed as the prompt argument, so give it a fixed prefix (text starting with `-` must not read as a flag). A missing session or worktree must lead to the fallback, not to retries. Check whether a resumed run reports cost and turns per invocation or cumulatively.

## Goal (part 1)
A pure parser for the agent's question block, and an `AgentRuntime` request that can say "ephemeral", "persist" or "resume this session". No behaviour change in production.

## Non-goals
New events, statuses, commands, migrations, UI, prompt changes; persisting any session in a running pipeline.

## Assumptions
- Format: one line per question, `QUESTION: {"text": "...", "options": ["A","B"]}`; JSON on a single line. `options` may be missing or empty (open question). Several lines = several questions.
- Only the block at the **end** of the final message counts (blank lines and code fences allowed between and after), so a quoted example mid-message is not a question.
- A malformed line (bad JSON, missing or blank `text`) is ignored; valid lines in the block still count. No valid line = not asking.
- Markdown around the line (`**`, backticks) is tolerated, as in `parse_verdict`.
- Judge, improver, smoke check and (until part 3) pipeline sessions stay `Ephemeral`: `--no-session-persistence` is dropped only for `Persist` and `Resume`, so no session files pile up.
- `claude -p --resume <id>` without `--fork-session` continues under the same id (both flags exist in local `claude --help`; slice 3 verifies the behaviour).

## Design
- New `crates/wh-core/src/question.rs`: `Question { text: String, #[serde(default)] options: Vec<String> }` (Serialize, Deserialize, Clone, PartialEq, Eq) and `parse_questions(&str) -> Vec<Question>`; export from `lib.rs`.
- `crates/wh-runner/src/runtime.rs`: `enum SessionMode { #[default] Ephemeral, Persist, Resume(String) }`; new field `AgentRequest.session`. Export from `lib.rs`.
- `crates/wh-runner/src/claude_code.rs`: `ClaudeCode::args` adds `--no-session-persistence` for `Ephemeral`, nothing for `Persist`, `--resume <id>` for `Resume`. All other flags are unchanged on resume (model, budget, tools, permissions).
- One-line `session: SessionMode::Ephemeral` at the other `AgentRequest` literals: `wh-runner/src/runner.rs`, `wh-scorer/src/lib.rs`, `wh-improver/src/lib.rs`, `wh-cli/src/sandbox.rs`, tests in `claude_code.rs`.
- `docs/architecture.md`: in §5.3 describe the question block and session modes.

## Boundaries
No change to events, migrations, factory.yaml, agent prompts or `parse_verdict`. Do not edit the phase 1 note's history; add to §5.3.

## Plan
1. Parser with unit tests (with options, without, several, broken JSON, blank text, mid-message example, markdown/fences). Check: `cargo test -p wh-core question`.
2. `SessionMode`, args, call sites, unit test of all three modes. Check: `cargo test -p wh-runner claude_code && cargo build`.
3. Verify resume once with the real CLI (small cost, haiku): run `-p` with persistence, then `--resume <session_id>` in the same directory; record in §5.3 whether the id is kept and how cost is reported. If it cannot be run, say so in the summary. Then docs.

## Acceptance criteria
- `cargo test -p wh-core question`: each case in slice 1 passes; broken JSON alone yields no questions.
- `cargo test -p wh-runner session_mode`: `Resume("abc")` args contain `--resume abc` and no `--no-session-persistence`; `Persist` contains neither; `Ephemeral` keeps `--no-session-persistence`.
- `cargo test` passes and `cargo clippy --all-targets -- -D warnings` is clean.
