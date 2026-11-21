-- Phase 8: trash bin. A deleted item lives on disk at <data>/<uid>/trash/<id>/item (and its older versions in
-- <id>/versions/); this table is the index. Versions of live files are plain files in
-- <data>/<uid>/versions/<fileid>/<mtime> (no table: the directory listing is the index).
CREATE TABLE trash_items (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    owner_id    INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    name        TEXT    NOT NULL,                 -- original base name
    orig_dir    TEXT    NOT NULL,                 -- original parent folder ('' = top level)
    deleted_at  INTEGER NOT NULL,                 -- unix seconds, unique per (owner, name)
    is_dir      INTEGER NOT NULL,
    size        INTEGER NOT NULL,
    mtime       INTEGER NOT NULL,
    mimetype    TEXT    NOT NULL DEFAULT '',
    orig_fileid INTEGER NOT NULL,
    UNIQUE (owner_id, name, deleted_at)
);
CREATE INDEX trash_items_owner_idx ON trash_items (owner_id, deleted_at);
