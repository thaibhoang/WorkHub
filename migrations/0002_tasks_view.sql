-- Read model: rebuildable from events at any time (`wh projections rebuild`).
CREATE TABLE tasks_view (
    task_id       UUID PRIMARY KEY,
    title         TEXT        NOT NULL,
    source        TEXT        NOT NULL,
    status        TEXT        NOT NULL,
    current_stage TEXT,
    created_at    TIMESTAMPTZ NOT NULL,
    updated_at    TIMESTAMPTZ NOT NULL,
    last_seq      BIGINT      NOT NULL
);
