#!/usr/bin/env bash
# Automated proof that a backup restores to the same data: build state (users, files, versions, trash, shares),
# back it up, WIPE the deployment, restore, and compare checksums, the index (scan), logins, shares, trash,
# versions and a fresh nextcloudcmd download. Throwaway local container; touches nothing else.
#   scripts/backup_restore_test.sh [--no-build]
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE="${IMAGE:-rustbucket:e2e}"; PORT="${PORT:-18482}"; URL="http://127.0.0.1:$PORT"
BUILD=1; [[ "${1:-}" == --no-build ]] && BUILD=0
W="$(mktemp -d "${TMPDIR:-/tmp}/rb-bk.XXXXXX")"; C="rb-bk-$$"
fail=0; ok() { echo "  ok   $*"; }; bad() { echo "  FAIL $*"; fail=1; }
check() { local d="$1"; shift; if "$@"; then ok "$d"; else bad "$d"; fi; }
cleanup() { docker rm -f "$C" >/dev/null 2>&1; rm -rf -- "$W"; }; trap cleanup EXIT
[[ $BUILD == 1 ]] && { docker build -q --target server -t "$IMAGE" "$ROOT" >/dev/null || exit 1; }

# the deployment folder like a compose directory: ./db and ./data are what the scripts back up and restore
CD="$W/compose"
start() { docker rm -f "$C" >/dev/null 2>&1; docker run -d --name "$C" --user "$(id -u):$(id -g)" -p "127.0.0.1:$PORT:8080" -v "$CD/db:/db" -v "$CD/data:/data" "$IMAGE" >/dev/null
  for _ in $(seq 1 50); do curl -sf "$URL/status.php" >/dev/null && return 0; sleep 0.3; done; return 1; }
rb() { docker exec -i "$C" rustbucket "$@"; }
tree_sums() { ( cd "$1" && find . -type f -print0 | LC_ALL=C sort -z | xargs -0 -r sha256sum ); }
api() { curl -s -u "$1:$2" -H 'OCS-APIRequest: true' "${@:3}"; }
OCS="$URL/ocs/v2.php/apps/files_sharing/api/v1"

echo "== build state"
mkdir -p "$CD/db" "$CD/data"; start || { echo "server did not start"; exit 1; }
printf 'pw-alice\n' | rb user create alice --display-name "Alice A" >/dev/null; printf 'pw-bob\n' | rb user create bob >/dev/null
rb user modify alice --email alice@example.org >/dev/null
D="$URL/remote.php/dav/files/alice"
curl -s -u alice:pw-alice -X MKCOL "$D/docs" -o /dev/null
for i in 1 2 3; do head -c $((200000 * i)) /dev/urandom | curl -s -u alice:pw-alice -X PUT -H "X-OC-Mtime: 17000000$i" --data-binary @- "$D/docs/versioned.bin" -o /dev/null; done
for i in $(seq 1 30); do echo "file $i äöü" | curl -s -u alice:pw-alice -X PUT --data-binary @- "$D/docs/f-$i.txt" -o /dev/null; done
echo "to be trashed" | curl -s -u alice:pw-alice -X PUT --data-binary @- "$D/trashed.txt" -o /dev/null; curl -s -u alice:pw-alice -X DELETE "$D/trashed.txt" -o /dev/null
curl -s -u alice:pw-alice -X MKCOL "$D/shared" -o /dev/null; echo "for bob" | curl -s -u alice:pw-alice -X PUT --data-binary @- "$D/shared/hello.txt" -o /dev/null
api alice pw-alice -X POST "$OCS/shares?format=json" -d shareType=0 -d shareWith=bob -d path=/shared -d permissions=15 | grep -q '"statuscode":200' && ok "share created" || bad "share"
before="$(tree_sums "$CD/data")"; nfiles=$(echo "$before" | wc -l)
vers_before="$(curl -s -u alice:pw-alice -X PROPFIND -H 'Depth: 1' "$URL/remote.php/dav/versions/alice/versions/$(curl -s -u alice:pw-alice -X PROPFIND -H 'Depth: 0' --data '<d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns"><d:prop><oc:fileid/></d:prop></d:propfind>' "$D/docs/versioned.bin" | grep -o '<oc:fileid>[0-9]*' | grep -o '[0-9]*')" | grep -o '<d:href>' | wc -l)"
echo "  state: $nfiles files on disk, $vers_before version entries"

