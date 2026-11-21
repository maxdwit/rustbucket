#!/usr/bin/env bash
# Smoke test of the standalone image (server + nginx + self-signed certificate in ONE container):
# build, start, probe over TLS (certificate verified against the generated one), WebDAV incl. a streamed 30 MB
# upload, restart keeps the certificate, backup -> purge -> restore round trip. Everything is throw-away.
#   scripts/smoke.sh            (make smoke)        Env: PORT=18443  IMAGE=rustbucket:smoke  KEEP=1 (leave the container)
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
IMAGE="${IMAGE:-rustbucket:smoke}"; PORT="${PORT:-18443}"; N="rbsmoke-$$"
W="$(mktemp -d "${TMPDIR:-/tmp}/rb-smoke.XXXXXX")"; fail=0
URL="https://localhost:$PORT"; PW="smoke-$(head -c6 /dev/urandom | base64 | tr -dc 'a-z0-9')"
ok() { echo "  ok   $*"; }; bad() { echo "  FAIL $*"; fail=1; }
check() { local d="$1"; shift; if "$@" >/dev/null 2>&1; then ok "$d"; else bad "$d"; fi; }
cleanup() {
  [[ "${KEEP:-0}" == 1 ]] && { echo "kept container $N (https://localhost:$PORT, admin password $PW)"; return; }
  docker rm -f "$N" >/dev/null 2>&1; docker volume rm "$N-data" "$N-db" "$N-certs" >/dev/null 2>&1
  rm -rf -- "$W"
}
trap cleanup EXIT
cv() { curl -sS --max-time 60 --cacert "$W/tls.crt" "$@"; }   # verifies the certificate, no -k
wait_healthy() { for _ in $(seq 1 120); do [[ "$(docker inspect -f '{{.State.Health.Status}}' "$N" 2>/dev/null)" == healthy ]] && return 0; sleep 1; done; return 1; }
fp() { docker exec "$N" openssl x509 -noout -fingerprint -sha256 -in /certs/tls.crt | cut -d= -f2; }

echo "== build $IMAGE"
docker build -q -t "$IMAGE" . >/dev/null || { echo "build failed"; exit 1; }
echo "== start"
docker run -d --name "$N" -p "127.0.0.1:$PORT:8443" -v "$N-data:/data" -v "$N-db:/db" -v "$N-certs:/certs" \
  -e RB_ADMIN_PASSWORD="$PW" -e RB_TLS_NAMES=localhost,127.0.0.1 "$IMAGE" >/dev/null || exit 1
check "container becomes healthy (nginx -> server)" wait_healthy
docker cp "$N:/certs/tls.crt" "$W/tls.crt" >/dev/null
check "the private key is readable by its owner only (mode 600)" test "$(docker exec "$N" stat -c %a /certs/tls.key)" = 600
check "runs unprivileged" test "$(docker exec "$N" id -u)" = 10001
check "only port 8443 is exposed by the image" test "$(docker inspect -f '{{range $p, $_ := .Config.ExposedPorts}}{{$p}} {{end}}' "$N")" = "8443/tcp "
check "the server itself listens on loopback only (not reachable from outside the container)" bash -c "[ \"\$(docker exec $N sh -c 'echo \$RUSTBUCKET_LISTEN')\" = 127.0.0.1:8080 ]"

echo "== over TLS (certificate verified)"
check "status.php says Nextcloud 35" bash -c "$(declare -f cv); W='$W'; PORT=$PORT; cv $URL/status.php | grep -q '\"versionstring\":\"35\.'"
check "capabilities" bash -c "$(declare -f cv); W='$W'; cv -H 'OCS-APIRequest: true' '$URL/ocs/v2.php/cloud/capabilities?format=json' | grep -q '\"statuscode\":200'"
check "login flow v2 advertises the public https URL incl. port" bash -c "$(declare -f cv); W='$W'; cv -X POST $URL/index.php/login/v2 | grep -q '\"endpoint\":\"https://localhost:$PORT/index.php/login/v2/poll\"'"
check "wrong password is rejected (401)" test "$(cv -o /dev/null -w '%{http_code}' -u "admin:nope" "$URL/remote.php/dav/files/admin/")" = 401
check "the administrator from RB_ADMIN_PASSWORD can log in" test "$(cv -o /dev/null -w '%{http_code}' -X PROPFIND -u "admin:$PW" "$URL/remote.php/dav/files/admin/")" = 207
check "security headers present" bash -c "$(declare -f cv); W='$W'; cv -sI $URL/login | grep -qi '^x-content-type-options: nosniff'"
check "nginx does not announce its version" bash -c "$(declare -f cv); W='$W'; ! cv -sI $URL/login | grep -qi '^server: nginx/'"

