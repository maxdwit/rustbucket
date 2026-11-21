# Backup and restore

**Standalone container:** `make backup` writes `backups/rustbucket-<stamp>.tar.gz` (database snapshot first, then the
files, hard links kept). `make restore FILE=...` puts a backup into **empty** volumes and starts the container; it never
overwrites anything: to restore over a running installation first take a backup, then `make purge CONFIRM=1`, then
`make restore`. `scripts/smoke.sh` tests the round trip. The rest of this page describes the building blocks and the
compose-style scripts (`scripts/backup.sh`, `scripts/restore.sh`) for deployments with `./db` and `./data` bind mounts.

## What to back up
1. **The database** - take an online snapshot, never copy `rustbucket.db` while the server runs:
   `docker exec -i rustbucket rustbucket backup /db/backups/snap-$(date +%F).db`
2. **The data folder** (`data/`): files, trash, versions. **Copy it after the snapshot**, and keep hard links
   (`rsync -aH`, `cp -a`, `tar`): old versions are hard links of the older file contents.
   `data/.appdata` (preview cache) and `data/*/tmp`, `data/*/uploads` need no backup.

`scripts/backup.sh DEST` does both in the right order and writes `SHA256SUMS`:
`COMPOSE_DIR=/var/lib/rustbucket SUDO="sudo -n" scripts/backup.sh /mnt/backup/rustbucket`.

Why the order is safe: a file is written before its index row and removed after it. A data copy that is newer than
the database snapshot only contains *extra* files (picked up by `scan`) or lacks files deleted meanwhile (dropped by `scan`).

## Restore
`scripts/restore.sh BACKUP_DIR` (verifies `SHA256SUMS`, stops the server, **moves the current `db/` and `data/` aside**
instead of deleting them, copies the backup in, fixes ownership, starts the server and runs `scan --dry-run`).
If the scan lists differences (files changed after the snapshot) run `rustbucket scan` once.
Clients keep working: file ids, etags and app passwords are in the database. A client that had synced *newer* data than
the backup will re-upload its changes on the next sync.

Manual restore: stop the server, put `rustbucket.db` into `db/` (delete stale `-wal`/`-shm`), copy `data/` back with
`rsync -aH`, `chown -R 10001 db data`, start, `rustbucket scan --dry-run`.

## Tested automatically
`scripts/backup_restore_test.sh` builds a deployment (users, ~35 files, three versions, a trashed file, a share),
backs it up with `backup.sh`, **wipes everything**, restores, and checks: byte-identical data tree, index == disk,
logins, profile, share, trash, versions, and a fresh `nextcloudcmd` download equal to the server tree.

## Off-site and VM snapshots
A VM/volume snapshot of the host is fine as an additional layer if the snapshot is crash-consistent (SQLite WAL
recovers) - but keep the file-level backup above, it can be restored without the VM.
