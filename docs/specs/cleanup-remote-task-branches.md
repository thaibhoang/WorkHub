# Spec: delete the remote task branch after its PR is merged or closed

## Goal / non-goals

The janitor (`wh_foreman::cleanup`) also removes `wh/task/<id>` from the remote of a factory with `github:`.

Non-goals: conflict handling, databases/migrations, web UI, closing PRs through `gh`, changes to ship or the poller, new events.

## Rules

| Task | Remote branch | Local branch |
|---|---|---|
| head `PrMerged` | delete now | delete now (as today) |
| head `PrClosed`, status `Blocked` | delete after grace | **keep** (blocked; resume can push it again) |
| status `Cancelled` | delete after grace | delete after grace (as today) |
| anything else (completed locally, blocked, open, shipped, resumed) | keep | keep |

- Grace is measured from the `recorded_at` of the task stream's last event.
- "Cancelled" comes from the folded `TaskState` (`TaskState::fold`), not the head string: the poller can append `PrReviewed`/`PrClosed`/… after `TaskCancelled`, which today hides the task from cleanup.

## Design

No new events, projections or migrations. Read-only on the event store.

- `CleanupConfig` gets `remote: Option<String>`, set by `for_factory` from `f.github.remote`. `None` = no remote work (CLI call sites need no change).
- Candidates come from `git ls-remote --heads <remote> 'refs/heads/wh/task/*'`, **independently of the local branches**. So:
  - a failed delete is retried on the next sweep even if the local branch is gone;
  - a branch already gone is simply not listed;
  - existing leftovers (`wh/task/2881a80a1bb6`, `wh/task/b303d971477a`) get cleaned up.
- A listed branch is matched to a task by `task_branch(id)`. Unknown names and anything outside `refs/heads/wh/task/` are never deleted.
- Delete with `git push <remote> --delete refs/heads/wh/task/<id>`. A "remote ref does not exist" failure (race with GitHub's auto-delete) counts as success and is not reported as removed.
- After the pass (not in dry run): `git remote prune <remote>`. This also drops tracking refs of branches GitHub deleted itself.
- Network git commands run with `GIT_TERMINAL_PROMPT=0` and a 60 s timeout, so the janitor cannot hang. `ls-remote`, delete and prune failures go to `CleanupReport.errors`; the rest of the sweep continues.
- Report: `Removed { kind: "remote-branch", what: "wh/task/<id>", bytes: 0 }`. Dry run lists these without pushing or pruning.
- `task_over` includes `PrClosed`. Update the module docs and the `Removed.kind` comment.
- Docs: the **Cleanup** paragraph in `CLAUDE.md`, and the "Dọn dẹp" bullet in `docs/architecture.md` (in Vietnamese).

## For the reviewer to decide

1. **Cancelled task with an open PR:** deleting its head branch makes GitHub close the PR. Proposed: delete anyway after the grace period, as the task says (cancel = abandon the work, and the grace period is the window to object). Alternative: skip while the PR is open.
2. Commits pushed to the PR branch by someone else are lost from the remote branch on deletion (GitHub can still restore it from the PR). Proposed: accept this.

## Acceptance criteria and test plan

New `#[sqlx::test]` in `crates/wh-foreman/tests/cleanup.rs`, with a temp repo whose `origin` is a local bare repo (never GitHub). The existing test sets `remote: None`.

1. Dry run, long grace: lists `remote-branch` for the merged task only; the bare repo and tracking refs are unchanged.
2. Real run, long grace: the merged branch is gone from the bare repo, as is `refs/remotes/origin/…`. Closed and cancelled branches remain.
3. Grace zero: the closed and cancelled remote branches are gone, including a cancelled task with a later `PrClosed` event. The closed task's local branch remains.
4. Locally completed, blocked, open, closed-then-resumed tasks and a `wh/task/unknown` branch are kept at grace zero.
5. A merged task whose branch was already deleted in the bare repo (stale tracking ref left in the local repo): no errors, ref pruned.
6. A second sweep removes nothing and reports no errors.
7. An unreachable remote yields an entry in `errors`, and local cleanup still happens.

Also: `cargo test` and `cargo clippy --workspace --all-targets -- -D warnings` pass. `wh cleanup --dry-run` prints the `remote-branch` lines.
