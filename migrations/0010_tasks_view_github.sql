-- GitHub (phase 8): the task's pull request and its review / merge timestamps,
-- for PR links and cycle times (kickoff → PR → first review → merge).
ALTER TABLE tasks_view
    ADD COLUMN external_ref      TEXT,            -- e.g. owner/name#12 for tasks from issues
    ADD COLUMN pr_url            TEXT,
    ADD COLUMN pr_number         INT,
    ADD COLUMN pr_state          TEXT,            -- open | merged | closed
    ADD COLUMN shipped_at        TIMESTAMPTZ,     -- first push / PR opened
    ADD COLUMN first_review_at   TIMESTAMPTZ,     -- first human review on the PR
    ADD COLUMN merged_at         TIMESTAMPTZ,
    ADD COLUMN pr_reviews        INT NOT NULL DEFAULT 0,
    ADD COLUMN changes_requested INT NOT NULL DEFAULT 0,
    ADD COLUMN ci_failures       INT NOT NULL DEFAULT 0,
    ADD COLUMN pr_rework_loops   INT NOT NULL DEFAULT 0;
