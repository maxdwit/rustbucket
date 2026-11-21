#!/usr/bin/env bash
# Soak test: CLIENTS (default 10) real nextcloudcmd clients, two per user, hammer one throwaway server for
# DURATION seconds (default 3600) with random creates/edits/renames/deletes/moves (small files, a few 30-60 MB
# chunked uploads, a folder shared between users, one deliberately contended file). Then everything must converge:
# every client's tree == the server tree, index == disk, no orphaned uploads, no 5xx, no panic, and the server's
# memory must not keep growing.
#   scripts/soak.sh [DURATION_SECONDS]      Env: CLIENTS=10  PORT=18483  IMAGE=rustbucket:e2e  NOBUILD=1
set -uo pipefail
DURATION="${1:-3600}"; CLIENTS="${CLIENTS:-10}"; PORT="${PORT:-18483}"; IMAGE="${IMAGE:-rustbucket:e2e}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"; URL="http://127.0.0.1:$PORT"
W="$(mktemp -d "${TMPDIR:-/tmp}/rb-soak.XXXXXX")"; C="rb-soak-$$"; NUSERS=$(( (CLIENTS + 1) / 2 ))
fail=0; ok() { echo "  ok   $*"; }; bad() { echo "  FAIL $*"; fail=1; }
cleanup() { kill $(jobs -p) 2>/dev/null; docker rm -f "$C" >/dev/null 2>&1; [[ "${KEEP:-0}" == 1 ]] && echo "kept $W" || rm -rf -- "$W"; }; trap cleanup EXIT
[[ "${NOBUILD:-0}" == 1 ]] || docker build -q --target server -t "$IMAGE" "$ROOT" >/dev/null || exit 1
mkdir -p "$W/db" "$W/data"
docker run -d --name "$C" --user "$(id -u):$(id -g)" -p "127.0.0.1:$PORT:8080" -v "$W/db:/db" -v "$W/data:/data" -e RUSTBUCKET_MAX_FILE_SIZE=0 -e RUSTBUCKET_VERSIONS_MAX=5 "$IMAGE" >/dev/null
for _ in $(seq 1 60); do curl -sf "$URL/status.php" >/dev/null && break; sleep 0.3; done
rb() { docker exec -i "$C" rustbucket "$@"; }
uname_of() { echo "user$1"; }
for u in $(seq 1 "$NUSERS"); do printf 'pw-%s\n' "$u" | rb user create "$(uname_of "$u")" >/dev/null; done
# every user shares "team" with the next one (read-write): rings of shared folders
for u in $(seq 1 "$NUSERS"); do   # everybody's own folder first, so the mounts get numbered names ("team (2)")
  curl -s -u "$(uname_of "$u"):pw-$u" -X MKCOL "$URL/remote.php/dav/files/$(uname_of "$u")/team" -o /dev/null
done
for u in $(seq 1 "$NUSERS"); do
  n=$(( u % NUSERS + 1 ))
  curl -s -u "$(uname_of "$u"):pw-$u" -H 'OCS-APIRequest: true' -X POST "$URL/ocs/v2.php/apps/files_sharing/api/v1/shares?format=json" \
    -d shareType=0 -d "shareWith=$(uname_of "$n")" -d path=/team -d permissions=15 >/dev/null
done

