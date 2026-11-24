ALTER TABLE tasks_view
    ADD COLUMN factory         TEXT,
    ADD COLUMN rework_loops    INT NOT NULL DEFAULT 0,
    ADD COLUMN gate_rejections INT NOT NULL DEFAULT 0,
    ADD COLUMN blocked_reason  TEXT,
    ADD COLUMN completed_at    TIMESTAMPTZ;
