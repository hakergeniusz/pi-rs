# Wiring notes — package-manager npm source arm (`feat/pm-npm`)

## Files changed

- `src/package_manager.rs` — npm arm implementation, injectable command
  runner, test fixtures (10 new npm tests, 20 in the file total).
- `src/cli/package_command.rs` — doc comment only (`install`/`remove` now
  route to the live npm arm; the `update` arm stays deferred).
- `Cargo.toml` — one new dependency (below).
- This file.

## New dependency

```toml
semver = "1"
```

Added to `crates/pi-coding-agent/Cargo.toml` for npm version selection
(`is_exact_npm_version`, `getNpmVersionRange`, `satisfies`, `gt`,
`maxSatisfying`, `rcompare`), mirroring the `semver` npm package imported at
`package-manager.ts:40`. `semver-1.0.28` is already in the offline cargo
registry cache on the operator's machine.

**The coordinator must run `cargo generate-lockfile` / `cargo check
--locked`-style update once** so `Cargo.lock` gains the new `semver` entry in
`pi-coding-agent`'s dependency list. No other lockfile changes are expected.

### Deliberately *not* added

`reqwest`, `flate2`, `tar`, `sha2` — upstream pi 1.5.2 **does not download npm
tarballs itself**. It shells out to the configured package manager
(`npm`/`pnpm`/`bun`) and lets that binary do registry resolution, tarball
download and extraction (`package-manager.ts:1989-1992`, `:2018-2048`). Porting
a parallel HTTP/tar implementation would be a 1:1-fidelity deviation, add
unnecessary deps, and require a new offline cache entry on a memory-starved
build machine. The "tarball download + extraction" part of the slice brief is
therefore satisfied **by delegation to the package-manager binary**, exactly
as upstream does; the managed install layout still lives in pi's own
`<agent-dir>/npm` / `<cwd>/.pi/npm` roots.

## Call-site changes

- `PackageManager::install`/`remove` now call `install_npm`/`uninstall_npm`
  instead of returning `"npm sources are not implemented in pi-rs yet"`.
  The two deferral error strings are gone; no other error/suggestion text
  changed.
- `getInstalledPath` now includes the legacy global fallback
  (`npm root -g` / `pnpm list -g --json` / bun's `pm bin -g`), so
  `pi list` can report an older globally installed package.
- New constructor `PackageManager::with_runner(..., Box<dyn CommandRunner>)`.
  `new()` is unchanged and uses `SystemCommandRunner`; no CLI or library
  call site needs updating (only `cli/package_command.rs:290` constructs the
  manager, via `new`).
- `ParsedSource::Npm` now wraps a public `NpmSource { spec, name, version,
  range, pinned }` instead of carrying three inline fields. No code outside
  `package_manager.rs` matched this variant.
- `build_no_matching_package_message` and `find_suggested_configured_source`
  are now `pub` (they were `#[allow(dead_code)]`); they serve the `update()`
  suggestion path upstream. The CLI still prints the plain no-match text,
  verbatim.

## Semantics ported (upstream line cites in module docs)

- `isOfflineModeEnabled` (`:63-68`), `isExactNpmVersion` / `getNpmVersionRange`
  (`:70-76`), `parseSource` npm fields (`:1653-1665`).
- `installNpm` / `uninstallNpm` / `installNpmBatch` / `getNpmInstallArgs`
  (`:1357-1361`, `:2018-2067`), `ensureNpmProject` / `markPathIgnoredByCloudSync`
  (`:2262-2273`, `utils/paths.ts:169-184`).
- `getLatestNpmVersion` / `getInstalledNpmVersion` /
  `installedNpmMatchesConfiguredVersion` / `npmHasAvailableUpdate` /
  `shouldUpdateNpmSource` (`:1299-1316`, `:1680-1737`) with verbatim error
  strings (`Empty response from npm view`, `Unexpected response from npm view`).
- `getNpmInstallRoot` / `getManagedNpmInstallPath` / `getGlobalNpmRoot` /
  `getPnpmGlobalPackagePath` / `getLegacyGlobalNpmInstallPath` /
  `getNpmInstallPath` (`:2285-2352`), including the command-key cache.
- Per-manager install/uninstall flags for npm, pnpm and bun, and the
  `Ambiguous npmCommand package managers: …` / `Invalid npmCommand…` errors.
- Test seam: `CommandRunner` (`runCommand`/`runCommandCapture`/`runCommandSync`)
  with a `FakeRunner` driving synthetic `npm view` JSON, fake tarball results
  (a written `package.json` under `<root>/node_modules/<name>`), and canned
  `npm root -g` / `pnpm list -g --json` responses. No test touches the network.

## Deviations / still deferred

- `temporary` scope (`getTemporaryDir`, `Scope = "temporary"`) stays deferred:
  it only serves `resolveExtensionSources`, which is not ported. Scope is
  still `User | Project`.
- Progress events (`withProgress`, `runWithConcurrency`) stay dropped, as in
  the git/local slice; the CLI prints the start line itself.
- The `update` CLI arm still reports `Error: update is not implemented in
  pi-rs yet` (self-update, model catalogs and the batch updater are a separate
  deferred item). The PM-level helpers needed by its npm path now exist.
- npm's full range grammar is approximated: `x`/`X` wildcards, hyphen ranges,
  space-separated comparators and `||` are normalized into Rust `semver`'s
  `VersionReq`. Exotic forms (e.g. prerelease-tag ranges) are rejected the
  same way upstream's `validRange` rejects non-semver tags like `latest`.

## Verification the coordinator should run

```sh
# in the RAM container, one gate at a time (see root AGENTS.md)
podman exec pi-rs-build-ram sh /work/scripts/ram-build.sh test -p pi-coding-agent
podman exec pi-rs-build-ram sh /work/scripts/ram-build.sh gate
```

This subagent did **not** run any cargo command (per the slice's hard rule);
`rustfmt --edition 2024` was run on both changed `.rs` files and succeeded.