# ---- client loop ---------------------------------------------------------------------------------------------
sync_client() {  # sync_client <user n> <client letter>
  local u="$1" c="$2" dir="$W/c-$1$2"; mkdir -p "$dir" "$W/conf-$1$2"
  OWNCLOUD_CHUNK_SIZE=5000000 timeout 600 nextcloudcmd --silent --non-interactive --confdir "$W/conf-$1$2" -u "$(uname_of "$u")" -p "pw-$u" "$dir" "$URL" >>"$W/sync-$1$2.log" 2>&1
  local rc=$?; [[ $rc -ne 0 ]] && echo "$(date +%T) exit $rc" >>"$W/errors-$1$2.log"; return 0
}
client() {  # client <user n> <client letter> : random operations until the deadline, syncing in between
  local u="$1" c="$2" dir="$W/c-$1$2" end=$((SECONDS + DURATION)) n=0
  mkdir -p "$dir/own-$c" "$dir/team/$c-$u"
  while (( SECONDS < end )); do
    # stop cleanly instead of failing with ENOSPC when the work disk runs low (everything lives under $W)
    if (( $(df --output=avail -m "$W" | tail -1) < 3000 )); then touch "$W/disklow"; break; fi
    n=$((n+1)); local r=$((RANDOM % 100)) d
    case $((RANDOM % 4)) in
      0) d="$dir/own-$c";;
      1) d="$dir/team/$c-$u";;
      2) if [[ -d "$dir/team (2)" ]]; then d="$dir/team (2)/$c-$u"; else d="$dir/own-$c"; fi;;   # the previous user's share (read-write)
      *) d="$dir/own-$c/sub$((RANDOM % 4))";; esac
    mkdir -p "$d"
    if   (( r < 40 )); then head -c $((RANDOM % 20000)) /dev/urandom > "$d/f$((RANDOM % 30)).dat"
    elif (( r < 55 )); then f="$(ls "$d" 2>/dev/null | shuf -n1)"; [[ -f "$d/$f" ]] && echo "edit $n" >> "$d/$f"
    elif (( r < 65 )); then f="$(ls "$d" 2>/dev/null | shuf -n1)"; [[ -f "$d/$f" ]] && mv "$d/$f" "$d/r$n-${f}"
    elif (( r < 75 )); then f="$(ls "$d" 2>/dev/null | shuf -n1)"; [[ -f "$d/$f" ]] && rm -f "$d/$f"
    elif (( r < 80 )); then mkdir -p "$d/dir$((RANDOM % 5))" && cp -n "$d"/f* "$d/dir$((RANDOM % 5))/" 2>/dev/null
    elif (( r < 83 )); then (( RANDOM % 6 == 0 )) && head -c $((30000000 + RANDOM * 300)) /dev/urandom > "$dir/own-$c/big$((RANDOM % 3)).bin"   # rarely: chunked upload
    elif (( r < 90 )); then echo "$c $n $(date +%s)" > "$dir/hot.txt"            # contended with the other client of this user
    else f="$(ls "$d" 2>/dev/null | shuf -n1)"; [[ -f "$d/$f" ]] && touch -d "@$((1700000000 + RANDOM))" "$d/$f"
    fi
    sync_client "$u" "$c"
    sleep $((2 + RANDOM % 6))
  done
  echo done > "$W/done-$u$c"
}

# ---- memory sampling -----------------------------------------------------------------------------------------
# resident memory of the server process (docker stats also counts the kernel's file cache, which large uploads inflate)
( while [[ ! -e "$W/stop-sampling" ]]; do
    docker exec "$C" sh -c "awk '/VmRSS/ {print \$2/1024}' /proc/1/status" 2>/dev/null >>"$W/mem.log"; sleep 30
  done ) &

echo "== $CLIENTS clients, $NUSERS users, $DURATION s  ($(date +%T))"
i=0; for u in $(seq 1 "$NUSERS"); do for c in a b; do i=$((i+1)); [[ $i -le $CLIENTS ]] && client "$u" "$c" & done; done
wait $(jobs -p | grep -v "^$(jobs -p | head -1)$") 2>/dev/null
while [[ $(ls "$W"/done-* 2>/dev/null | wc -l) -lt $CLIENTS ]]; do sleep 5; done
touch "$W/stop-sampling"

[[ -e "$W/disklow" ]] && bad "the work disk ran low: shorten the run or use a bigger TMPDIR"
echo "== converge ($(date +%T)): three rounds of syncs, nobody editing"
for round in 1 2 3; do
  rm -f "$W"/errors-final-*.log
  for u in $(seq 1 "$NUSERS"); do for c in a b; do [[ -d "$W/c-$u$c" ]] && { sync_client "$u" "$c"; }; done; done
