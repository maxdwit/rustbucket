CREATE TABLE users (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    uid           TEXT    NOT NULL UNIQUE,
    display_name  TEXT    NOT NULL,
    email         TEXT,
    password_hash TEXT    NOT NULL,
    is_admin      BOOLEAN NOT NULL DEFAULT 0,
    enabled       BOOLEAN NOT NULL DEFAULT 1,
    created_at    TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE groups (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    gid          TEXT NOT NULL UNIQUE,
    display_name TEXT NOT NULL,
    created_at   TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE group_members (
    group_id INTEGER NOT NULL REFERENCES groups (id) ON DELETE CASCADE,
    user_id  INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    PRIMARY KEY (group_id, user_id)
);

CREATE INDEX group_members_user_idx ON group_members (user_id);
