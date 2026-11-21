#!/usr/bin/env bash
# Coverage-guided fuzzing of the parsers (fuzz/): every target for SECS seconds (default 60).
#   scripts/fuzz.sh [SECS]        needs a nightly toolchain and cargo-fuzz:  rustup toolchain install nightly; cargo install cargo-fuzz
# A crash leaves its input in fuzz/artifacts/<target>/ - turn it into a regression test before fixing.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
SECS="${1:-60}"
for t in propfind search paths config share_params; do
  echo "== fuzz $t for ${SECS}s"
  cargo +nightly fuzz run "$t" -- -max_total_time="$SECS" -rss_limit_mb=2048 2>&1 | tail -4
done
echo "fuzzing finished without a crash"
