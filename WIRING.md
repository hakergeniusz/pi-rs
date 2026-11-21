# WIRING — release binary / installer / v0.1.0 cut

Produced by the `feat/release-install` subagent. This file is the handoff to
the coordinator. It contains (1) the workflow gap analysis, (2) the exact
proposed YAML diff — **not applied** — and (3) the exact, ordered steps to cut
`v0.1.0`, with approval gates called out. Nothing here has been pushed or
tagged.

## 1. Files created on this branch

- `scripts/install.sh` — POSIX installer (download → checksum → install).
- `docs/RELEASE.md` — release runbook.
- `WIRING.md` — this file.

No existing file was modified. In particular `AGENTS.md`, `HANDOFF.md`,
`README.md`, `Cargo.toml` and `.github/workflows/ci.yml` are untouched.

## 2. Workflow gap analysis + proposed YAML diff (NOT applied)

### 2.1 What the installer needs vs. what `.github/workflows/ci.yml` has

| Need | Current `release` job (lines 94–113) | Gap |
|---|---|---|
| Four assets `pi-{linux,darwin}-{x86_64,aarch64}` | builds only `pi`, uploads CI artifact `pi-linux-x86_64` | **missing** darwin and arm64 builds; CI artifact is not a Release asset |
| `<asset>.sha256` per binary | none | **missing** checksum generation |
| A GitHub Release with assets attached | `actions/upload-artifact@v4` only | **missing** `softprops/action-gh-release` (or `gh release upload`) step |
| Trigger on tag push | `on: push: branches: [main]`, `pull_request` | **missing** `tags: ['v*']` |
| Permission to write the Release | default `contents: read` | **missing** `permissions: contents: write` on the publisher |

Also: the existing `release` job runs on tag pushes too (no `if` guard),
which is a wasted duplicate build once the tag-triggered jobs exist.

### 2.2 Exact diff to apply to `.github/workflows/ci.yml`

Apply after review (this subagent deliberately did not edit the workflow).
Two new jobs replace the need to touch the existing `release` job beyond the
one-line `if` guard: a matrix build job and a single publisher job (single
publisher avoids four jobs racing to create the same Release).

