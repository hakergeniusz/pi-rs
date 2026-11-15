#!/bin/sh
# Certification gate (AGENTS.md order) + the engine-free shape check.
# Run: podman exec pi-rs-build sh /work/.gate.sh
export CARGO_TARGET_DIR=/build-target
export CARGO_BUILD_JOBS=8
export CARGO_INCREMENTAL=0
export CARGO_PROFILE_DEV_DEBUGINFO=0
cd /work || exit 1

echo "== fmt (write) =="
nice -n 19 cargo fmt --all
echo "FMT=$?"

echo "== clippy =="
nice -n 19 cargo clippy --workspace --all-targets -- -D warnings > /tmp/gate-clippy.log 2>&1
echo "CLIPPY=$?"

echo "== test =="
nice -n 19 cargo test --workspace -- --test-threads=4 > /tmp/gate-test.log 2>&1
echo "TEST=$?"

echo "== fmt (check) =="
nice -n 19 cargo fmt --all -- --check
echo "FMTCHECK=$?"

echo "== no-default-features =="
nice -n 19 cargo check -p pi-coding-agent --no-default-features > /tmp/gate-ndf.log 2>&1
echo "NDF=$?"
