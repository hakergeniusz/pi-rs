# Cutting a pi-rs release

This is the release runbook. It complements `PLAN.md` (P7 exit: "tagged
v0.1.0") and `AGENTS.md` (git/release policy). The installer contracts
(`scripts/install.sh` for Linux/macOS and `scripts/install.ps1` for Windows)
are defined at the bottom of this document.

## v0.1.1 - actual release state (2026-11-23)

The `v0.1.1` tag is published as a GitHub Release with **ten assets**: five
binaries (Linux x86_64/aarch64, macOS x86_64/aarch64, Windows x86_64 `.exe`)
and one `.sha256` file per binary. The tag workflow verifies every binary has
a matching checksum before publishing. The npm wrapper is
`@hakergeniusz/pi-rs@0.1.2`; it downloads the matching release binary and
verifies its SHA-256.

The repository is **public** (`gh api repos/hakergeniusz/pi-rs --jq .private`
returns `false`), so release assets are anonymously downloadable and no token
is needed for verification. The tag workflow needs `contents: write` to upload
assets.

## 1. Version discipline

- `main` carries `0.1.1` (`[workspace.package] version` in `Cargo.toml`) and
  the release tag is `v0.1.1`, with the leading `v`.
- Patch bumps keep the tag and Cargo version in lockstep. Crate manifests use
  `version.workspace = true` / `workspace = true`, so they inherit it; commit
  the regenerated `Cargo.lock`.
- Only the coordinator performs the bump, and only after the user approves the
  cut. Never bump on a feature branch as a side effect of other work.
- `AGENTS.md` and the `README.md` badge show `0.1.1`. `HANDOFF.md` is a dated
  log: append a new entry, do not rewrite history.

## 2. Preconditions for the cut

- P7 exit criteria are met: release binary builds, installer + checksums work
  (`PLAN.md` §4, `AGENTS.md` "Current status").
- The full gate is green **in CI on `main`** (see §4). CI is the authority for
  the full run; the workstation's long local gates get cancelled by restarts.
- The tag workflow in `.github/workflows/ci.yml` is implemented and merged
  (§3 and §5); no proposed diff needs applying before a tag is pushed.
- User has explicitly approved: the version bump, the push, the tag, and the
  GitHub Release creation (`PLAN.md` §7: "No GitHub push until user explicitly
  approves").

## 3. What CI does today

`.github/workflows/ci.yml` runs on every push and PR to `main`, and on `v*`
tags. Jobs: `fmt`, `clippy`, `test`, `no-default-features`, `msrv`, and
`audit` (advisory, non-blocking).

- On ordinary pushes/PRs the `release` job builds `cargo build --release` on
  `ubuntu-latest` and uploads `target/release/pi` as the `pi-linux-x86_64`
  artifact. It is a per-push *build smoke test*, not a publisher.
- On a `v*` tag, `build-release-assets` builds all five targets on native
  runners (Ubuntu x86_64, Ubuntu ARM64, `macos-15-intel`, `macos-15`,
  `windows-latest`), stages each binary plus a `<asset>.sha256` in `dist/`,
  and uploads one artifact per target. `fail-fast: false` keeps a failed row
  from cancelling the others.
- `publish-release` waits for those jobs, checks that every binary has a
  matching non-empty checksum file, then creates/updates the GitHub Release
  with all assets and generated notes (`contents: write`).

## 4. The full gate

Run all five steps, in this order (`AGENTS.md` "Build and test"; `.gate.sh`):

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace -- --test-threads=4
cargo fmt --all -- --check
cargo check -p pi-coding-agent --no-default-features   # NDF
```

`NDF` is not redundant: the `pi-ext-js` dependency is optional, and a
cfg-fenced build once went red without any other job noticing. The interactive
tests need `fd` and `rg` on `PATH` (CI apt-installs `ripgrep` + `fd-find`).

On the memory-capped workstation use the host script; podman and
`scripts/ram-build.sh` are retired:

```sh
cd /home/hakergeniusz/Projects/pi-rs
TMPDIR=/var/tmp PI_CACHE_DIR=/var/cache/zstd5/cache \
  PI_TARGET_DIR=/var/cache/zstd5/cargo-target JOBS=2 \
  sh scripts/host-build.sh gate
```

The knobs are `PI_TARGET_DIR`, `JOBS` and `PI_CACHE_DIR`; caches stay under
`/var/cache/zstd5`, never `/tmp`, and the script applies `nice -n 19` to every
cargo step. Clippy pedantic lints are errors (`-D warnings`); do not silence
them to get green. After the bump, confirm `pi --version` / the binary's
reported version matches the Cargo version before tagging.

## 5. Publishing the release

### 5.1 Preferred: tag-triggered workflow (implemented)

The `build-release-assets` + `publish-release` jobs in
`.github/workflows/ci.yml` implement the `v*` tag trigger: all five targets,
one `<asset>.sha256` per binary, and a single Release created/updated with
generated notes. The coordinator's job is then:

1. push the annotated tag (approval required);
2. watch the workflow: `gh run list --branch <tag> --limit 5` then
   `gh run watch <run-id> --exit-status`;
3. confirm the Release exists and has **ten** assets (five binaries + five
   `.sha256` files), including `pi-windows-x86_64.exe` and its checksum;
4. edit the Release notes if the generated ones need context (approval not
   needed for wording, but announce the release only after user sign-off).

### 5.2 Manual fallback (only if the workflow is unavailable)

With explicit user approval, set the version, gate, and build each target
locally, then create the release and attach the files in one command:

```sh
gh release create v0.1.1 \
  dist/pi-linux-x86_64 dist/pi-linux-x86_64.sha256 \
  dist/pi-linux-aarch64 dist/pi-linux-aarch64.sha256 \
  dist/pi-darwin-x86_64 dist/pi-darwin-x86_64.sha256 \
  dist/pi-darwin-aarch64 dist/pi-darwin-aarch64.sha256 \
  dist/pi-windows-x86_64.exe dist/pi-windows-x86_64.exe.sha256 \
  --title "pi-rs v0.1.1" --generate-notes
```

Generate each checksum in the directory containing the binary, so the file
contains just the basename (this is the format the installers parse):

```sh
cd dist
sha256sum pi-linux-x86_64 > pi-linux-x86_64.sha256       # Linux
shasum -a 256 pi-darwin-aarch64 > pi-darwin-aarch64.sha256  # macOS/BSD
```

On Windows, generate the checksum in PowerShell (the CI staging step uses the
same command; the basename in the file includes the `.exe` extension):

```powershell
$hash = (Get-FileHash -Algorithm SHA256 pi-windows-x86_64.exe).Hash.ToLower()
[System.IO.File]::WriteAllText("pi-windows-x86_64.exe.sha256", "$hash  pi-windows-x86_64.exe`n")
```

`gh release upload v0.1.1 <files...>` attaches additional assets to an
existing release. Never commit release binaries - they are build artifacts.

## 6. Installer contract

Both installers expect, for release tag `TAG`:

| Asset | Purpose |
|---|---|
| `pi-linux-x86_64` | binary, Linux x86-64 |
| `pi-linux-aarch64` | binary, Linux ARM64 |
| `pi-darwin-x86_64` | binary, macOS Intel |
| `pi-darwin-aarch64` | binary, macOS Apple Silicon |
| `pi-windows-x86_64.exe` | binary, Windows x86-64 (per-user, no admin needed) |
| `<binary asset>.sha256` | one checksum file per binary, format `<sha>  <basename>`; the basename includes the `.exe` extension for the Windows asset. A single `SHA256SUMS` with the same lines is also accepted as a fallback by `install.sh`. |

`scripts/install.sh` maps `uname -s`/`uname -m` to that table, downloads the
binary and its checksum, verifies SHA-256 (with `sha256sum` or `shasum -a
256`), and only then copies it to `$PI_INSTALL_DIR/pi-rs` (default
`~/.local/bin/pi-rs`). It never pipes downloads to a shell, never writes
outside the prefix (apart from a cleaned-up `TMPDIR` scratch dir), and uses
`sudo` only with `--sudo`.

`scripts/install.ps1` supports the one-liner:

```powershell
irm https://hakergeniusz.github.io/pi-rs/install.ps1 | iex
```

It detects an x86-64 host (clear error otherwise), downloads
`pi-windows-x86_64.exe` plus `pi-windows-x86_64.exe.sha256` with
`Invoke-WebRequest -UseBasicParsing`, verifies SHA-256 with `Get-FileHash`
**before** writing anything, and installs to `<Prefix>\pi-rs.exe` (default
`$env:LOCALAPPDATA\Programs\pi-rs`). It writes only inside the prefix, except
that the opt-in `-AddToPath` switch appends the prefix to the *user* PATH.
`-Version` selects the release tag (default `v0.1.1`; pass `latest` for the
newest release) and `-Force` replaces an existing install. No downloaded
content other than this script itself is ever passed to `iex`.

## 7. Verify a release locally with the installer

The repo is public, so no token is needed. Download the script as a *file*
(the piped one-liner is also supported):

```sh
curl -fsSLO https://hakergeniusz.github.io/pi-rs/install.sh
sh ./install.sh --help                    # options/usage
sh ./install.sh --version                 # installer version
DEST=$(mktemp -d)
sh ./install.sh --prefix "$DEST" v0.1.1
"$DEST/pi-rs" --help
"$DEST/pi-rs" --version                   # must report 0.1.1
```

Expected: a `checksum OK` line, then the install path and next steps. Re-run
to update an existing install in the same prefix (overwrite is the default;
`--no-update` refuses). To check the checksum independently of the installer:

```sh
cd "$DEST" && sha256sum -c pi-linux-x86_64.sha256   # or shasum -a 256 -c ...
```

A mismatch, a 404, or a token error must leave the prefix untouched. After
verifying, remove the scratch dir (use `trash` on the operator's workstation).

The Windows installer can be verified the same way, downloading the script as
a file first so it can be read before running:

```powershell
irm https://hakergeniusz.github.io/pi-rs/install.ps1 -OutFile install.ps1
$DEST = Join-Path $env:TEMP 'pi-rs-install-test'
powershell -ExecutionPolicy Bypass -File .\install.ps1 -Prefix $DEST -Version v0.1.1
& (Join-Path $DEST 'pi-rs.exe') --version    # must report 0.1.1
```

Expect a `checksum OK` line followed by the install path and next steps. A
checksum mismatch aborts before the prefix is touched. To uninstall, remove
`<Prefix>\pi-rs.exe` (and, if you used `-AddToPath`, remove the prefix from
your user PATH).

## 8. Post-release

- Append a dated entry to `HANDOFF.md` (release cut, tag, CI run ID, asset
  list) and update the `README.md` status line if the gate changed it.
- Confirm CI's `main` run is still green after the version commit.
- ff-merge the release topic branch to `main`, delete the branch, push - each
  per `AGENTS.md` and with user approval.
- Deleting a bad tag/Release or re-cutting a published version is a
  destructive/shared-history operation: ask the user first.
