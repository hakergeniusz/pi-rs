# Platform build status (2026-11-27)

The release matrix in `.github/workflows/ci.yml` produces one asset plus a
`.sha256` per platform. This file records which rows are verified and why the
others are best effort.

## Verified rows

| Asset | How | Evidence |
| --- | --- | --- |
| `pi-linux-x86_64` | native `ubuntu-latest` | shipped in v0.1.1 |
| `pi-linux-aarch64` | native `ubuntu-24.04-arm` | shipped in v0.1.1 |
| `pi-darwin-x86_64` | native `macos-15-intel` | shipped in v0.1.1 |
| `pi-darwin-aarch64` | native `macos-15` | shipped in v0.1.1 |
| `pi-windows-x86_64.exe` | native `windows-latest` | shipped in v0.1.1 |
| `pi-windows-aarch64.exe` | native `windows-latest`, `aarch64-pc-windows-msvc` | added in this wave; std + MSVC ARM64 tools are available on the runner |
| `pi-linux-riscv64` | `cross build --target riscv64gc-unknown-linux-gnu` | CI run `46025179532`; quickjs atomics need `-C link-arg=-latomic` |
| `pi-freebsd-x86_64` | `cross build --target x86_64-unknown-freebsd` | CI run `46025179532`; rquickjs has no BSD bindings, so CI seeds them from the Linux System V bindings |

## Best-effort rows (tagged releases only)

The `build-tier3-bsd-assets` job is `continue-on-error: true`; it never blocks
the release, and any assets it stages are published alongside the verified
ones.

| Asset | Rust target | Why not verified |
| --- | --- | --- |
| `pi-freebsd-aarch64` | `aarch64-unknown-freebsd` | the aarch64 cross image's gcc/linker is not discoverable by build scripts (`oxc_sourcemap` links host `ld` against ARM64 objects); no hosted ARM64 FreeBSD runner exists |
| `pi-netbsd-x86_64` | `x86_64-unknown-netbsd` | the cross image lacks a usable `libexecinfo` on the target linker path |
| `pi-netbsd-aarch64` | `aarch64-unknown-netbsd` | Rust tier 3: no prebuilt `std`, no upstream cross image |
| `pi-openbsd-x86_64` | `x86_64-unknown-openbsd` | Rust tier 3: no prebuilt `std`, no upstream cross image |
| `pi-openbsd-aarch64` | `aarch64-unknown-openbsd` | Rust tier 3: no prebuilt `std`, no upstream cross image |

The tier-3 job builds `std` with nightly `-Z build-std=std,panic_abort` and
uses `zig cc` for C dependencies. It currently fails in `ring` because the
container has no BSD libc headers for `assert.h`. Publishing a working asset
for these rows needs either hosted BSD runners or a purpose-built cross image
with BSD sysroot headers and libraries.

## Notes

- All asset names are `<os>-<arch>`; Windows keeps the `.exe` suffix and the
  checksum filename derives from the asset name.
- `scripts/install.sh` maps `uname -s`/`uname -m` to every row;
  `scripts/install.ps1` maps AMD64/ARM64; `npm/install.js` maps Node's
  `process.platform`/`process.arch`.
- The rquickjs BSD binding seed is an ABI assumption (64-bit System V),
  documented inline in the workflow. If a BSD-specific binding difference ever
  matters, switch back to bindgen with a sysroot-complete cross image.
