-- Phase 3: file index. The DB is the source of truth; blobs live on disk under an immutable
-- random storage_id, so rename/move are pure DB updates and the fileid never changes.

CREATE TABLE settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE file_cache (
    fileid     INTEGER PRIMARY KEY AUTOINCREMENT,   -- never reused
    owner_id   INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    parent_id  INTEGER REFERENCES file_cache (fileid) ON DELETE CASCADE,  -- NULL for the user root
    name       TEXT    NOT NULL,                    -- '' for the root
    path       TEXT    NOT NULL,                    -- '' for the root, 'a/b' (no leading slash), NFC
    is_dir     INTEGER NOT NULL,
    size       INTEGER NOT NULL DEFAULT 0,          -- recursive for directories
    mtime      INTEGER NOT NULL,
    etag       TEXT    NOT NULL,
    mimetype   TEXT    NOT NULL DEFAULT '',
    checksum   TEXT,                                -- e.g. 'SHA1:...' as sent by the client
    storage_id TEXT,                                -- blob id, NULL for directories
    UNIQUE (owner_id, path)
);
CREATE INDEX file_cache_parent_idx ON file_cache (parent_id);
