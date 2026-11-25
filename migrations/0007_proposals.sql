-- Phase 5: self-improvement.

-- Outcome the pipeline gave the stage a session ran for (from StageExited on the
-- task stream): succeeded | failed | rejected. Lets metrics count review rejects.
ALTER TABLE session_facts ADD COLUMN stage_outcome TEXT;

-- Read model: one row per improver run / proposal. Rebuildable from proposal-* streams.
CREATE TABLE proposals_view (
    proposal_id              UUID PRIMARY KEY,
    factory                  TEXT             NOT NULL,
    -- requested | running | open | no_proposal | failed | approved | rejected | rolled_out
    status                   TEXT             NOT NULL,
    trigger                  TEXT             NOT NULL,
    focus                    TEXT,
    requested_by             TEXT             NOT NULL,
    requested_at             TIMESTAMPTZ      NOT NULL,
    started_at               TIMESTAMPTZ,
    finished_at              TIMESTAMPTZ,
    base_factory_version     TEXT,
    sessions_observed        BIGINT,
    title                    TEXT,
    rationale                TEXT,
    expected_impact          TEXT,
    evidence                 JSONB            NOT NULL DEFAULT '[]',
    files                    JSONB            NOT NULL DEFAULT '[]',
    diff                     TEXT,
    branch                   TEXT,
    commit_sha               TEXT,
    proposed_factory_version TEXT,
    -- skip reason / failure message
    message                  TEXT,
    cost_usd                 DOUBLE PRECISION NOT NULL DEFAULT 0,
    duration_ms              BIGINT,
    transcript               TEXT,
    decided_by               TEXT,
    decided_at               TIMESTAMPTZ,
    decision_comment         TEXT,
    rolled_out_at            TIMESTAMPTZ,
    rolled_out_version       TEXT,
    last_seq                 BIGINT           NOT NULL
);

CREATE INDEX proposals_view_status_idx ON proposals_view (factory, status);
CREATE INDEX proposals_view_requested_idx ON proposals_view (requested_at);
