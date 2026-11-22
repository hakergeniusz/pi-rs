# E4 extension-host wiring — `node:*` builtins, host packages and shim fidelity

This document records what the E4 extension-host slices added, what still
needs host wiring, and the gaps deliberately left open. It is written for the
next agent with fresh context; `HANDOFF.md` remains the dated status log.
Section 2 was last updated by the `feat/node-shims` slice (WHATWG URL,
`Stats`, HMAC and `process.env` fidelity).

## 1. What landed (baseline E4)

- **Virtual modules** in `crates/pi-ext-js/src/node/` served through
  `resolver::virtual_module_source`:
  - `node:fs`, `node:fs/promises`, `node:path`, `node:os`, `node:url`,
    `node:child_process`, `node:buffer`, `node:util`, `node:process`,
    `node:crypto`.
  - `@earendil-works/pi-coding-agent` (alias `@mariozechner/...`) and
    `@earendil-works/pi-mcp` (alias `@mariozechner/...`).
- **Rust host ops** installed by `resolver::node::install_ops` from
  `engine::JsHost::install_ops`, all reading the same `HostCore` the `pi`
  API uses. No Node process is embedded and no extension gets direct
  filesystem access from JS.
- **`@earendil-works/pi-codemode/declarations` is untouched** — it remains
  the E1 shim in `glue.rs`.
- Unknown specifiers, including `@earendil-works/pi-ai*`, `pi-tui`,
  `pi-agent-core`, `pi-codemode/source` and `pi-*`, keep the E1 rejection
  string verbatim.

## 2. Shim fidelity raised by `feat/node-shims`

Ground truth for this section is the WHATWG URL Standard and Node's
`node:url` / `node:fs` / `node:crypto` / `node:process` documentation,
**not** a TypeScript file: upstream only imports these as real Node builtins.

### 2.1 `node:url` (`src/node/url.js`)

- Full component parser with authority handling for `http:`, `https:`,
  `ws:`, `wss:`, `ftp:` and `file:`:
  - userinfo (`user[:pass]@`) is kept out of `host`/`hostname`;
  - `hostname` is lowercased for special schemes and keeps IPv6 brackets
    (`[::1]`);
  - `port` drops scheme defaults (`80`, `443`, `21`) and rejects out-of-range
    or non-numeric values; `file:` rejects ports;
  - `host` = `hostname` + optional `:port`, `origin` = `scheme://host` for
    origin-bearing schemes and `"null"` for `file:`/custom schemes.
- Percent-encoding of `pathname`, `search` and `hash` over UTF-8 bytes,
  preserving well-formed `%XX` escapes and the characters allowed by the
  WHATWG path/query encode sets. Existing file conversions
  (`fileURLToPath`, `pathToFileURL`) still go through the Rust ops.
- `URLSearchParams` now uses form-urlencoded serialization (`+` for spaces,
  only `*-._` unescaped besides alphanumerics) and keeps a single live
  object: `set`/`append`/`delete`/`sort` write back into `search` and
  `href`. `get`/`getAll`/`has`, iterators and `forEach` are supported.
- Relative resolution against a base covers network-path (`//host/x`),
  absolute-path (`/x`), query-only (`?x=1`), fragment-only (`#f`) and
  dot-segment (`../x`, `./x`) forms.

### 2.2 `node:fs` (`src/node/fs.js`, host ops in `src/node/mod.rs`)

- `fs.Stats` is built from real metadata for all four host calls: `isFile`,
  `isDirectory`, `isSymbolicLink`, `size` (bytes), `mtimeMs` (fractional
  milliseconds since the epoch) and `mode` (raw Unix permission/type bits),
  plus `isBlockDevice`/`isCharacterDevice`/`isFIFO`/`isSocket` and `mtime`.
- `statSync` follows symlinks (`std::fs::metadata`); `lstatSync` does **not**
  (`std::fs::symlink_metadata`), so `isSymbolicLink()` is now observable.
  `readdirSync(..., {withFileTypes:true})` uses `lstat` so `Dirent` reports
  links correctly.
- Added `symlinkSync(target, path)` (POSIX symlink, Windows symlink_file /
  symlink_dir) so extensions and the fixture can create links.

### 2.3 `node:crypto` (`src/node/hash.js`, host op `nodeCryptoHmac`)

- `createHmac(algorithm, key)` is implemented (previously threw) with the
  same chainable `update`/`digest`/`copy` shape as `createHash`. RFC 2104
  HMAC is computed in Rust over the existing `sha2` dependency (no new
  crate): keys longer than the block size are hashed first, then the
  inner (`0x36`) / outer (`0x5c`) pads are applied. Block sizes: SHA-256 64,
  SHA-512 128.
- Algorithm names are normalized (`SHA-256`, `SHA256`, `sha256` all work).
  Unknown digests still throw `ERR_CRYPTO_INVALID_DIGEST`.
- `getHashes()` reports `sha256`/`sha512`; `digest("buffer")` returns bytes
  and `digest("hex")` lowercase hex.

### 2.4 `node:process` (`src/node/process.js`, host op `nodeProcessEnv`)

