#!/usr/bin/env bash
# End-to-end sync test with the real desktop client CLI (nextcloudcmd) against a throwaway local
# rustbucket container. Touches nothing outside its own temp dir (own --confdir, own ports).
#
#   scripts/e2e.sh [--keep] [--no-build]
# Env: N=10000 (small files)  PORT=18480  IMAGE=rustbucket:e2e
#
# Covers: many small files up, second client down, chunked upload (40 MB), size limit (60 MB
# refused, limit set to 50 MB here), conflict (simultaneous edit; the client keeps a local conflict copy), rename/move/delete, edits,
# index == disk (scan --dry-run), no orphaned upload folders.
set -uo pipefail

N="${N:-10000}"
PORT="${PORT:-18480}"
IMAGE="${IMAGE:-rustbucket:e2e}"
KEEP=0; BUILD=1
for a in "$@"; do case "$a" in --keep) KEEP=1 ;; --no-build) BUILD=0 ;; *) echo "unknown $a" >&2; exit 2 ;; esac; done

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
W="$(mktemp -d "${TMPDIR:-/tmp}/rb-e2e.XXXXXX")"
C="rb-e2e-$$"
URL="http://127.0.0.1:$PORT"
USERNAME=alice; PW="e2e-$(head -c6 /dev/urandom | base64 | tr -dc 'a-z0-9')"
BOBPW="e2e-$(head -c6 /dev/urandom | base64 | tr -dc 'a-z0-9')"
SRV="$W/data/$USERNAME/files"
fail=0
FAILS=()

cleanup() {
  if [[ "$KEEP" == 1 ]]; then echo "kept: $W (container $C)"; return; fi
  docker rm -f "$C" >/dev/null 2>&1; rm -rf -- "$W"
}
trap cleanup EXIT

ok()   { echo "  ok   $*"; }
bad()  { echo "  FAIL $*"; fail=1; FAILS+=("$*"); }
step() { echo; echo "== $(date +%T) $*"; }
check() { local d="$1"; shift; if "$@"; then ok "$d"; else bad "$d"; fi; }

# the client's own bookkeeping files are not part of the synced tree
listing() { ( cd "$1" && find . -type f ! -name '.sync_*.db*' ! -name '._sync_*' ! -name '.csync_journal*' ! -name '*(conflicted copy*' -print0 | LC_ALL=C sort -z | xargs -0 -r sha256sum | sed 's/  \.\//  /' ; find . -mindepth 1 -type d -empty | LC_ALL=C sort | sed 's/^\.\//D /' ); }
same() { diff <(listing "$1") <(listing "$2") >/dev/null; }

sync() {  # sync <localdir> <confdir> [user password]
  mkdir -p "$1" "$2"
  local u="${3:-$USERNAME}" pw="${4:-$PW}"
  # small initial chunk size so 40-60 MB files take the chunked path (the client default is ~100 MB)
  OWNCLOUD_CHUNK_SIZE=5000000 timeout 1500 nextcloudcmd --silent --non-interactive --confdir "$2" -u "$u" -p "$pw" "$1" "$URL" >"$2/out.log" 2>&1
  local rc=$?
  [[ $rc -ne 0 ]] && echo "  (nextcloudcmd exit $rc, see $2/out.log)"
  return 0
}
rb() { docker exec -i "$C" rustbucket "$@"; }
scan_clean() { [[ "$(rb scan "$USERNAME" --dry-run 2>&1)" == *"added 0 updated 0 removed 0"* ]]; }
no_orphans() { [[ -z "$(ls -A "$W/data/$USERNAME/uploads" 2>/dev/null)" && -z "$(ls -A "$W/data/$USERNAME/tmp" 2>/dev/null)" ]]; }

# --- server ---
if [[ "$BUILD" == 1 ]]; then step "build image $IMAGE"; docker build -q --target server -t "$IMAGE" "$ROOT" >/dev/null || { echo "build failed"; exit 1; }; fi
step "start server on $URL"
mkdir -p "$W/db" "$W/data" "$W/A" "$W/B"
docker run -d --name "$C" --user "$(id -u):$(id -g)" -p "127.0.0.1:$PORT:8080" \
  -v "$W/db:/db" -v "$W/data:/data" -e RUSTBUCKET_MAX_FILE_SIZE=52428800 "$IMAGE" >/dev/null || exit 1
for i in $(seq 1 50); do curl -sf "$URL/status.php" >/dev/null && break; sleep 0.3; done
curl -sf "$URL/status.php" >/dev/null || { docker logs "$C" | tail; echo "server did not start"; exit 1; }
printf '%s\n' "$PW" | rb user create "$USERNAME" >/dev/null || exit 1
printf '%s\n' "$BOBPW" | rb user create bob >/dev/null || exit 1

