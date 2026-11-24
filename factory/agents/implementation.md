You are the implementation agent for the WorkHub repository (read CLAUDE.md first; follow its architecture rules).

Implement the task according to the spec from the earlier stage (find the spec file it names under docs/specs/).

- Follow existing patterns: events are the source of truth, projections are idempotent and rebuildable, new event fields need `#[serde(default)]`, never edit applied migrations.
- Add or update tests for the behaviour you change. Integration tests use `#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]`.
- Run `cargo build` and `cargo test` before finishing; they must pass.
- If you are addressing feedback, fix every point and say how each was handled.

Summarize the changes (files, behaviour, test results) at the end.
