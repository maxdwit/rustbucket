#!/usr/bin/env bash
# Consistent backup of a rustbucket deployment (docker compose with ./db and ./data bind mounts).
#
#   scripts/backup.sh DEST            e.g.  scripts/backup.sh /mnt/backup/rustbucket
# Env: COMPOSE_DIR=.   (directory with compose.yml, db/ and data/)   SERVICE=server
#      COPY="rsync -aH" (how to copy the data folder; hard links must be kept: versions are hard links)
#      SUDO=""          (e.g. "sudo -n" when data/ is only readable by the container user)
#
# Order matters: 1. an online snapshot of the database (VACUUM INTO, safe while the server runs),
# 2. the data folder. Files are written before their index row and removed after it, so a data copy that is
# newer than the database snapshot is repaired by `rustbucket scan` after a restore (restore.sh does that).
set -euo pipefail
DEST="${1:?usage: backup.sh DEST}"
COMPOSE_DIR="${COMPOSE_DIR:-.}"; SERVICE="${SERVICE:-server}"
COPY="${COPY:-rsync -aH}"; SUDO="${SUDO:-}"
command -v "${COPY%% *}" >/dev/null || COPY="cp -a"          # no rsync: cp -a keeps hard links within one run too
TS="$(date +%Y%m%d-%H%M%S)"
OUT="$DEST/$TS"; mkdir -p "$OUT"
cd "$COMPOSE_DIR"

echo "1/3 database snapshot"
docker compose exec -T "$SERVICE" sh -c "mkdir -p /db/backups && rustbucket backup /db/backups/snap-$TS.db"
$SUDO cp "db/backups/snap-$TS.db" "$OUT/rustbucket.db"

echo "2/3 data folder"
mkdir -p "$OUT/data"
if [[ "$COPY" == "cp -a" ]]; then $SUDO cp -a data/. "$OUT/data/"; else $SUDO $COPY data/ "$OUT/data/"; fi

echo "3/3 checksums"
( cd "$OUT" && find . -type f ! -name SHA256SUMS -print0 | LC_ALL=C sort -z | xargs -0 -r sha256sum > SHA256SUMS )
echo "backup written to $OUT  ($(wc -l < "$OUT/SHA256SUMS") files, $(du -sh "$OUT" | cut -f1))"