# --- 1. many small files ---
step "1. $N small files, client A -> server"
python3 - "$W/A" "$N" <<'EOF'
import os, random, sys
root, n = sys.argv[1], int(sys.argv[2])
rnd = random.Random(42)
for i in range(n):
    d = os.path.join(root, "bulk", f"d{i % 100:03d}", f"s{(i // 100) % 10}")
    os.makedirs(d, exist_ok=True)
    with open(os.path.join(d, f"file-{i:05d} äöü.txt"), "wb") as f:
        f.write(rnd.randbytes(rnd.choice([0, 1, 10, 500, 2000])))
EOF
t0=$SECONDS; sync "$W/A" "$W/confA"; echo "  took $((SECONDS - t0)) s"
check "server tree == client A ($N files)" same "$W/A" "$SRV"
check "server file count" test "$(find "$SRV/bulk" -type f | wc -l)" -eq "$N"
check "index matches disk" scan_clean

step "2. client B (empty) downloads everything"
t0=$SECONDS; sync "$W/B" "$W/confB"; echo "  took $((SECONDS - t0)) s"
check "client B == server" same "$W/B" "$SRV"

# --- 3. chunked upload ---
step "3. 40 MB file (chunked upload v2) and a 60 MB file over the 50 MB limit"
head -c $((40*1024*1024)) /dev/urandom > "$W/A/chunked-40MB.bin"
head -c $((60*1024*1024)) /dev/urandom > "$W/A/toolarge-60MB.bin"
sync "$W/A" "$W/confA"
check "40 MB file identical on server" cmp -s "$W/A/chunked-40MB.bin" "$SRV/chunked-40MB.bin"
check "60 MB file refused (not on server)" test ! -e "$SRV/toolarge-60MB.bin"
check "no orphaned uploads/tmp" no_orphans
docker logs "$C" 2>&1 | grep -q '/dav/uploads/' && ok "client used /remote.php/dav/uploads" || bad "client did not use chunked upload endpoints"
rm -f "$W/A/toolarge-60MB.bin"; sync "$W/A" "$W/confA"

# --- 4. edits, renames, moves, deletes ---
step "4. edit 100 files, rename, move, delete"
python3 - "$W/A" <<'EOF'
import os, random, sys
root = sys.argv[1]
rnd = random.Random(7)
files = sorted(os.path.join(dp, f) for dp, _, fs in os.walk(os.path.join(root, "bulk")) for f in fs)
for p in rnd.sample(files, 100):
    with open(p, "ab") as f: f.write(b"edited\n")
for p in rnd.sample(files, 50): os.remove(p)
os.rename(os.path.join(root, "bulk", "d001"), os.path.join(root, "bulk", "renamed d001 ü"))
os.makedirs(os.path.join(root, "moved"), exist_ok=True)
os.rename(os.path.join(root, "bulk", "d002"), os.path.join(root, "moved", "d002"))
EOF
sync "$W/A" "$W/confA"
check "server == A after edits/renames/moves/deletes" same "$W/A" "$SRV"
sync "$W/B" "$W/confB"
check "B == server" same "$W/B" "$SRV"
check "index matches disk" scan_clean

# --- 5. conflict ---
step "5. simultaneous edit of one file on A and B"
mkdir -p "$W/A/docs"; printf 'base\n' > "$W/A/docs/shared.txt"
sync "$W/A" "$W/confA"; sync "$W/B" "$W/confB"
printf 'edit from A\n' > "$W/A/docs/shared.txt"; sleep 1
printf 'edit from B, which is longer\n' > "$W/B/docs/shared.txt"
sync "$W/A" "$W/confA"           # A wins the race
sync "$W/B" "$W/confB"           # B must get A's version and keep its own as conflict copy
sync "$W/B" "$W/confB"           # the desktop client does not upload conflict copies by default
check "server has A's version" test "$(cat "$SRV/docs/shared.txt")" = "edit from A"
check "B has A's version" test "$(cat "$W/B/docs/shared.txt")" = "edit from A"
conf="$(find "$W/B/docs" -name 'shared*conflict*' | head -1)"
check "B kept its own edit as a conflict copy" test -n "$conf" -a "$(cat "$conf" 2>/dev/null)" = "edit from B, which is longer"
check "server untouched by B's conflict (no copy, A's version intact)" test "$(find "$SRV/docs" -type f | wc -l)" -eq 1 -a "$(cat "$SRV/docs/shared.txt")" = "edit from A"
sync "$W/A" "$W/confA"
check "A == B == server at the end" bash -c "$(declare -f listing same); same '$W/A' '$SRV' && same '$W/B' '$SRV'"
check "index matches disk" scan_clean
check "no orphaned uploads/tmp" no_orphans


