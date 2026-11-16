#!/bin/sh
# Certification gate (AGENTS.md order) + the engine-free shape check.
# Run: podman exec pi-rs-build sh /work/.gate.sh
# Exit status: 0 only when EVERY step passed (the echoes are per-step).
export CARGO_TARGET_DIR=/build-target
export CARGO_BUILD_JOBS=8
export CARGO_INCREMENTAL=0
export CARGO_PROFILE_DEV_DEBUGINFO=0
cd /work || exit 1

STATUS=0

echo "== fmt (write) =="
nice -n 19 cargo fmt --all
code=$?
echo "FMT=$code"
[ "$code" -ne 0 ] && STATUS=1

echo "== clippy =="
nice -n 19 cargo clippy --workspace --all-targets -- -D warnings > /tmp/gate-clippy.log 2>&1
code=$?
echo "CLIPPY=$code"
[ "$code" -ne 0 ] && STATUS=1

echo "== test =="
nice -n 19 cargo test --workspace -- --test-threads=4 > /tmp/gate-test.log 2>&1
code=$?
echo "TEST=$code"
[ "$code" -ne 0 ] && STATUS=1

echo "== fmt (check) =="
nice -n 19 cargo fmt --all -- --check
code=$?
echo "FMTCHECK=$code"
[ "$code" -ne 0 ] && STATUS=1

echo "== no-default-features =="
nice -n 19 cargo check -p pi-coding-agent --no-default-features > /tmp/gate-ndf.log 2>&1
code=$?
echo "NDF=$code"
[ "$code" -ne 0 ] && STATUS=1

exit "$STATUS"