echo "== backup (online) with the shipped script"
# the script talks to `docker compose`; here a tiny stand-in maps it to the throwaway container
mkdir -p "$W/bin"; cat > "$W/bin/docker" <<STUB
#!/usr/bin/env bash
if [[ "\$1" == compose ]]; then shift; sub="\$1"; shift
  case "\$sub" in
    exec) shift; [[ "\$1" == -T ]] && shift; shift; exec docker-real exec -i "$C" "\$@" ;;
    stop) docker-real stop "$C" >/dev/null ;;
    up) docker-real start "$C" >/dev/null ;;
  esac
else exec docker-real "\$@"; fi
STUB
ln -sf "$(command -v docker)" "$W/bin/docker-real"; chmod +x "$W/bin/docker"
export PATH="$W/bin:$PATH"
COMPOSE_DIR="$CD" COPY="cp -a" "$ROOT/scripts/backup.sh" "$W/backups" >"$W/backup.log" 2>&1; tail -2 "$W/backup.log"
BK="$(ls -d "$W/backups"/*/ | head -1)"; check "backup folder exists with db + data + checksums" test -f "$BK/rustbucket.db" -a -d "$BK/data" -a -s "$BK/SHA256SUMS"

echo "== DISASTER: the server and both folders are gone"
docker stop "$C" >/dev/null; rm -rf "$CD/db" "$CD/data"
echo "== restore with the shipped script"
COMPOSE_DIR="$CD" COPY="cp -a" OWNER="$(id -u):$(id -g)" "$ROOT/scripts/restore.sh" "$BK" >"$W/restore.log" 2>&1; tail -3 "$W/restore.log"
for _ in $(seq 1 50); do curl -sf "$URL/status.php" >/dev/null && break; sleep 0.3; done

echo "== verify"
check "restore.sh checked the backup and the index" grep -q "added 0 updated 0 removed 0" "$W/restore.log"
check "every data file is byte-identical" test "$before" = "$(tree_sums "$CD/data")"
check "index matches the restored disk" bash -c "[[ \"\$(docker exec -i $C rustbucket scan alice --dry-run 2>&1)\" == *'added 0 updated 0 removed 0'* ]]"
check "alice logs in with her old password" test "$(api alice pw-alice -o /dev/null -w '%{http_code}' "$URL/ocs/v2.php/cloud/user")" = 200
check "her profile (email) came back" bash -c "api() { curl -s -u alice:pw-alice -H 'OCS-APIRequest: true' \"\$@\"; }; api '$URL/ocs/v2.php/cloud/user?format=json' | grep -q 'alice@example.org'"
check "the share survived: bob still sees the folder" test "$(curl -s -u bob:pw-bob "$URL/remote.php/dav/files/bob/shared/hello.txt")" = "for bob"
check "the trash survived" bash -c "curl -s -u alice:pw-alice -X PROPFIND -H 'Depth: 1' '$URL/remote.php/dav/trashbin/alice/trash/' | grep -q 'trashed.txt.d'"
fid="$(curl -s -u alice:pw-alice -X PROPFIND -H 'Depth: 0' --data '<d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns"><d:prop><oc:fileid/></d:prop></d:propfind>' "$D/docs/versioned.bin" | grep -o '<oc:fileid>[0-9]*' | grep -o '[0-9]*')"
vers_after="$(curl -s -u alice:pw-alice -X PROPFIND -H 'Depth: 1' "$URL/remote.php/dav/versions/alice/versions/$fid" | grep -o '<d:href>' | wc -l)"
check "versions survived ($vers_before == $vers_after)" test "$vers_before" = "$vers_after"
mkdir -p "$W/client" "$W/conf"
timeout 600 nextcloudcmd --silent --non-interactive --confdir "$W/conf" -u alice -p pw-alice "$W/client" "$URL" >/dev/null 2>&1
check "a fresh client downloads the whole tree identically" bash -c "diff <(cd '$CD/data/alice/files' && find . -type f ! -name '.sync_*' -print0 | LC_ALL=C sort -z | xargs -0 sha256sum) <(cd '$W/client' && find . -type f ! -name '.sync_*' ! -name '._sync_*' -print0 | LC_ALL=C sort -z | xargs -0 sha256sum) >/dev/null"
echo; [[ $fail == 0 ]] && echo "BACKUP/RESTORE OK" || echo "BACKUP/RESTORE FAILED"; exit $fail
