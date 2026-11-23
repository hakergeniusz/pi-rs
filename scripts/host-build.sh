#!/bin/sh
# Host-side build workflow for pi-rs (replaces the retired podman containers).
#
# Keeps the low-TBW contract from the old RAM container: cargo artifacts and
# test caches live on the host tmpfs (/tmp/opencode), and only an explicit
# `snapshot` writes a single compressed archive to the workspace's
# .build-cache dir. The cargo registry stays read-only/offline.
#
# Usage:
#   sh scripts/host-build.sh check [-p crate]
#   sh scripts/host-build.sh test [-p crate]
#   sh scripts/host-build.sh clippy [-p crate]
#   sh scripts/host-build.sh build [--release]
#   sh scripts/host-build.sh gate
#   sh scripts/host-build.sh snapshot

set -u

# /tmp is a user-quota'd tmpfs that fills during --all-targets builds; the
# cargo build cache therefore lives under /var/cache/zstd5 (disk-backed,
# survives reboots) with DEBUGINFO=0 and no incremental data, and `snapshot`
# batches it into one archive. PI_TARGET_DIR overrides the target dir.
export CARGO_TARGET_DIR="${PI_TARGET_DIR:-/var/cache/zstd5/cargo-target}"
export CARGO_BUILD_JOBS="${JOBS:-4}"
export CARGO_INCREMENTAL=0
export CARGO_PROFILE_DEV_DEBUG=0
export CARGO_PROFILE_TEST_DEBUG=0
export CARGO_PROFILE_RELEASE_DEBUG=0
export CARGO_NET_OFFLINE=true
export XDG_CACHE_HOME="${PI_CACHE_DIR:-/var/cache/zstd5/cache}"

# Stale host-level overrides from earlier PTY smoke tests make test fixtures
# write to /pi; the crate's tests manage their own synthetic dirs.
unset PI_CODING_AGENT_DIR PI_CODING_AGENT_SESSION_DIR

mkdir -p "$CARGO_TARGET_DIR" "$XDG_CACHE_HOME"
cd "$(dirname "$0")/.." || exit 1

SNAPSHOT=.build-cache/host-target.tar.zst

snapshot() {
  mkdir -p .build-cache
  echo "[host-build] snapshotting $CARGO_TARGET_DIR -> $SNAPSHOT (zstd -5)"
  tar -I 'zstd -5' -cf "${SNAPSHOT}.tmp" -C "$(dirname "$CARGO_TARGET_DIR")" "$(basename "$CARGO_TARGET_DIR")"
  mv -f "${SNAPSHOT}.tmp" "$SNAPSHOT"
}

gate() {
  rc=0
  echo "=== fmt ==="
  nice -n 19 cargo fmt --all || rc=1
  echo "=== clippy ==="
  nice -n 19 cargo clippy --workspace --all-targets --offline -- -D warnings || rc=1
  echo "=== test ==="
  nice -n 19 cargo test --workspace --offline -- --test-threads=4 || rc=1
  echo "=== fmt --check ==="
  nice -n 19 cargo fmt --all -- --check || rc=1
  echo "=== NDF ==="
  # Match CI exactly: the ext-js-gated test helpers are not built without the
  # feature; only the library shape must compile.
  nice -n 19 cargo check -p pi-coding-agent --offline --no-default-features || rc=1
  echo "GATE_RC=$rc"
  return "$rc"
}

cmd="${1:-check}"
shift || true
case "$cmd" in
  check)    nice -n 19 cargo check --offline "$@" ;;
  test)     nice -n 19 cargo test --offline "$@" ;;
  clippy)   nice -n 19 cargo clippy --offline "$@" ;;
  build)    nice -n 19 cargo build --offline "$@" ;;
  fmt)      nice -n 19 cargo fmt --all ;;
  snapshot) snapshot ;;
  gate)     gate ;;
  *)
    echo "usage: $0 {check|test|clippy|build|fmt|snapshot|gate} [args...]" >&2
    exit 2
    ;;
esac
