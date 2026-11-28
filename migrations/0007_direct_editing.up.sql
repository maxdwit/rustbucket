-- Direct editing for the Nextcloud mobile apps: a one-time token (handed out by the OCS "open" call) is exchanged
-- for a direct session that may open exactly one file in the editor and nothing else.
CREATE TABLE direct_tokens (
    token_hash TEXT PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    file_id    INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE TABLE direct_sessions (
    id_hash    TEXT PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    file_id    INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