# --- 6. sharing between two users (alice -> bob), both with the real client ---
step "6. sharing: alice shares folders with bob; both sync"
share() {  # share <path> <permissions>   (alice -> bob)
  curl -s -u "$USERNAME:$PW" -H 'OCS-APIRequest: true' -X POST "$URL/ocs/v2.php/apps/files_sharing/api/v1/shares?format=json" \
    -d shareType=0 -d shareWith=bob -d "path=$1" -d "permissions=$2" | grep -q '"statuscode":200'
}
BOBSRV="$W/data/bob/files"; BOBDIR="$W/BOB"; mkdir -p "$BOBDIR"
bsync() { sync "$BOBDIR" "$W/confBOB" bob "$BOBPW"; }
mkdir -p "$W/A/shared-rw/sub" "$W/A/shared-ro"
echo "rw one" > "$W/A/shared-rw/a.txt"; echo "rw two" > "$W/A/shared-rw/b.txt"; echo "deep" > "$W/A/shared-rw/sub/deep.txt"
echo "ro one" > "$W/A/shared-ro/r.txt"; head -c 2000000 /dev/urandom > "$W/A/shared-ro/random.bin"
sync "$W/A" "$W/confA"
echo "bob's own file" > "$BOBDIR/mine.txt"; bsync          # bob exists, nothing shared yet
share /shared-rw 15 && ok "share rw created" || bad "share rw create"
share /shared-ro 1  && ok "share ro created" || bad "share ro create"
bsync
check "bob got the rw share" test "$(cat "$BOBDIR/shared-rw/a.txt" 2>/dev/null)" = "rw one" -a -f "$BOBDIR/shared-rw/sub/deep.txt"
check "bob got the ro share (2 MB file identical)" cmp -s "$W/A/shared-ro/random.bin" "$BOBDIR/shared-ro/random.bin"
check "bob's own file untouched on the server" test "$(cat "$BOBSRV/mine.txt")" = "bob's own file"
check "mounts are not copied into bob's folder on the server" test ! -e "$BOBSRV/shared-rw"

step "6a. bob edits, adds, renames and deletes inside the rw share"
echo "bob changed a" > "$BOBDIR/shared-rw/a.txt"; echo "new from bob" > "$BOBDIR/shared-rw/from-bob.txt"
mv "$BOBDIR/shared-rw/b.txt" "$BOBDIR/shared-rw/b-renamed.txt"; rm "$BOBDIR/shared-rw/sub/deep.txt"
bsync
check "alice's server tree has bob's changes" bash -c "[ \"\$(cat '$SRV/shared-rw/a.txt')\" = 'bob changed a' ] && [ -f '$SRV/shared-rw/from-bob.txt' ] && [ -f '$SRV/shared-rw/b-renamed.txt' ] && [ ! -e '$SRV/shared-rw/b.txt' ] && [ ! -e '$SRV/shared-rw/sub/deep.txt' ]"
sync "$W/A" "$W/confA"
check "alice's client got them too" test "$(cat "$W/A/shared-rw/a.txt")" = "bob changed a" -a -f "$W/A/shared-rw/from-bob.txt"

step "6b. alice edits; bob picks it up through his root etag alone"
echo "alice again" > "$W/A/shared-rw/from-alice.txt"; echo "alice edit" > "$W/A/shared-rw/from-bob.txt"
sync "$W/A" "$W/confA"; bsync
check "bob has alice's new file and edit" test "$(cat "$BOBDIR/shared-rw/from-alice.txt")" = "alice again" -a "$(cat "$BOBDIR/shared-rw/from-bob.txt")" = "alice edit"

step "6c. read-only share: bob's local changes never reach the server"
ro_before="$(listing "$SRV/shared-ro")"
# the client makes read-only shares read-only on disk (it reads oc:permissions): prove that, then force the issue
[[ ! -w "$BOBDIR/shared-ro/r.txt" ]] && ok "client keeps the read-only share read-only locally" || bad "ro share files are writable locally"
chmod -R u+w "$BOBDIR/shared-ro"
echo "bob tamper" > "$BOBDIR/shared-ro/r.txt"; echo "bob new" > "$BOBDIR/shared-ro/new-by-bob.txt"; rm "$BOBDIR/shared-ro/random.bin"
bsync; bsync
check "alice's read-only folder is byte-identical" test "$ro_before" = "$(listing "$SRV/shared-ro")"
check "nothing was created or lost in the owner's tree" test ! -e "$SRV/shared-ro/new-by-bob.txt" -a -f "$SRV/shared-ro/random.bin"
sync "$W/A" "$W/confA"
check "alice's client still has the original files" test "$(cat "$W/A/shared-ro/r.txt")" = "ro one" -a -f "$W/A/shared-ro/random.bin"

