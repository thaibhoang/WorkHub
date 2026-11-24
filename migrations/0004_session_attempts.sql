ALTER TABLE session_facts
    ADD COLUMN attempt  INT  NOT NULL DEFAULT 1,
    ADD COLUMN retry_of UUID;