echo "== WebDAV through nginx"
DAV="$URL/remote.php/dav/files/admin"
head -c 30000000 /dev/urandom > "$W/big.bin"; want="$(sha256sum "$W/big.bin" | cut -d' ' -f1)"
check "MKCOL with an umlaut name" test "$(cv -o /dev/null -w '%{http_code}' -X MKCOL -u "admin:$PW" "$DAV/Fotos%20%C3%84%C3%B6/")" = 201
check "PUT 30 MB is streamed through nginx (no body-size limit)" test "$(cv -o /dev/null -w '%{http_code}' -T "$W/big.bin" -u "admin:$PW" "$DAV/Fotos%20%C3%84%C3%B6/big.bin")" = 201
check "GET returns the same bytes" test "$(cv -u "admin:$PW" "$DAV/Fotos%20%C3%84%C3%B6/big.bin" | sha256sum | cut -d' ' -f1)" = "$want"
check "MOVE (Destination header with the public URL)" test "$(cv -o /dev/null -w '%{http_code}' -X MOVE -u "admin:$PW" -H "Destination: $DAV/moved.bin" "$DAV/Fotos%20%C3%84%C3%B6/big.bin")" = 201
check "PROPFIND depth 1 lists it" bash -c "$(declare -f cv); W='$W'; cv -X PROPFIND -H 'Depth: 1' -u 'admin:$PW' $DAV/ | grep -q 'moved.bin'"

echo "== restart keeps certificate and data, creates no second administrator"
before="$(fp)"; docker restart "$N" >/dev/null; wait_healthy
check "same certificate after a restart" test "$(fp)" = "$before"
check "data still there" test "$(cv -u "admin:$PW" "$DAV/moved.bin" | sha256sum | cut -d' ' -f1)" = "$want"
check "first-start message is not repeated" bash -c "[ \"\$(docker logs $N 2>&1 | grep -c 'first start: created administrator')\" = 1 ]"
docker rm -f "$N" >/dev/null
docker run -d --name "$N" -p "127.0.0.1:$PORT:8443" -v "$N-data:/data" -v "$N-db:/db" -v "$N-certs:/certs" \
  -e RB_TLS_NAMES=localhost,127.0.0.1,smoke.example.com "$IMAGE" >/dev/null; wait_healthy
check "a changed RB_TLS_NAMES renews the certificate" bash -c "docker exec $N openssl x509 -noout -ext subjectAltName -in /certs/tls.crt | grep -q smoke.example.com"
check "the data volume survived the container being recreated" test "$(docker exec "$N" cat /data/admin/files/moved.bin | sha256sum | cut -d' ' -f1)" = "$want"
docker cp "$N:/certs/tls.crt" "$W/tls.crt" >/dev/null

echo "== backup -> purge -> restore"
MK=(make --no-print-directory NAME="$N" IMAGE="$IMAGE" PORT="$PORT" DEST="$W/backups")
check "make backup" "${MK[@]}" backup
arc="$(ls "$W"/backups/rustbucket-*.tar.gz 2>/dev/null | head -1)"
check "the archive holds the database snapshot and the file" bash -c "tar -tzf '$arc' | grep -qx 'db/rustbucket.db' && tar -tzf '$arc' | grep -q 'data/admin/files/moved.bin'"
check "make purge CONFIRM=1" "${MK[@]}" purge CONFIRM=1
check "make restore into the empty volumes" "${MK[@]}" restore FILE="$arc"
wait_healthy
docker cp "$N:/certs/tls.crt" "$W/tls.crt" >/dev/null
check "after the restore the same admin logs in and the file is intact" test "$(cv -u "admin:$PW" "$DAV/moved.bin" | sha256sum | cut -d' ' -f1)" = "$want"
check "restore refuses to run over existing data" bash -c "! ${MK[*]} restore FILE='$arc' >/dev/null 2>&1"

# last, because it locks this peer out for a while: nginx must overwrite X-Forwarded-For with the real peer, so
# forged addresses cannot dodge the login throttle (5 failures per 10 minutes)
echo "== login throttle cannot be dodged with a forged X-Forwarded-For"
codes=""; for i in 1 2 3 4 5 6 7 8; do codes="$codes $(cv -o /dev/null -w '%{http_code}' -u "ghost:nope" -H "X-Forwarded-For: 198.51.100.$i" "$DAV/")"; done
echo "  responses:$codes"
check "a 429 shows up despite eight different forged addresses" bash -c "[[ '$codes' == *429* ]]"

echo; [[ $fail == 0 ]] && echo "SMOKE OK" || echo "SMOKE FAILED"; exit $fail