```diff
--- a/.github/workflows/ci.yml
+++ b/.github/workflows/ci.yml
@@ -3,6 +3,7 @@
 on:
   push:
     branches: [main]
+    tags: ['v*']        # publish a GitHub Release when a v* tag is pushed
   pull_request:
 
 concurrency:
@@ -91,6 +92,7 @@
   # The operator's daily driver is the release binary; build it here and
   # publish it as an artifact so a release-profile break is caught by CI
   # instead of by the next local run.
   release:
+    if: github.ref_type != 'tag'   # tag pushes are handled by publish-release below
     runs-on: ubuntu-latest
     env:
       CARGO_PROFILE_RELEASE_DEBUG: 0
@@ -112,6 +114,105 @@
           path: target/release/pi
           if-no-files-found: error
 
+  # Build the exact four assets scripts/install.sh expects. Native runners
+  # avoid cross-compiling quickjs-ng's C code; ubuntu-24.04-arm is GitHub's
+  # hosted ARM runner. If that runner is unavailable for this private repo,
+  # swap it for ubuntu-latest + taiki-e/install-action(tool: cross) and
+  # `cross build --release --target aarch64-unknown-linux-gnu`.
+  build-release-assets:
+    if: startsWith(github.ref, 'refs/tags/v')
+    strategy:
+      fail-fast: false
+      matrix:
+        include:
+          - runner: ubuntu-latest
+            os: linux
+            arch: x86_64
+            target: x86_64-unknown-linux-gnu
+          - runner: ubuntu-24.04-arm
+            os: linux
+            arch: aarch64
+            target: aarch64-unknown-linux-gnu
+          - runner: macos-13
+            os: darwin
+            arch: x86_64
+            target: x86_64-apple-darwin
+          - runner: macos-14
+            os: darwin
+            arch: aarch64
+            target: aarch64-apple-darwin
+    runs-on: ${{ matrix.runner }}
+    env:
+      CARGO_PROFILE_RELEASE_DEBUG: 0
+      CARGO_PROFILE_RELEASE_STRIP: symbols
+    steps:
+      - uses: actions/checkout@v4
+      - uses: dtolnay/rust-toolchain@stable
+        with:
+          targets: ${{ matrix.target }}
+      - name: Build release binary
+        run: cargo build --release --target "${{ matrix.target }}"
+      - name: Stage binary and checksum
+        run: |
+          mkdir -p dist
+          asset="pi-${{ matrix.os }}-${{ matrix.arch }}"
+          cp "target/${{ matrix.target }}/release/pi" "dist/$asset"
+          cd dist
+          if command -v sha256sum >/dev/null 2>&1; then
+            sha256sum "$asset" > "$asset.sha256"
+          else
+            shasum -a 256 "$asset" > "$asset.sha256"
+          fi
+          cat "$asset.sha256"
+      - name: Upload assets
+        uses: actions/upload-artifact@v4
+        with:
+          name: release-${{ matrix.os }}-${{ matrix.arch }}
+          path: dist/*
+          if-no-files-found: error
+
+  # One job creates/updates the Release so the four matrix jobs cannot race.
+  publish-release:
+    if: startsWith(github.ref, 'refs/tags/v')
+    needs: build-release-assets
+    runs-on: ubuntu-latest
+    permissions:
+      contents: write
+    steps:
+      - uses: actions/download-artifact@v4
+        with:
+          pattern: release-*
+          path: dist
+          merge-multiple: true
+      - name: Verify we have four binaries and four checksums
+        run: |
+          set -eu
+          for asset in pi-linux-x86_64 pi-linux-aarch64 \
+                       pi-darwin-x86_64 pi-darwin-aarch64; do
+            test -s "dist/$asset"
+            test -s "dist/$asset.sha256"
+          done
+          ls -l dist
+      - name: Create GitHub Release with assets
+        uses: softprops/action-gh-release@v2
+        with:
+          files: dist/*
+          generate_release_notes: true
+          fail_on_unmatched_files: true
+
   # Advisory + license scan over the dependency tree; deliberately
   # non-blocking so an unmaintained-crate warning cannot strand pushes.
   audit:
```

Review notes for the coordinator:

- `softprops/action-gh-release@v2` uses the job's `GITHUB_TOKEN`; the
  `permissions: contents: write` block is required (repo defaults are read).
- The checksum files contain `<sha>  <basename>` because `sha256sum`/`shasum`
  run from inside `dist/` — exactly the format `install.sh` parses.
- `merge-multiple: true` flattens the four artifacts into one `dist/` directory
  so `files: dist/*` attaches all eight files.
- The manual fallback if the hosted ARM runner is unavailable for a private
  repo is noted in the diff comment; a self-hosted ARM runner or `cross` also
  works. Do not drop the arm64 assets — the installer maps `uname -m=arm64`.
- `concurrency` at the top cancels in-progress runs for the same ref; different
  tags are different refs, so a tag run will not be cancelled by a `main` push.

## 3. Exact steps to cut v0.1.0

Legend: **[APPROVAL]** = stop and get explicit user confirmation before acting
(`PLAN.md` §7, `AGENTS.md` "Git workflow" / "No push until user approves").

### Step 0 — preconditions (no changes)

- [ ] Confirm P7 exit criteria are met and all in-flight P7 slices have landed.
- [ ] Confirm CI is green on `main` (all 7 jobs, incl. `release`).
- [ ] **[APPROVAL]** User approves cutting `v0.1.0` and permits the version
      bump. Decide also whether Releases stay private (installer then needs
      `GITHUB_TOKEN`) or the repo/releases become public.

### Step 1 — apply the workflow diff (§2.2)