- `cwd()`, `platform`, `arch` and `version` are always strings; `arch`
  maps the Rust target triple (`x86_64` → `x64`, `aarch64` → `arm64`).
  `globalThis.process` now exposes the full export set, including `versions`.
- `process.env` is built with `std::env::vars_os()` and filtered so:
  - names must be non-empty and shell-shaped (`[A-Za-z_][A-Za-z0-9_]*`),
    which also rejects NUL/`=`/invalid-UTF-8 entries;
  - credential-shaped names (containing `key`, `token`, `secret`,
    `password`, `credential` or `auth`, case-insensitively) are dropped —
    a deliberate security deviation from Node, not spec behaviour.
  The filter is a pure helper (`filter_env`) unit-tested with synthetic
  entries only; no real secret is ever read or logged.

## 3. Host wiring obligations (unchanged)

1. **cwd is authoritative in `HostCore`.** `node:process.cwd()`,
   `node:path.resolve` and `node:child_process` read `HostCore::cwd`, not
   `std::env::current_dir()`. The embedder must keep `cwd` in sync with the
   session's cwd (already synced by `ExtensionRunner`).
2. **Session metadata is a snapshot.** `getSessionMetadata()` reports
   `session_name`, `cwd`, `mode`, `thinking_level`, entry/label counts and
   tool counts from `HostCore`. Re-run `bind()`/snapshot sync after a
   `newSession`/`fork`/`switchSession` so a captured import sees fresh values.
3. **Tools come from the runner's aggregated state.** `getAllTools()` and
   `getActiveTools()` read `HostCore::all_tools`/`active_tools`;
   `setActiveTools` applies the existing registered/non-hidden filter.
4. **Settings are live.** `getSettings()` reuses the existing `getSettings`
   op, so a settings change after load is visible without reload.
5. **Filesystem permissions mirror upstream.** Extensions run with the
   host's privileges; the trust gate in `loader.rs` still decides whether an
   extension loads. The new `node:fs` shim does not add a second sandbox.

No new wiring is required for the fidelity work in §2: every new behaviour
is either pure JS or an added `__piOps` entry installed by `install_ops`,
which `engine.rs` already calls for every `JsHost`.

## 4. Deliberate deviations still in force

- `node:child_process`: only sync calls plus a shape-compatible `spawn`
  whose events fire after the engine drains its job queue; no real
  concurrent subprocess streams. `fork` throws.
- `node:path.win32` uses POSIX lexical semantics.
- `node:process.exit()` is a no-op and `chdir()` throws; the embedded host
  owns process lifetime and cwd.
- `process.env` is a **snapshot** (Node's is live) and applies the
  credential/name filter from §2.4.
- `node:crypto.randomBytes` uses a non-cryptographic PRNG; only
  `randomUUID` (host `/dev/urandom`) is used by the bundled MCP flow.
- `node:util.TextEncoder`/`TextDecoder` delegate to the Buffer shim; no
  full-stream decode semantics.
- `node:url`: IDNA/punycode conversion is a lowercase pass-through;
  opaque/custom-scheme paths are only partially encoded; there is no
  `URL.canParse` or `URL.parse`.
- `node:os`: `type` is hardcoded `"Linux"`, `cpus()` is synthetic and
  `availableParallelism()` comes from the host thread count.

## 5. Remaining E4 gaps

1. **CJS interop** — ESM-only transpile path remains (`docs/EXTENSIONS.md`
   §6.3); `require()` in an extension still fails.
2. **`/reload` per-path unload** — `ctx.reload()` stales the runtime but the
   JS-side registries are not unloaded per extension path; module-cache
   invalidation is still process-lifetime.
3. **`node_modules` resolution** — bare specifiers outside the virtual
   table are rejected; no npm dependency resolution inside extensions.
4. **Host packages not bundled by the port** — `pi-ai`, `pi-tui`,
   `pi-agent-core`, `pi-codemode/source`, `pi-codemode/worker` keep the E1
   error. The Rust-native MCP/codemode/TUI hosts do not need them today.
5. **`Intl.Segmenter`** — still the E1 regex shim; unspaced scripts segment
   as one run (HANDOFF E1 deviation §1).
6. **Async `spawn` streaming**, real TTY flags, and fuller `crypto`/`url`/
   `os` surfaces (see §4) remain.
7. **Golden fixture parity** — run the upstream-generated extension
   fixtures through these shims when the P4 fixture set is extended for E4.

## 6. Tests

- `crates/pi-ext-js/tests/node_builtins.rs`: 4 tests — one representative
  result per module/package, the raised-fidelity assertions (HMAC, Stats,
  URL, process), the E1 rejection string, and the resolver table check.
- `crates/pi-ext-js/src/node/mod.rs`: 8 unit tests — path semantics, Buffer
  encodings, `fs` error codes, shim table coverage, RFC 4231 HMAC-SHA256 /
  SHA-512 vectors, digest-name normalization, synthetic env filtering, and
  real-metadata `Stats` including `lstat` vs `stat`.
- Fixture: `crates/pi-ext-js/tests/fixtures/node-builtins/index.ts` calls
  every new API with synthetic values only.
