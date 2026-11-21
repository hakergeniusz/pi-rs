#!/bin/sh
# RAM-first build workflow for pi-rs (workstation-specific).
#
# Every cargo artifact and test cache lives in tmpfs (/ramdisk) inside the
# `pi-rs-build-ram` container. The SSD volume (/build-target) is touched only
# once per gate, by `snapshot`, which writes ONE compressed archive instead of
# the thousands of small files a normal cargo target dir produces. This keeps
# SSD TBW low and makes the build IO bursty (RAM) -> sequential (disk).
#
# Run from the host:
#   podman exec pi-rs-build-ram sh /work/scripts/ram-build.sh check -p pi-mcp
#   podman exec pi-rs-build-ram sh /work/scripts/ram-build.sh test -p pi-mcp
#   podman exec pi-rs-build-ram sh /work/scripts/ram-build.sh gate
#   podman exec pi-rs-build-ram sh /work/scripts/ram-build.sh snapshot
#
# Environment (all set here so gate steps share one cache):
#   CARGO_TARGET_DIR=/ramdisk/target      (tmpfs, not the SSD volume)
#   XDG_CACHE_HOME=/ramdisk/cache         (oxc transpile cache -> RAM)
#   XDG_DATA_HOME=/ramdisk/data
#   CARGO_INCREMENTAL=0, DEBUGINFO=0      (small artifacts, no incremental churn)
#   CARGO_NET_OFFLINE=true                (registry bind mount is read-only)

set -u

export CARGO_TARGET_DIR=/ramdisk/target
export CARGO_BUILD_JOBS="${JOBS:-4}"
export CARGO_INCREMENTAL=0
export CARGO_PROFILE_DEV_DEBUGINFO=0
export CARGO_PROFILE_RELEASE_DEBUGINFO=0
export CARGO_NET_OFFLINE=true
export XDG_CACHE_HOME=/ramdisk/cache
export XDG_DATA_HOME=/ramdisk/data

mkdir -p "$CARGO_TARGET_DIR" "$XDG_CACHE_HOME" "$XDG_DATA_HOME"
cd /work || exit 1

SNAPSHOT=/build-target/ram-target.tgz
TMP_SNAPSHOT=/build-target/ram-target.tgz.tmp

restore() {
  if [ -f "$SNAPSHOT" ]; then
    echo "[ram-build] restoring target from $SNAPSHOT"
    tar xzf "$SNAPSHOT" -C /ramdisk
  else
    echo "[ram-build] no snapshot; cold RAM target"
  fi
}

snapshot() {
  # One grouped, sequential write to disk. Atomic rename avoids a torn archive.
  echo "[ram-build] snapshotting $CARGO_TARGET_DIR -> $SNAPSHOT"
  tar czf "$TMP_SNAPSHOT" -C /ramdisk target
  mv -f "$TMP_SNAPSHOT" "$SNAPSHOT"
  echo "[ram-build] snapshot complete"
}

gate() {
  # Full workspace gate. Capture every exit code (never rely on a masked pipe).
  local rc=0
  echo "=== fmt ==="
  nice -n 19 cargo fmt --all || rc=1
  echo "=== clippy ==="
  nice -n 19 cargo clippy --workspace --all-targets --offline -- -D warnings || rc=1
  echo "=== test ==="
  nice -n 19 cargo test --workspace --offline -- --test-threads=4 || rc=1
  echo "=== fmt --check ==="
  nice -n 19 cargo fmt --all -- --check || rc=1
  echo "=== check --no-default-features ==="
  nice -n 19 cargo check --workspace --all-targets --offline --no-default-features || rc=1
  echo "GATE_RC=$rc"
  return "$rc"
}

cmd="${1:-check}"
shift || true
case "$cmd" in
  check)    nice -n 19 cargo check --offline "$@" ;;
  test)     nice -n 19 cargo test --offline "$@" ;;
  clippy)   nice -n 19 cargo clippy --offline "$@" ;;
  fmt)      nice -n 19 cargo fmt --all ;;
  restore)  restore ;;
  snapshot) snapshot ;;
  gate)     gate ;;
  *)
    echo "usage: $0 {check|test|clippy|fmt|restore|snapshot|gate} [cargo args...]" >&2
    exit 2
    ;;
esac