- [ ] Review and apply the diff to `.github/workflows/ci.yml` on a topic
      branch (e.g. `ci/release-assets`).
- [ ] Gate/validate YAML, commit (`ci(release): publish checksummed release
      assets on v* tags` + `Assisted-by:` trailer), **[APPROVAL]** push, ff-merge
      to `main`, **[APPROVAL]** push `main`, confirm CI green.

### Step 2 — the version bump

- [ ] On a topic branch (e.g. `release/v0.1.0`), set Cargo version to `0.1.0`:
      `Cargo.toml:15` (workspace version) **and** `Cargo.toml:54–60` (seven
      internal dependency `version = "0.1.0-alpha.1"` entries). Crate manifests
      inherit via `version.workspace = true`.
- [ ] Update `README.md:7` status from `v0.1.0-alpha.1 (pre-alpha)` to `v0.1.0`.
- [ ] Verify no stale versions: `rg -n '0\.1\.0-alpha' --glob '!Cargo.lock' .`
      (only `AGENTS.md:133` policy prose and the historical `HANDOFF.md:439`
      line should remain).
- [ ] Regenerate `Cargo.lock` as part of the gate (do not hand-edit).

### Step 3 — gate

- [ ] Run the full gate (fmt → clippy `-D warnings` → test → `fmt --check` →
      `cargo check -p pi-coding-agent --no-default-features`). See
      `docs/RELEASE.md` §4 for the RAM-container commands. CI is the authority
      for the full run.
- [ ] Confirm the built binary reports `0.1.0`.

### Step 4 — commit and land

- [ ] Commit with a conventional message, e.g.
      `chore(release): bump workspace to v0.1.0` plus
      `Assisted-by: OpenCode (hy4-flash-free)`.
- [ ] **[APPROVAL]** ff-merge to `main`, push, delete the topic branch.
- [ ] Wait for CI green on the version commit (`gh run watch <run-id>
      --exit-status`).

### Step 5 — tag and release

- [ ] **[APPROVAL]** Create the annotated tag on the green `main` commit:
      `git tag -a v0.1.0 -m "pi-rs v0.1.0"`.
- [ ] **[APPROVAL]** Push the tag: `git push origin v0.1.0`.
- [ ] Watch the tag workflow (`gh run list --branch v0.1.0 --limit 5`), confirm
      `build-release-assets` (4×) and `publish-release` succeed.
- [ ] **[APPROVAL]** Confirm/finalize the GitHub Release: eight assets attached
      (four binaries + four `.sha256`), notes acceptable. If using the manual
      fallback instead, run the `gh release create v0.1.0 …` command from
      `docs/RELEASE.md` §5.2 — also **[APPROVAL]**.

### Step 6 — verify with the installer

- [ ] Follow `docs/RELEASE.md` §7: download the script to a file (never
      `curl | sh`), install into a temp prefix with `GITHUB_TOKEN` set, confirm
      `checksum OK`, `pi --version` reports `0.1.0`, and `sha256sum -c` passes.
- [ ] Confirm a bad checksum / missing token leaves the prefix untouched.

### Step 7 — post-release docs

- [ ] Append a dated `HANDOFF.md` entry (tag, CI run ID, asset list, deviations)
      and any final `README.md` touch-ups. Do not rewrite history.

## 4. Operations that need user confirmation — summary

1. Cutting `v0.1.0` at all, and the private-vs-public release decision.
2. Applying/pushing the CI workflow change (§2.2) and ff-merging it to `main`.
3. The version-bump commit's merge and push to `main`.
4. Creating the `v0.1.0` tag.
5. Pushing the tag.
6. Creating/finalizing the GitHub Release (or the manual `gh release create`).
7. Any deletion/replacement of the tag or Release (shared-history change).

This subagent performed **none** of steps 1–7 beyond authoring these files and
committing them on `feat/release-install` (no push, no tag, no workflow edits).
