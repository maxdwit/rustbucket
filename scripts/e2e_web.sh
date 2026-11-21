#!/usr/bin/env bash
# Browser end-to-end test of the web UI (tests/web_e2e.rs) with the host's chromedriver + chromium.
#
#   scripts/e2e_web.sh
# Env: CARGO="cargo" (command that runs cargo, e.g. a docker wrapper)  PORT=9515
#      CARGO_DIR_IN_CONTAINER=/work  (where the repo is seen by that cargo; only needed with a wrapper)
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PORT="${PORT:-9515}"
CARGO="${CARGO:-cargo}"
UP="$ROOT/.e2e-uploads"; mkdir -p "$UP"
command -v chromedriver >/dev/null && command -v chromium >/dev/null || { echo "need chromedriver and chromium"; exit 2; }
chromedriver --port="$PORT" --silent >"$UP/chromedriver.log" 2>&1 &
CD=$!
trap 'kill $CD 2>/dev/null; rm -f "$UP"/hello.txt "$UP"/chunky.bin' EXIT
for _ in $(seq 1 50); do curl -sf "http://127.0.0.1:$PORT/status" >/dev/null && break; sleep 0.2; done
export WEBDRIVER_URL="http://127.0.0.1:$PORT"
export E2E_UPLOAD_DIR="${CARGO_DIR_IN_CONTAINER:-$ROOT}/.e2e-uploads"
export E2E_UPLOAD_DIR_HOST="$UP"
cd "$ROOT"
$CARGO test --test web_e2e -- --ignored --nocapture --test-threads=1
