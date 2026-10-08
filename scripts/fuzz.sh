#!/usr/bin/env bash
# Long-running fuzzing of all heifer fuzz targets in parallel, on all CPU cores.
#
# Usage:
#   scripts/fuzz.sh            # 8 hours
#   scripts/fuzz.sh 30m        # any duration: 90s, 30m, 8h, 2d
#   scripts/fuzz.sh 2h hevc    # a single target, on all cores
#
# Stop at any time with Ctrl+C. Interesting inputs accumulate in fuzz/corpus/ (reused by the next
# run), crashes are written to fuzz/artifacts/<target>/ and logs to fuzz/logs/.
# Requires: rustup nightly toolchain and cargo-fuzz (cargo install cargo-fuzz).
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$(pwd)

duration=${1:-8h}
only=${2:-}
case "$duration" in
  *d) seconds=$(( ${duration%d} * 86400 )) ;;
  *h) seconds=$(( ${duration%h} * 3600 )) ;;
  *m) seconds=$(( ${duration%m} * 60 )) ;;
  *s) seconds=${duration%s} ;;
  *)  seconds=$duration ;;
esac

command -v cargo-fuzz >/dev/null || { echo "cargo-fuzz missing: cargo install cargo-fuzz"; exit 1; }
rustup toolchain list | grep -q nightly || { echo "nightly missing: rustup toolchain install nightly"; exit 1; }

cores=$(sysctl -n hw.ncpu 2>/dev/null || nproc)
echo "Fuzzing for $duration on $cores cores"

cd fuzz
echo "Building fuzz targets..."
cargo +nightly fuzz build -O -s none

# Seed corpora (existing corpora are kept and grow across runs).
mkdir -p corpus/{decode,container,parallel,hevc,cabac} logs artifacts
seed() { [ -e "$2" ] && find "$2" -type f -size -400k -exec cp -n {} "corpus/$1/" \; 2>/dev/null || true; }
for t in decode container parallel; do
  seed "$t" "$ROOT/tests/fixtures"
  seed "$t" "$ROOT/fuzz/seeds/libheif"
done
[ -d "$ROOT/tests/conformance" ] && find "$ROOT/tests/conformance" \( -name '*.bit' -o -name '*.bin' \) -size -100k \
  -exec cp -n {} corpus/hevc/ \; 2>/dev/null || true
[ -n "$(ls corpus/cabac)" ] || printf '\x1a\x00\x01\x02' > corpus/cabac/seed

# Share cores: the HEVC decoder is the deepest code, the container parser the fastest.
share() { local n=$(( cores * $1 / 10 )); [ -n "$only" ] && n=$cores; [ "$n" -lt 1 ] && n=1; echo "$n"; }
run() {
  local target=$1 jobs=$2; shift 2
  [ -n "$only" ] && [ "$only" != "$target" ] && return
  echo "  $target: $jobs process(es)"
  cargo +nightly fuzz run -O -s none "$target" "corpus/$target" -- \
    -fork="$jobs" -ignore_crashes=1 -ignore_timeouts=1 -ignore_ooms=1 \
    -max_total_time="$seconds" -rss_limit_mb=2048 -timeout=20 \
    -artifact_prefix="artifacts/$target/" "$@" > "logs/$target.log" 2>&1 &
}
mkdir -p artifacts/{decode,container,parallel,hevc,cabac}
run hevc      "$(share 4)" -dict=dicts/hevc.dict -max_len=200000
run decode    "$(share 3)" -dict=dicts/heif.dict -max_len=400000
run parallel  "$(share 1)" -dict=dicts/heif.dict -max_len=400000
run container "$(share 1)" -dict=dicts/heif.dict -max_len=400000
run cabac     "$(share 1)" -max_len=8192

trap 'echo; echo "Stopping..."; kill $(jobs -p) 2>/dev/null; wait' INT TERM
echo "Running. Follow progress with: tail -f fuzz/logs/hevc.log"
wait

echo
echo "Done. Corpus sizes:"
for t in hevc decode parallel container cabac; do
  printf "  %-10s %6s inputs\n" "$t" "$(ls "corpus/$t" | wc -l | tr -d ' ')"
done
crashes=$(find artifacts -type f | wc -l | tr -d ' ')
if [ "$crashes" -gt 0 ]; then
  echo "FOUND $crashes problem input(s):"
  find artifacts -type f | sed 's/^/  fuzz\//'
  echo "Reproduce with: cd fuzz && cargo +nightly fuzz run <target> <file>"
  exit 1
fi
echo "No crash, timeout or out-of-memory found."
