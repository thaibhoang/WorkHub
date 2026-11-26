# Spec: per-task database for agents

## Goal
No agent ever inherits the live `DATABASE_URL`. A factory can give each task its own migrated database, shared by its stages and dropped by cleanup.

## Non-goals
A separate Postgres server or role, the web UI. This prevents **accidents, not malice**: the task URL carries the admin credentials, so an agent could still connect to the live database on purpose.

## Design
No new events, commands or projections.

**Environment** (`ClaudeCode::command`, so stages, scorers, improver, `wh run`, sandbox smoke; worktree and docker): `env_remove` `DATABASE_URL` and every `WH_AGENT_DB_*` variable *before* `envs(&req.env)`. Bench `run_check` (runs agent-written code) strips the same and gets the case task's URL, if any.

**Factory**: top-level optional `task_database: { admin_url_env: <NAME> }`. `NAME` must be a variable name starting with `WH_AGENT_DB_` (so stripping needs no plumbing). The value is read from the WorkHub process, never stored in events or logs.

**Provisioning** (new `wh-foreman/src/task_db.rs`):
- Name: `wh_task_` + the **last** 12 hex of the task UUID, the same id part as `task_branch`. The first 12 of a UUIDv7 are a timestamp and would collide.
- URL: the admin URL with only the database name replaced.
- Before each stage (inside the dispatched future, before `StageEntered`): create if missing (a concurrent "already exists" is fine), run `MIGRATOR`, mark ready. A ready database is **not** migrated again: the task branch may have applied newer migrations.
- `RunSpec.env["DATABASE_URL"]` = task URL unless the agent's factory `env:` sets it.
- Failure (variable unset, server down, migration error): no session; append `TaskBlocked{stage, reason: "task database: …"}` at `Exact(version)`. `wh task resume` retries.
- Benchmark tasks get databases the same way.

**Ownership**: new migration `migrations/0012_instance.sql`, a one-row table with a random uuid (not a read model; `rebuild` leaves it). Each task database gets `COMMENT ON DATABASE` = `workhub:<uuid>[ ready]`. Without this, "task no longer exists" would let a sweep from another event store on the same server (e.g. an agent's `cargo test`) drop live databases.

**Cleanup**: `CleanupConfig` gets the optional admin URL (redacted in `Debug`). `sweep` considers only databases matching `^wh_task_[0-9a-f]{12}$` **and** carrying this instance's comment. It drops them (`WITH (FORCE)`, kind `database`) when the task head is `TaskCompleted`/`PrMerged`, is `TaskCancelled` older than the grace period, or no task in the store maps to the name. Blocked, shipped and open tasks keep theirs. `--dry-run` lists them. An unset variable or unreachable server goes to `report.errors`; the rest of cleanup still runs.

**Nested `wh` (addition beyond the task, strike if unwanted)**: with `cargo run --bin wh` allowed, an agent's `wh cleanup` against its task database would see every live worktree and sandbox home as orphaned and delete them. So the foreman also sets `WH_DATA_DIR=<tmp>/wh-agent-<task name>` for task sessions (factory `env:` wins).

**Config and docs**: `factory/factory.yaml` sets `task_database: { admin_url_env: WH_AGENT_DB_ADMIN_URL }` and allows `Bash(cargo run --bin wh:*)`. Update `.env.example`, `deploy/` (compose variable + README), `CLAUDE.md`, `docs/architecture.md`.

**For the reviewer**
- A running WorkHub needs `WH_AGENT_DB_ADMIN_URL` before loading the new factory, or tasks block.
- The allowed pattern also permits `wh run` / `wh foreman`, i.e. an agent can start paid sessions on its own database, outside the live budget and metrics.
- Without `task_database`, agents' `#[sqlx::test]` tests have no URL unless factory `env:` sets one.

## Acceptance criteria
- `cargo test` and `cargo clippy --all-targets -- -D warnings` pass.
- `wh factory` accepts `factory/factory.yaml`; a name without the prefix or an unknown key under `task_database` is rejected.
- After a task's first stage, `psql -l` shows its `wh_task_*` database; `wh cleanup --dry-run` lists it once the task is done.

## Test plan
- `wh-runner` unit: the command (worktree and docker) removes the parent's `DATABASE_URL` and `WH_AGENT_DB_*`, and carries `DATABASE_URL` only when `req.env` has it.
- `wh-factory` unit: parsing and validation. `task_db` unit: name (two ids with the same timestamp differ) and URL rewrite (query string kept).
- `#[sqlx::test]`, fake runtime recording `req.env`, admin URL = the test `DATABASE_URL`:
  - two stages get the same URL, the database exists with the `events` table, and a marker row written after stage 1 survives stage 2;
  - an extra `_sqlx_migrations` row does not break the next stage;
  - two tasks get different URLs;
  - provisioning failure blocks the task with no session;
  - sweep: done and merged dropped; cancelled only after grace; open, blocked and shipped kept; marked orphan dropped; dry run drops nothing;
  - sweep leaves untouched a non-`wh_task_*` database, a near-miss name, and a `wh_task_*` database without this instance's comment.
- Tests use fresh random ids and drop what they create.
