#!/usr/bin/env bash
# UAT item 8 with the real client: a large upload is interrupted (client killed mid-transfer = network cut) and,
# when the client runs again, it RESUMES (does not start over) and the file arrives intact; the aborted upload
# leaves nothing behind. Throwaway local server.   scripts/e2e_resume.sh [--no-build]
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE="${IMAGE:-rustbucket:e2e}"; PORT="${PORT:-18484}"; URL="http://127.0.0.1:$PORT"
W="$(mktemp -d "${TMPDIR:-/tmp}/rb-resume.XXXXXX")"; C="rb-resume-$$"; fail=0
ok() { echo "  ok   $*"; }; bad() { echo "  FAIL $*"; fail=1; }
cleanup() { docker rm -f "$C" >/dev/null 2>&1; rm -rf -- "$W"; }; trap cleanup EXIT
[[ "${1:-}" == --no-build ]] || docker build -q --target server -t "$IMAGE" "$ROOT" >/dev/null || exit 1
mkdir -p "$W/db" "$W/data" "$W/client" "$W/conf"
docker run -d --name "$C" --user "$(id -u):$(id -g)" -p "127.0.0.1:$PORT:8080" -v "$W/db:/db" -v "$W/data:/data" -e RUSTBUCKET_MAX_FILE_SIZE=0 "$IMAGE" >/dev/null
for _ in $(seq 1 50); do curl -sf "$URL/status.php" >/dev/null && break; sleep 0.3; done
printf 'pw\n' | docker exec -i "$C" rustbucket user create alice >/dev/null
MB=300; CHUNK=10000000
head -c $((MB * 1024 * 1024)) /dev/urandom > "$W/client/big.bin"
want="$(sha256sum "$W/client/big.bin" | cut -d' ' -f1)"
sync() { OWNCLOUD_CHUNK_SIZE=$CHUNK OWNCLOUD_MAX_CHUNK_SIZE=$CHUNK OWNCLOUD_MIN_CHUNK_SIZE=$CHUNK nextcloudcmd --silent --non-interactive --confdir "$W/conf" "$@" -u alice -p pw "$W/client" "$URL"; }
chunk_puts() { docker logs "$C" 2>&1 | python3 -c '
import sys, json
n = 0
for l in sys.stdin:
    try: j = json.loads(l)
    except Exception: continue
    s = j.get("span", {}); f = j.get("fields", {})
    if s.get("method") == "PUT" and "/dav/uploads/" in s.get("uri", "") and f.get("status") == 201: n += 1
print(n)'; }

echo "== run 1: slow link, cut after ~12 s"
sync --uplimit 15000 >/dev/null 2>&1 &
pid=$!; sleep 12; kill -9 "$pid" 2>/dev/null; pkill -9 -f "nextcloudcmd.*$W/conf" 2>/dev/null; wait "$pid" 2>/dev/null
first="$(chunk_puts)"; echo "  chunks received before the cut: $first (of $(( (MB * 1024 * 1024 + CHUNK - 1) / CHUNK )))"
check() { local d="$1"; shift; if "$@"; then ok "$d"; else bad "$d"; fi; }
check "some chunks arrived, the file did not" bash -c "[[ $first -ge 1 && ! -e '$W/data/alice/files/big.bin' ]]"
check "the unfinished upload is kept on the server for resuming" bash -c "ls '$W/data/alice/uploads'/*/ >/dev/null 2>&1"

echo "== run 2: link is back"
t0=$SECONDS; sync >/dev/null 2>&1; echo "  took $((SECONDS - t0)) s"
total="$(chunk_puts)"; second=$((total - first)); echo "  chunks sent in run 2: $second"
check "the file arrived intact" test "$(sha256sum "$W/data/alice/files/big.bin" 2>/dev/null | cut -d' ' -f1)" = "$want"
check "run 2 resumed instead of starting over (fewer chunks than the whole file)" test "$second" -lt $(( (MB * 1024 * 1024 + CHUNK - 1) / CHUNK ))
check "no orphaned upload folder afterwards" bash -c "[[ -z \"\$(ls -A '$W/data/alice/uploads' 2>/dev/null)\" && -z \"\$(ls -A '$W/data/alice/tmp' 2>/dev/null)\" ]]"
check "index matches disk" bash -c "[[ \"\$(docker exec $C rustbucket scan alice --dry-run 2>&1)\" == *'added 0 updated 0 removed 0'* ]]"
echo; [[ $fail == 0 ]] && echo "RESUME OK" || echo "RESUME FAILED"; exit $fail
