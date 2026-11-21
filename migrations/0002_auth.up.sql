-- Phase 2: app passwords, Login Flow v2 tokens, browser sessions.
-- All secrets are stored as SHA-256 hex digests (they are high-entropy random tokens).
-- Timestamps are unix seconds.

CREATE TABLE app_passwords (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id      INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    name         TEXT    NOT NULL,
    token_hash   TEXT    NOT NULL UNIQUE,
    created_at   INTEGER NOT NULL,
    last_used_at INTEGER
);
CREATE INDEX app_passwords_user_idx ON app_passwords (user_id);

CREATE TABLE login_flow_tokens (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    poll_hash    TEXT    NOT NULL UNIQUE,
    flow_hash    TEXT    NOT NULL UNIQUE,
    device_name  TEXT    NOT NULL,
    expires_at   INTEGER NOT NULL,
    user_id      INTEGER REFERENCES users (id) ON DELETE CASCADE,
    -- plaintext app password, held only until the client polls it (or the flow expires)
    app_password TEXT
);

CREATE TABLE web_sessions (
    id_hash    TEXT PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
