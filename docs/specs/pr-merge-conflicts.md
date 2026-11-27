# Spec: detect and resolve merge conflicts on shipped PRs

## Goal / non-goals
A Shipped PR that conflicts with its base is detected, optionally gated, merged by WorkHub, resolved by `github.rework_stage`, re-verified and pushed to the same PR.
Out of scope: rebase, force-push, auto-merge, benchmarks, commits others pushed to the PR branch. No migration, no new `tasks_view` columns. One implementation pass.

## Design
**Event / fold (`wh-core`)**
- `TaskEvent::PrConflicted { base, commit }`. `CONFLICT_STAGE = "conflict"`.
- `TaskState.conflict: Option<{base, commit, approved}>`. Set by `PrConflicted` only if `Shipped` and `commit == pr.commit`. `GateDecided{stage: conflict, approved: true}` sets `approved`.
- Cleared by `StageExited{Succeeded}`, `TaskShipped`, `PrMerged`, `PrClosed`. A failed merge stage keeps it, so resume merges again.
- `StageEntered` counts in `pr_rework_loops` if `Shipped` **or** `conflict` is set (the status is `AwaitingGate` after a gate).
- `pr_feedback` is also collected while `conflict` is set and no stage has started, so feedback arriving during the gate is not lost.

**Poller (`wh-github`)**
- Add `mergeable,mergeStateStatus,baseRefName` to `PR_VIEW_FIELDS`. Only `mergeable == CONFLICTING` triggers; `UNKNOWN`/`MERGEABLE` do nothing.
- `observe` emits `Conflict` for an `OPEN` PR, once per head commit (`Seen.conflicted_commits`), after checks, in the same `Exact(version)` batch.
- `sync_task` drops it unless the task is `Shipped` and the head equals `pr.commit`, so an ignored sighting never marks the commit as seen.

**Factory:** `github.conflict_gate: Option<String>` (non-empty). Stage id `conflict` is reserved when `github:` is set. Enable `conflict-review` in `factory/factory.yaml`.

**`decide`**: new `Action::ResolveConflict { stage: rework_stage, feedback }`; feedback = conflict description + any `pr_feedback` (+ gate/resume comment).
- `Shipped` with `conflict` set, head event any `Pr*` (checked **before** the PR-feedback arm): at the loop limit → `Block`; else `RequestGate{conflict_gate, "conflict"}` or, without a gate, `ResolveConflict`.
- `GateDecided` for `conflict`, as guarded arms **before** the generic one (which would otherwise ship unmerged, or panic on reject): approved → `ResolveConflict`; rejected → `Block{stage: "conflict"}`.
- A `Pr*` head while `conflict.approved` and still `AwaitingGate` → `ResolveConflict` (a poll landed before the foreman handled the approval).
- `TaskResumed` with `conflict` set and target `conflict` or the rework stage → `ResolveConflict` (resume counts as approval; the limit still applies).

**Merge (`wh-runner`, driven by the foreman)**
- `RunSpec.merge: Option<{remote, base}>`, set only for `ResolveConflict`.
- Per attempt, after `Worktree::create`: `git fetch <remote> <base>`, then `git merge --no-ff --no-commit FETCH_HEAD` (WorkHub identity). Do this before `AgentDispatched`, so the recorded and sent prompt includes the result: conflicted files with conflict type and marker hunks (size-capped, always listing every file), or "merged cleanly, check it works".
- Fetch failure, or merge failure without unmerged files → `env_setup`, not retried, task blocks.
- After a successful agent run: if a conflicted file still has `<<<<<<< `/`>>>>>>> ` lines, the session fails (existing `FailureClass`, not retried) and nothing is committed.
- `commit_all` must commit whenever `MERGE_HEAD` exists, even with an empty `status --porcelain`.
- The agent gets no new tools, network or git write access. Delete/binary conflicts it cannot fix with its tools end up blocked or caught by review.

**Surfaces:** `tasks.rs` projection (advance `last_seq` only), CLI event display, API factory DTO + `web/src/lib/api.ts` (`conflict_gate`), `RunDetail.tsx` (`PrConflicted` line "Conflicts with <base> at <sha>"; gate text for stage `conflict`: approve = merge base and let the rework stage resolve, reject = block), `Factory.tsx` (show the gate).
**Docs:** phase 8 notes in `docs/architecture.md` (Vietnamese, plus the event list), `wh-github` paragraph in `CLAUDE.md`.

## Acceptance criteria and test plan
`cargo test`, `cargo clippy --all-targets -- -D warnings` and `cd web && npm run build` pass.
- `wh-core` fold: conflict set only while Shipped on the current commit (ignored during a rework); loop counted in `pr_rework_loops` after a gate; survives a failed stage; old events decode.
- `decide`: gate vs. no gate; approve → `ResolveConflict`; reject → blocked; limit → blocked; conflict + failed CI in one batch → one rework carrying both; resume after reject.
- `poll::observe`: `CONFLICTING` once per commit; `MERGEABLE`, `UNKNOWN`, missing field and merged/closed PRs emit nothing.
- `wh-factory`: `conflict_gate` parses; empty name and stage `conflict` are rejected.
- Runner (temp repo, fake runtime): clean merge, resolved conflict (two parents), leftover markers fail without a commit, resolution equal to "ours" still commits the merge.
- `crates/wh-foreman/tests/github.rs` (fake `gh`, bare remote, never real GitHub): ship → commit on remote `main` touching the same lines → `pr_view.json` says `CONFLICTING` → one `PrConflicted` even when polled twice → gate approved → fake agent sees the markers and resolves → code_review/verify → ship gate if configured → same PR, remote branch contains `main`, pushed without force → merged → `Done`.