step "6d. a share created later shows up for a client whose own files did not change"
mkdir -p "$W/A/late"; echo "late share" > "$W/A/late/l.txt"; sync "$W/A" "$W/confA"
bsync
check "no late folder yet" test ! -e "$BOBDIR/late"
share /late 15 && ok "late share created" || bad "late share create"
bsync
check "bob's client picked up the new mount" test "$(cat "$BOBDIR/late/l.txt" 2>/dev/null)" = "late share"

step "6e. bob deletes a mount locally: it is only unshared, alice keeps everything"
sbefore="$(listing "$SRV/shared-rw")"
rm -r "$BOBDIR/shared-rw"; bsync
check "alice's folder is untouched" test "$sbefore" = "$(listing "$SRV/shared-rw")"
check "bob no longer sees the share" test ! -e "$BOBDIR/shared-rw"
check "the other shares are still there" test -f "$BOBDIR/late/l.txt" -a -f "$BOBDIR/shared-ro/r.txt"
sync "$W/A" "$W/confA"; check "alice's client still has the folder" test -f "$W/A/shared-rw/a.txt"
check "index matches disk (alice)" scan_clean
check "index matches disk (bob)" bash -c "[[ \"\$(docker exec -i '$C' rustbucket scan bob --dry-run 2>&1)\" == *'added 0 updated 0 removed 0'* ]]"
check "no orphaned uploads/tmp" no_orphans

# --- 7. trash bin and versions with the real client ---
step "7. local delete -> server trash -> restore; overwrites -> versions -> restore"
dav() { curl -s -u "$USERNAME:$PW" "$@"; }
TRASH="$URL/remote.php/dav/trashbin/$USERNAME/trash"
trash_hrefs() { dav -X PROPFIND -H 'Depth: 1' --data '<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/></d:prop></d:propfind>' "$TRASH/" | grep -o '<d:href>[^<]*' | sed 's/<d:href>//' | grep -v '/trash/$'; }
mkdir -p "$W/A/trashtest"; echo "keep me" > "$W/A/trashtest/precious.txt"; head -c 300000 /dev/urandom > "$W/A/trashtest/blob.bin"
sync "$W/A" "$W/confA"
rm "$W/A/trashtest/precious.txt"; rm -r "$W/A/trashtest"; sync "$W/A" "$W/confA"
check "client delete reached the server" test ! -e "$SRV/trashtest"
check "both deletions are in the trash (folder entry)" bash -c "$(declare -f trash_hrefs dav); USERNAME=$USERNAME PW=$PW URL=$URL TRASH=$TRASH; trash_hrefs | grep -q 'trashtest.d'"
href="$(trash_hrefs | grep 'trashtest.d' | head -1)"
check "trashed content is still readable" test "$(dav "$URL${href}precious.txt")" = "keep me"
code="$(dav -o /dev/null -w '%{http_code}' -X MOVE -H "Destination: $URL/remote.php/dav/trashbin/$USERNAME/restore/x" "$URL$href")"
check "restore answers 201" test "$code" = "201"
check "restored on the server" test "$(cat "$SRV/trashtest/precious.txt")" = "keep me" -a -f "$SRV/trashtest/blob.bin"
sync "$W/A" "$W/confA"
check "the client got the restored folder back" test "$(cat "$W/A/trashtest/precious.txt" 2>/dev/null)" = "keep me" -a -f "$W/A/trashtest/blob.bin"
check "the restored folder left the trash" bash -c "$(declare -f trash_hrefs dav); USERNAME=$USERNAME PW=$PW URL=$URL TRASH=$TRASH; ! trash_hrefs | grep -q 'trashtest.d'"

