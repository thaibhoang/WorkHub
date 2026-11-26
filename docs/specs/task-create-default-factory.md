# Spec: `wh task create` uses the factory by default

## Goal
A task created with `wh task create` belongs to a factory unless the user opts out, so a foreman can drive it. A bad factory file fails the command instead of leaving a task that stays `open` forever.

## Non-goals
Web UI, API (`wh_api::commands::create_task`), `wh run`, other `wh` commands. No check that a foreman is running. Existing factory-less tasks are not touched.

## Design
Only `crates/wh-cli` and `CLAUDE.md`. No new events, commands, projections or migrations; `TaskCreated.factory` still holds the factory **name** or `None`.

- `TaskCmd::Create` (`crates/wh-cli/src/main.rs`):
  - `factory: PathBuf` with `#[arg(long, env = "WH_FACTORY", default_value = "factory/factory.yaml")]`, as in the other commands.
  - new `no_factory: bool` (`--no-factory`): a manual task that no foreman drives.
- A small pure function, e.g. `task_factory(no_factory, &Path) -> anyhow::Result<Option<String>>`: `None` if `no_factory` (the file is not read), else `Factory::load(path)?.name`. The handler calls it **before** appending `TaskCreated`, so an error appends nothing.
- `Factory::load` already reports a missing or unparsable file with its path; no new error types.

### Decisions for the reviewer
1. `--no-factory` always wins, with **no clap conflict**, because a conflict would also fire when `WH_FACTORY` is merely set in the environment (as in production). Consequence: `--factory F --no-factory` silently ignores `F`.
2. The default path is relative to the current directory, so outside the repo root and without `WH_FACTORY`, `task create` now errors (intended: set one, or use `--no-factory`).
3. A foreman only drives tasks whose factory name matches its own. A task still waits if the running foreman uses a differently named factory.

### CLAUDE.md (Commands section)
- `wh -- task create --title "..." [--body "..."]` becomes `wh -- task create --title "..." [--body "..."] [--factory F | --no-factory]`, with a comment: factory defaults to `$WH_FACTORY`, else `factory/factory.yaml`.
- Delete the later `task create ... --factory factory/factory.yaml` line (now redundant).

## Acceptance criteria
With Postgres up and migrated, from the repo root, `WH_FACTORY` unset unless stated (also not in `.env`, which `wh` loads). Check the factory in the `TaskCreated` payload with `wh events stream task-<id>`.

1. `wh task create --title x` → factory is `workhub-dev`.
2. `WH_FACTORY=<other valid file>` → that file's factory name; `--factory` overrides the env var.
3. `wh task create --title x --no-factory` → no factory, whether `WH_FACTORY` is unset, set, or points at a missing file.
4. A missing or invalid file, from `--factory` or `WH_FACTORY` → non-zero exit, error names the path, no new task in `wh task list`.
5. `wh task create --help` shows the default, the env var and `--no-factory`.
6. `CLAUDE.md` updated as above; `cargo test` and `cargo clippy` pass.

## Test plan
- Unit tests in `crates/wh-cli/src/main.rs` (`cargo test -p wh-cli`; add `tempfile = "3"` to dev-dependencies):
  - `task_factory` returns the name for a minimal temp factory (yaml + agent prompt, modelled on the `OK` fixture in the `wh-factory` tests).
  - A missing file and an invalid yaml return errors.
  - `no_factory = true` with a missing path returns `Ok(None)`.
  - Through `Cli::command()`: the `factory` arg of `task create` has env `WH_FACTORY` and default `factory/factory.yaml`; `--no-factory` parses together with `--factory`. Tests must not mutate the process env.
- Manual: run criteria 1–5 against a local database.
