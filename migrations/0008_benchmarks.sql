-- Phase 6: benchmarks.

-- Benchmark cases are real tasks/sessions but must stay out of production
-- metrics (dashboard, stats, improver observation, before/after by version).
ALTER TABLE tasks_view
    ADD COLUMN benchmark_run  UUID,
    ADD COLUMN benchmark_case TEXT;
ALTER TABLE session_facts ADD COLUMN benchmark_run UUID;

CREATE INDEX tasks_view_benchmark_idx ON tasks_view (benchmark_run);
CREATE INDEX session_facts_benchmark_idx ON session_facts (benchmark_run);

-- Read model: one row per benchmark run (suite × factory variant). Rebuildable from bench-* streams.
CREATE TABLE benchmark_runs (
    run_id          UUID PRIMARY KEY,
    suite           TEXT             NOT NULL,
    suite_file      TEXT             NOT NULL,
    git_ref         TEXT,
    proposal_id     UUID,
    label           TEXT,
    repeat          INT              NOT NULL,
    -- requested | running | completed | failed
    status          TEXT             NOT NULL,
    requested_by    TEXT             NOT NULL,
    requested_at    TIMESTAMPTZ      NOT NULL,
    started_at      TIMESTAMPTZ,
    finished_at     TIMESTAMPTZ,
    factory         TEXT,
    factory_version TEXT,
    factory_commit  TEXT,
    suite_version   TEXT,
    planned         INT,
    cases_done      INT              NOT NULL DEFAULT 0,
    passed          INT              NOT NULL DEFAULT 0,
    cost_usd        DOUBLE PRECISION NOT NULL DEFAULT 0,
    duration_ms     BIGINT,
    -- failure message, or why the run stopped early
    message         TEXT,
    last_seq        BIGINT           NOT NULL
);

CREATE INDEX benchmark_runs_suite_idx ON benchmark_runs (suite, requested_at);
CREATE INDEX benchmark_runs_proposal_idx ON benchmark_runs (proposal_id);

-- Read model: one row per case run (case × repeat) of a benchmark run.
CREATE TABLE benchmark_cases (
    run_id          UUID             NOT NULL,
    case_id         TEXT             NOT NULL,
    repeat          INT              NOT NULL,
    task_id         UUID             NOT NULL,
    -- running | done | blocked | cancelled | error
    status          TEXT             NOT NULL,
    passed          BOOLEAN,
    check_passed    BOOLEAN,
    check_exit_code INT,
    check_output    TEXT,
    cost_usd        DOUBLE PRECISION NOT NULL DEFAULT 0,
    duration_ms     BIGINT,
    sessions        INT,
    rework_loops    INT,
    message         TEXT,
    started_at      TIMESTAMPTZ      NOT NULL,
    finished_at     TIMESTAMPTZ,
    last_seq        BIGINT           NOT NULL,
    PRIMARY KEY (run_id, case_id, repeat)
);
