-- Append-only event log: the single source of truth.
CREATE TABLE events (
    global_seq     BIGSERIAL PRIMARY KEY,
    event_id       UUID        NOT NULL UNIQUE,
    stream_id      TEXT        NOT NULL,
    stream_version BIGINT      NOT NULL,
    event_type     TEXT        NOT NULL,
    schema_version SMALLINT    NOT NULL DEFAULT 1,
    payload        JSONB       NOT NULL,
    meta           JSONB       NOT NULL,
    recorded_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (stream_id, stream_version)
);

CREATE INDEX events_type_idx ON events (event_type);
CREATE INDEX events_correlation_idx ON events ((meta ->> 'correlation_id'));

CREATE FUNCTION events_immutable() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'events table is append-only';
END
$$;

CREATE TRIGGER events_no_update_delete
BEFORE UPDATE OR DELETE ON events
FOR EACH ROW EXECUTE FUNCTION events_immutable();

-- Per-consumer position in the global log (projections, foreman, scorers...).
CREATE TABLE consumer_checkpoints (
    consumer   TEXT PRIMARY KEY,
    last_seq   BIGINT      NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
