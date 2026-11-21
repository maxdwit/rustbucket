-- Phase 6: sharing with users and groups. A share points at the shared node by fileid (never by path),
-- so the owner can rename/move the shared item and the share follows; deleting the item removes the share.
CREATE TABLE shares (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    share_type  INTEGER NOT NULL CHECK (share_type IN (0, 1)),          -- 0 user, 1 group
    owner_id    INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    file_source INTEGER NOT NULL REFERENCES file_cache (fileid) ON DELETE CASCADE,
    share_with  TEXT    NOT NULL,                                       -- uid or gid
    permissions INTEGER NOT NULL,                                       -- 1 read, 2 update, 4 create, 8 delete, 16 share
    stime       INTEGER NOT NULL,
    note        TEXT    NOT NULL DEFAULT ''
);
CREATE INDEX shares_with_idx ON shares (share_type, share_with);
CREATE INDEX shares_source_idx ON shares (file_source);
CREATE INDEX shares_owner_idx ON shares (owner_id);

-- Where the share appears in a recipient's tree. One row per recipient; for group shares it is
-- created the first time the member is seen. `hidden`: the recipient removed a group share for themselves.
CREATE TABLE share_targets (
    share_id INTEGER NOT NULL REFERENCES shares (id) ON DELETE CASCADE,
    user_id  INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    target   TEXT    NOT NULL,                                          -- path in the recipient's tree, no leading slash
    hidden   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (share_id, user_id)
);
CREATE INDEX share_targets_user_idx ON share_targets (user_id);
