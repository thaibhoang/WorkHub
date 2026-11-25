-- Login for `wh serve --auth` (deployment behind a public URL).
-- Not read models: these hold credentials, are not derived from events and are
-- never truncated by `projections rebuild`. Managed with `wh user ...`.
CREATE TABLE users (
    username      TEXT PRIMARY KEY,
    password_hash TEXT NOT NULL,           -- argon2id PHC string
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE auth_sessions (
    token_hash   BYTEA PRIMARY KEY,        -- sha256 of the cookie token (the token itself is never stored)
    username     TEXT NOT NULL REFERENCES users (username) ON DELETE CASCADE,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at   TIMESTAMPTZ NOT NULL,
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX auth_sessions_username ON auth_sessions (username);
