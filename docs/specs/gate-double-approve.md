# A gate can only be decided once

Size: 7 files (nominally L), a few lines each. Kept as one task: the UI fix needs the new state field, and the bug is only fixed with both.

## Root cause

`TaskState::apply` (`crates/wh-core/src/task.rs`) leaves the status at `AwaitingGate` on `GateDecided`; it only moves when the foreman appends the next event. Until then (forever if no foreman runs), `wh_api::commands::decide_gate` accepts a second decision, loads the new version, and `Exact` passes: a **second `GateDecided` is written**. Awaiting the refetch in the UI cannot fix this, since the refetched state is still `awaiting_gate`.

## Goal / Non-goals

- Goal: one `GateDecided` per `GateRequested`; a repeat returns 409; UI command buttons stay disabled until fresh data arrives.
- Non-goals: no new status or event, no change to foreman or ship behaviour, no gate id in the request body, no way to re-decide a gate the foreman never acted on (cancel still works).

## Assumptions

1. Keep `TaskStatus::AwaitingGate` after a decision and add a flag: `decide`, the conflict gate and `wh_github` ship all rely on that status.
2. 409 (`CommandError::Conflict`) applies while the decided gate is still the current status, for either verdict. Once the task has moved on, "not waiting at a gate" stays 422. Nothing is written in either case.
3. The existing Conflict message ("the task changed while you were deciding; reload and try again") is good enough.
4. A click made before the UI learns of a *new* gate is out of scope.
5. Resume, cancel and proposal commands already change status on their first event, so they need only the frontend fix.

## Design

- `wh-core` `TaskState`: new `pub gate_decided: bool`; `GateRequested` sets it false, `GateDecided` true (keep the conflict decision logic). Meaningful only while `AwaitingGate`. Derived by folding: no event, projection or migration changes.
- `wh-api` `commands::decide_gate`: after matching `AwaitingGate`, return `CommandError::Conflict` if `state.gate_decided`. This also covers `wh gate approve|reject`. Simultaneous requests are already handled by `Exact(version)`.
- `wh-api` `dto::TaskStateDto` and `web/src/lib/api.ts` `TaskState`: add `gate_decided`.
- `web/src/pages/RunDetail.tsx`:
  - `useCommand.onSuccess` returns `Promise.all` of both `invalidateQueries`, so `isPending` lasts through the refetch (this covers gate, resume, cancel).
  - The awaited refetch can be cancelled by the SSE invalidation in `lib/live.tsx`, so also remember the `state.version` a gate decision or resume succeeded on, and disable those buttons while `state.version` still equals it. Key `ActionPanel` by task id.
  - When `state.gate_decided`, replace the comment box and Approve/Reject with a short "Decision recorded, waiting for the foreman" line.
- `web/src/pages/ProposalDetail.tsx` `Actions`: `done` returns the `Promise.all` of its invalidations; both `onSuccess` handlers return it. In `decide`, call `apply.mutate()` before returning, so `busy` has no gap.
- `docs/architecture.md`: one sentence near the `GateDecided` event list (~line 129): a gate takes one decision, a repeat is 409.

## Boundaries

No changes to event shapes, migrations, `wh-foreman`, `wh-github`, or the existing 422 cases.

## Plan

1. Backend: fold flag, command check, DTO field, tests, doc sentence. Check: `cargo test -p wh-core && cargo test -p wh-api`, then `cargo test -p wh-foreman` (needs `docker compose up -d --wait`, or the stage's `DATABASE_URL`).
2. Frontend: `api.ts`, `RunDetail.tsx`, `ProposalDetail.tsx`. Check: `cd web && npm install && npm run build`.

## Acceptance criteria

- `cargo test -p wh-api double_gate`: new test `double_gate_decision_is_rejected` in `crates/wh-api/tests/api.rs`. Approve returns 200 with `gate_decided: true`; approve again and reject (with comment) each return 409; the stream has exactly one `GateDecided`; a new `GateRequested` for the same gate resets the flag and can be approved.
- `cargo test -p wh-core gate_decided`: unit test for set on decide, reset on request.
- `cargo test -p wh-api`, `cargo test -p wh-foreman` and `cd web && npm run build` pass.
- Manual: with `wh serve` (no foreman), double-click Approve: one success toast, no error toast, one gate approved event, buttons do not come back, including after a reload.
