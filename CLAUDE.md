# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

WorkHub is an agent factory: Claude agents take tasks through triage → spec → implement → review → verify → ship, every step is recorded as an event, and the metrics derived from those events (cost, duration, failure rate, rework loops, human interventions) feed a self-improvement agent that proposes changes to the workflow. The full design (in Vietnamese) is in `docs/architecture.md`. Read the relevant section before building a new component, and update the doc when the implementation diverges from it. The design is modelled on Warp's Factories product.

Status: **Phase 0 done** (event store, `tasks_view` projection, CLI). Next is Phase 1: a runner with a Claude Code headless adapter and a `session_facts` projection. See the roadmap in the architecture doc, §8.

Decided stack: Rust backend, React + TS + shadcn/ui in `web/` (not created yet, phase 3), local-only deployment, tasks created via WorkHub/CLI, git worktree as the agent sandbox.

## Commands

```bash
docker compose up -d --wait        # Postgres 17 on localhost:5544 (user/pass/db: workhub)
cp .env.example .env               # DATABASE_URL used by the CLI and tests
cargo build
cargo test                         # integration tests need the Postgres above running
cargo test -p wh-eventstore concurrent_appends   # single test (crate + name filter)
cargo run --bin wh -- db migrate
cargo run --bin wh -- task create --title "..." [--body "..."]
cargo run --bin wh -- task list
cargo run --bin wh -- events tail [--after N] | events stream task-<uuid>
cargo run --bin wh -- projections run [--follow] | projections rebuild
```

Integration tests use `#[sqlx::test(migrator = "wh_eventstore::MIGRATOR")]`. Each test gets a fresh throwaway database created from `DATABASE_URL`.

## Architecture (event sourcing + CQRS)

- **`crates/wh-core`**: pure domain types, no I/O. Typed IDs (`TaskId` etc.) map to stream ids `"<prefix>-<uuidv7>"`. `EventMeta` holds `correlation_id` (= task id), `causation_id`, `actor`, and `factory_version`. Domain events are internally tagged serde enums (`#[serde(tag = "type")]`) that implement `DomainEvent`. The payload's `type` must equal `event_type()`. `FailureClass` is the fixed failure taxonomy.
- **`crates/wh-eventstore`**: `events` is append-only (a DB trigger blocks UPDATE/DELETE). `append` takes a **global advisory lock**, so `global_seq` order equals commit order. That is what makes `read_all(after_seq)` with per-consumer `consumer_checkpoints` safe. Don't remove the lock without replacing that guarantee. `ExpectedVersion` gives optimistic concurrency per stream. Each append fires `NOTIFY wh_events`.
- **Consumers** implement `EventHandler` and are driven by `Subscription::catch_up` / `run`. Delivery is **at-least-once**, and the checkpoint is saved after each batch, so handlers must be idempotent. Projections do this by storing `last_seq` per row and ignoring events with a seq at or below it.
- **`crates/wh-metrics`**: read-model projections (implement `Projection` so `rebuild` can truncate and replay). Read-model tables live in `migrations/` alongside the event tables. They are disposable and must be fully derivable from events.
- **`crates/wh-cli`**: the `wh` binary. Writes go through `EventStore::append` (commands → events). Reads come only from projections.
- `migrations/` is embedded at compile time via `wh_eventstore::MIGRATOR`. Add new files and never edit applied ones.

New components from the design (foreman, runner, scorer, improver, api) become new `crates/wh-*` crates following the same pattern: consume events through `Subscription`, emit events through `EventStore`.
