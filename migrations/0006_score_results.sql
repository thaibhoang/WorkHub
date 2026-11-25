-- Read model: one row per scorer run (LLM-as-judge grading one agent session).
-- Rebuildable from score-* streams.
CREATE TABLE score_results (
    score_id        UUID PRIMARY KEY,
    scorer          TEXT             NOT NULL,
    target_session  UUID             NOT NULL,
    task_id         UUID             NOT NULL,
    agent           TEXT             NOT NULL,
    stage           TEXT             NOT NULL,
    model           TEXT,
    pass_threshold  DOUBLE PRECISION NOT NULL,
    manual          BOOLEAN          NOT NULL DEFAULT false,
    status          TEXT             NOT NULL, -- running | scored | failed
    score           DOUBLE PRECISION,
    pass            BOOLEAN,
    classification  TEXT,
    rationale       TEXT,
    error           TEXT,
    cost_usd        DOUBLE PRECISION NOT NULL DEFAULT 0,
    input_tokens    BIGINT           NOT NULL DEFAULT 0,
    output_tokens   BIGINT           NOT NULL DEFAULT 0,
    duration_ms     BIGINT,
    transcript      TEXT,
    started_at      TIMESTAMPTZ      NOT NULL,
    ended_at        TIMESTAMPTZ,
    -- factory version of the scorer definition (the graded session's version is in session_facts)
    factory_version TEXT,
    last_seq        BIGINT           NOT NULL
);

CREATE INDEX score_results_session_idx ON score_results (target_session);
CREATE INDEX score_results_task_idx ON score_results (task_id);
CREATE INDEX score_results_scorer_idx ON score_results (scorer, started_at);
