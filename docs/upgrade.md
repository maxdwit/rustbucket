# Upgrading

1. Read `git log` for the new version. Back up first (`make backup`; compose users: `scripts/backup.sh`) - see [backup-restore.md](backup-restore.md).
2. Build the new image (`make image`), then `make rm run` (the volumes stay; the server is back within seconds, clients retry).
3. Migrations run on start (they are forward-only in normal operation; each also has a `.down.sql` for emergencies).
4. Check `make logs`, `GET /status.php`, and log in once.

Roll back: stop, restore the backup taken in step 1 (the image tag you ran before + that database snapshot). Do not run an
older image on a database that a newer image already migrated.

The Nextcloud version the server reports is pinned (35.0.x); clients decide what to call from the advertised
capabilities, so new server features only appear when they are implemented and advertised on purpose.
