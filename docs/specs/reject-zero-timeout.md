# Spec: factory.yaml rejects `timeout_min: 0`

## Goal
`Factory::load` fails on `timeout_min: 0` instead of producing sessions that time out immediately, get retried and pollute metrics.

## Non-goals
- Other validations, including an upper bound on `timeout_min` (a huge value can overflow `60 * minutes`; follow-up).
- The CLI's own timeout arguments (`wh run` etc.), the web UI, `check_timeout_min` in benchmark suites (already validated in `wh-bench`).

## Design
Only `crates/wh-factory/src/lib.rs` changes. No events, commands, projections, new types or migrations.

In `Factory::load`, check each **explicitly written** value and return one of these errors:

| Where | Error message |
|---|---|
| `defaults` | `defaults: timeout_min must be > 0` |
| agent | `agent '<name>': timeout_min must be > 0` |
| scorer | `scorer '<id>': timeout_min must be > 0` |
| `improvement` | `improvement: timeout_min must be > 0` |

- `defaults` is checked on its own, before the agent loop. So `defaults: { timeout_min: 0 }` is rejected even if every agent overrides it, and the error names `defaults`, not the first agent that inherits it.
- The agent check looks at the agent's own value, not the resolved one.
- An omitted `timeout_min` keeps today's behaviour: agents inherit `defaults`, then 30; scorers 5; improvement 20.

`docs/architecture.md` needs no change: it does not describe field validation.

## Acceptance criteria
1. Each of the four cases above makes `Factory::load` return an error containing that exact message.
2. `defaults: { timeout_min: 0 }` is still rejected when every agent sets a non-zero `timeout_min`.
3. `cargo run --bin wh -- factory --file factory/factory.yaml` succeeds. This is also the factory used by `benchmarks/smoke.yaml`.
4. `cargo test -p wh-factory` passes.
5. `cargo clippy --workspace --all-targets -- -D warnings` passes.

## Test plan
In the `tests` module of `crates/wh-factory/src/lib.rs`:
- Add four rows to the `rejects_invalid_definitions` table, one per place, each an `OK.replace(...)` with the full error message as the needle.
- Add a fifth row for criterion 2: zero in `defaults`, with `timeout_min: 1` on both `a` and `b`.
- `loads_resolves_defaults_and_order` already asserts the agent default of 30 min. Add assertions there for the omitted scorer (5 min) and improvement (20 min) timeouts.

Criterion 3 is a manual command; no new test reads files from the repository.