echo "generation 1" > "$W/A/versioned.txt"; touch -d '2024-01-01 10:00:00' "$W/A/versioned.txt"; sync "$W/A" "$W/confA"
echo "generation 2, longer" > "$W/A/versioned.txt"; touch -d '2024-01-01 11:00:00' "$W/A/versioned.txt"; sync "$W/A" "$W/confA"
echo "generation 3" > "$W/A/versioned.txt"; touch -d '2024-01-01 12:00:00' "$W/A/versioned.txt"; sync "$W/A" "$W/confA"
fid="$(dav -X PROPFIND -H 'Depth: 0' --data '<d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns"><d:prop><oc:fileid/></d:prop></d:propfind>' "$URL/remote.php/dav/files/$USERNAME/versioned.txt" | grep -o '<oc:fileid>[0-9]*' | grep -o '[0-9]*')"
VERS="$URL/remote.php/dav/versions/$USERNAME/versions/$fid"
nvers="$(dav -X PROPFIND -H 'Depth: 1' --data '<d:propfind xmlns:d="DAV:"><d:prop><d:getcontentlength/></d:prop></d:propfind>' "$VERS" | grep -o '<d:href>' | wc -l)"
echo "  (file id $fid; version entries incl. the collection: $nvers)"
dav -X PROPFIND -H 'Depth: 1' --data '<d:propfind xmlns:d="DAV:"><d:prop><d:getcontentlength/></d:prop></d:propfind>' "$VERS" | grep -o '<d:href>[^<]*' | sed 's/^/    /'
check "three generations = at least 3 versions (+ the collection)" test "$nvers" -ge 4
oldest="$(dav -X PROPFIND -H 'Depth: 1' --data '<d:propfind xmlns:d="DAV:"><d:prop><d:getcontentlength/></d:prop></d:propfind>' "$VERS" | grep -o '<d:href>[^<]*' | sed 's/<d:href>//' | sed -n 2p)"
check "oldest version has the first content" test "$(dav "$URL$oldest")" = "generation 1"
code="$(dav -o /dev/null -w '%{http_code}' -X MOVE -H "Destination: $URL/remote.php/dav/versions/$USERNAME/restore/x" "$URL$oldest")"
check "restore of the oldest version answers 201" test "$code" = "201"
sync "$W/A" "$W/confA"; sync "$W/B" "$W/confB"
check "client A has the restored content" test "$(cat "$W/A/versioned.txt")" = "generation 1"
check "client B got it too" test "$(cat "$W/B/versioned.txt")" = "generation 1"
check "index matches disk" scan_clean
check "no orphaned uploads/tmp" no_orphans

# --- 8. group share: a member added LATER gets access (UAT item 11) ---
step "8. group share with a late member"
DAVEPW="e2e-$(head -c6 /dev/urandom | base64 | tr -dc 'a-z0-9')"; DAVEDIR="$W/DAVE"; mkdir -p "$DAVEDIR"
printf '%s\n' "$DAVEPW" | rb user create dave >/dev/null
rb group create family >/dev/null; rb group add-member family bob >/dev/null
mkdir -p "$W/A/fam"; echo "for the family" > "$W/A/fam/note.txt"; sync "$W/A" "$W/confA"
curl -s -u "$USERNAME:$PW" -H 'OCS-APIRequest: true' -X POST "$URL/ocs/v2.php/apps/files_sharing/api/v1/shares?format=json" \
  -d shareType=1 -d shareWith=family -d path=/fam -d permissions=15 | grep -q '"statuscode":200' && ok "group share created" || bad "group share create"
bsync
check "a member (bob) gets it" test "$(cat "$BOBDIR/fam/note.txt" 2>/dev/null)" = "for the family"
sync "$DAVEDIR" "$W/confDAVE" dave "$DAVEPW"
check "a non-member (dave) does not" test ! -e "$DAVEDIR/fam"
rb group add-member family dave >/dev/null
sync "$DAVEDIR" "$W/confDAVE" dave "$DAVEPW"
check "dave, added to the group later, gets it on her next sync" test "$(cat "$DAVEDIR/fam/note.txt" 2>/dev/null)" = "for the family"
echo "from dave" > "$DAVEDIR/fam/dave.txt"; sync "$DAVEDIR" "$W/confDAVE" dave "$DAVEPW"
check "dave can write into it (read-write share)" test "$(cat "$SRV/fam/dave.txt" 2>/dev/null)" = "from dave"
sync "$W/A" "$W/confA"; bsync
check "the owner and the other member see dave's file" test "$(cat "$W/A/fam/dave.txt" 2>/dev/null)" = "from dave" -a "$(cat "$BOBDIR/fam/dave.txt" 2>/dev/null)" = "from dave"
check "index matches disk (dave)" bash -c "[[ \"\$(docker exec -i '$C' rustbucket scan dave --dry-run 2>&1)\" == *'added 0 updated 0 removed 0'* ]]"
check "no orphaned uploads/tmp" no_orphans
echo
if [[ "$fail" == 0 ]]; then echo "E2E OK"; else echo "E2E FAILED:"; printf '  - %s\n' "${FAILS[@]}"; fi
exit "$fail"
