-- Read model: one flat row per agent session. Primary input for metrics and
-- for the self-improvement agent. Rebuildable from events.
CREATE TABLE session_facts (
    session_id         UUID PRIMARY KEY,
    task_id            UUID             NOT NULL,
    agent              TEXT             NOT NULL,
    stage              TEXT             NOT NULL,
    runtime            TEXT             NOT NULL,
    model              TEXT,
    status             TEXT             NOT NULL, -- dispatched | running | succeeded | failed
    failure_class      TEXT,
    failure_message    TEXT,
    dispatched_at      TIMESTAMPTZ      NOT NULL,
    ended_at           TIMESTAMPTZ,
    duration_ms        BIGINT,
    num_turns          INT,
    tool_calls         INT              NOT NULL DEFAULT 0,
    tool_errors        INT              NOT NULL DEFAULT 0,
    input_tokens       BIGINT           NOT NULL DEFAULT 0,
    output_tokens      BIGINT           NOT NULL DEFAULT 0,
    cache_read_tokens  BIGINT           NOT NULL DEFAULT 0,
    cache_write_tokens BIGINT           NOT NULL DEFAULT 0,
    cost_usd           DOUBLE PRECISION NOT NULL DEFAULT 0,
    workdir            TEXT,
    branch             TEXT,
    transcript         TEXT,
    factory_version    TEXT,
    last_seq           BIGINT           NOT NULL
);

CREATE INDEX session_facts_task_idx ON session_facts (task_id);
CREATE INDEX session_facts_agent_idx ON session_facts (agent, dispatched_at);
