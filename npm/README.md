# pi-rs (npm wrapper)

Native-binary wrapper for [pi-rs](https://github.com/hakergeniusz/pi-rs), a Rust
rewrite of the `pi` coding agent. On install it fetches the release binary for
your platform and verifies its SHA-256 checksum before writing it into `bin/`.

## Install

```sh
npx pi-rs --help
npm install -g pi-rs
bun install -g pi-rs
```

Then run it:

```sh
pi-rs --help
```

## Notes

- The upstream repository is public. Binaries and their `.sha256` checksums are
  downloaded over HTTPS from GitHub Releases `v0.1.1`.
- The checksum is verified before the binary is written; a mismatch aborts the
  install with a clear error.
- Supported platforms: Linux (x86_64, aarch64), macOS (x86_64, aarch64) and
  Windows (x86_64).
- Local development: set `PI_RS_SKIP_DOWNLOAD=1` to skip the download and
  provide the binary in `bin/` yourself.

License: MIT.