done
listing() { ( cd "$1" && find . -type f ! -name '.sync_*.db*' ! -name '._sync_*' ! -name '*(conflicted copy*' -print0 | LC_ALL=C sort -z | xargs -0 -r sha256sum | sed 's/  \.\//  /' ); }
for u in $(seq 1 "$NUSERS"); do
  srv="$W/data/$(uname_of "$u")/files"
  for c in a b; do
    [[ -d "$W/c-$u$c" ]] || continue
    if diff <(listing "$srv" | grep -v '^[0-9a-f]*  team') <(listing "$W/c-$u$c" | grep -v '^[0-9a-f]*  team') >/dev/null; then ok "user$u client $c == server (own files)"; else bad "user$u client $c differs from the server"; fi
  done
done
# the shared rings: what the next user sees as "team (2)" is exactly the owner's team folder
for u in $(seq 1 "$NUSERS"); do
  n=$(( u % NUSERS + 1 ))
  if [[ -d "$W/c-${n}a/team (2)" ]]; then
    if diff <(listing "$W/data/$(uname_of "$u")/files/team") <(listing "$W/c-${n}a/team (2)") >/dev/null; then ok "user$n's mount of user$u's team folder == owner's tree"; else bad "shared team folder of user$u differs at user$n"; fi
  else bad "user$n never got the share of user$u"; fi
done
for u in $(seq 1 "$NUSERS"); do
  out="$(rb scan "$(uname_of "$u")" --dry-run 2>&1)"
  [[ "$out" == *"added 0 updated 0 removed 0"* ]] && ok "index == disk for user$u" || bad "index differs for user$u: $out"
done
left="$(find "$W/data" -mindepth 3 \( -path "$W/data/*/uploads/*" -o -path "$W/data/*/tmp/*" \) 2>/dev/null | head -3)"
[[ -z "$left" ]] && ok "no orphaned uploads/tmp" || bad "orphans: $left"
logs="$(docker logs "$C" 2>&1)"
n5xx=$(grep -c '"status":5[0-9][0-9]' <<<"$logs"); npanic=$(grep -ci 'panicked' <<<"$logs")
[[ "$n5xx" == 0 ]] && ok "no 5xx responses" || bad "$n5xx responses were 5xx"
[[ "$npanic" == 0 ]] && ok "no panics in the log" || bad "$npanic panics"
nreq=$(grep -c 'finished processing request' <<<"$logs")
echo "  requests served: $nreq; client sync errors during the run: $(cat "$W"/errors-*.log 2>/dev/null | wc -l) (transient: files changing mid-sync)"
# memory: first vs last tenth of the samples
if [[ -s "$W/mem.log" ]]; then
  # steady state: the 30-40 % window (after warm-up) against the last tenth; growth must stay small
  read -r first last max < <(awk '{a[NR]=$1} END {k=int(NR/10); if (k<1) k=1; s=int(NR*0.3); if (s<1) s=1; for(i=s;i<s+k && i<=NR;i++) {f+=a[i]; nf++}; for(i=NR-k+1;i<=NR;i++) l+=a[i]; for(i=1;i<=NR;i++) if (a[i]>m) m=a[i]; printf "%.1f %.1f %.1f\n", f/nf, l/k, m}' "$W/mem.log")
  echo "  server memory (MiB): steady-state window ~$first, last tenth ~$last, peak $max, samples $(wc -l < "$W/mem.log")"
  if [[ $(wc -l < "$W/mem.log") -lt 10 ]]; then echo "  (fewer than 10 memory samples: too short to judge growth)"; elif awk -v f="$first" -v l="$last" 'BEGIN {exit !(l <= f * 1.5 + 40)}'; then ok "memory did not keep growing"; else bad "memory grew from $first to $last MiB"; fi
fi
echo; [[ $fail == 0 ]] && echo "SOAK OK" || echo "SOAK FAILED"; exit $fail
