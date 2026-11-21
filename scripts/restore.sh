#!/usr/bin/env bash
# Restore a backup made by backup.sh into a compose deployment. Nothing is deleted: the current db/ and
# data/ are renamed to db.before-restore-<ts>/ and data.before-restore-<ts>/ (remove them yourself later).
#
#   scripts/restore.sh BACKUP_DIR      e.g.  scripts/restore.sh /mnt/backup/rustbucket/20261120-203000
# Env: COMPOSE_DIR=.  SERVICE=server  COPY="rsync -aH"  SUDO=""  OWNER="10001:10001"  VERIFY=1 (check SHA256SUMS first)
set -euo pipefail
SRC="${1:?usage: restore.sh BACKUP_DIR}"
COMPOSE_DIR="${COMPOSE_DIR:-.}"; SERVICE="${SERVICE:-server}"
COPY="${COPY:-rsync -aH}"; SUDO="${SUDO:-}"; OWNER="${OWNER:-10001:10001}"
command -v "${COPY%% *}" >/dev/null || COPY="cp -a"
[[ -f "$SRC/rustbucket.db" && -d "$SRC/data" ]] || { echo "$SRC is not a rustbucket backup"; exit 2; }
if [[ "${VERIFY:-1}" == 1 && -f "$SRC/SHA256SUMS" ]]; then
  echo "verifying checksums"; ( cd "$SRC" && sha256sum --quiet -c SHA256SUMS ) || { echo "backup is damaged"; exit 3; }
fi
TS="$(date +%Y%m%d-%H%M%S)"
cd "$COMPOSE_DIR"
echo "1/4 stopping $SERVICE"; docker compose stop "$SERVICE"
echo "2/4 keeping the current state aside"
[[ -e db ]]   && $SUDO mv db "db.before-restore-$TS"
[[ -e data ]] && $SUDO mv data "data.before-restore-$TS"
mkdir -p db data
echo "3/4 restoring"
$SUDO cp "$SRC/rustbucket.db" db/rustbucket.db
if [[ "$COPY" == "cp -a" ]]; then $SUDO cp -a "$SRC/data/." data/; else $SUDO $COPY "$SRC/data/" data/; fi
$SUDO chown -R "$OWNER" db data; $SUDO chmod 750 db data
echo "4/4 starting and checking the index against the disk"
docker compose up -d "$SERVICE"
for _ in $(seq 1 40); do docker compose exec -T "$SERVICE" rustbucket healthcheck >/dev/null 2>&1 && break; sleep 1; done
docker compose exec -T "$SERVICE" rustbucket scan --dry-run
echo "If the scan reports differences (files written/removed after the database snapshot), run: docker compose exec $SERVICE rustbucket scan"
