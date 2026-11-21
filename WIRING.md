# Wiring notes — `feat/float-roundtrip` (serde_json `float_roundtrip`)

Branch: `feat/float-roundtrip` in `/tmp/opencode/pi-rs-wt-float-roundtrip`.
No push. **No cargo command was run by this subagent** (per the slice rules);
the coordinator must gate in the RAM container.

## What changed

| File | Change |
|---|---|
| `Cargo.toml` | Workspace `serde_json` now has `features = ["preserve_order", "float_roundtrip"]`. |
| `crates/pi-coding-agent/src/session/json.rs` | Module `//!` doc now records the two halves of float byte-fidelity (correct parse + V8 formatting) and why no raw-token passthrough is needed; `format_f64` doc cross-references V8's `DoubleToStringConverter`; **+2 unit tests**: exotic float tokens on the canonical `Record` path and a 38-line golden-fixture byte regression. |
| `crates/pi-coding-agent/src/session/records.rs` | **+1 unit test**: the same 1-ULP / >17-digit tokens survive the typed `SessionEntry` decode→encode path (an `f64` struct field and an `unknown`-bag `Value`). |

No writer logic changed: `format_f64` was already an ECMAScript `Number::toString`
implementation. The defect was entirely in serde_json's *default* float parser
(`significand as f64` followed by one multiply/divide by a power of ten), which
can land one ULP off for ~17-digit literals. With `float_roundtrip` the parse
matches V8's `JSON.parse`, and the existing shortest-round-trip formatter then
reproduces V8's token exactly.

### Why no passthrough

The brief allowed a targeted float-token passthrough if the feature were
insufficient. It is **not** needed and would be actively wrong: pi's own
load→rewrite cycle is `JSON.parse` → `JSON.stringify`, which *canonicalizes*
non-V8 spellings (`0.10000000000000000555…` → `0.1`). A verbatim passthrough
would diverge from pi precisely on hand-edited/non-canonical tokens. Correct
parsing plus V8 formatting covers both cases; this is documented in the
`session/json.rs` module doc.

## Coordinator gate

Run the normal RAM-container gate (do **not** let me run cargo):

```sh
podman exec pi-rs-build-ram sh /work/scripts/ram-build.sh gate
podman exec pi-rs-build-ram sh /work/scripts/ram-build.sh snapshot
```

Targeted alternative before the full gate:

```sh
podman exec pi-rs-build-ram sh /work/scripts/ram-build.sh test -p pi-coding-agent
```

### Cargo.lock

`serde_json`'s `float_roundtrip` feature is declared `float_roundtrip = []`
(no new crates; `lexical` is vendored inside serde_json), so **Cargo.lock should
be byte-identical** after the gate. Verify with:

```sh
git diff --exit-code -- Cargo.lock
```

If the regenerated lock changes the `serde_json` version (the workspace
requirement is the loose `"1"`), review that diff deliberately — 1.0.154 (the
currently pinned version) already ships the feature and its own
`test_roundtrip_f64` regression samples are the ones used in the new tests.
Commit the lock alongside if it legitimately moved, otherwise leave it alone.

## Verification done by this subagent (no cargo)

- `rustfmt --edition 2024` run on both edited `.rs` files (clean).
- The exact `format_f64` body extracted into a standalone `rustc`-only program
  (`/tmp/ai-agent/check_format.rs`) with correctly-rounded `str::parse::<f64>`
  as a stand-in for serde_json's `float_roundtrip` parse: **all 15 expected
  token spellings matched** (1-ULP samples, random-bit exponent forms,
  >17-digit decimal expansions, `1e+21` / `1e-7` thresholds).
- A Python oracle (correct parse + shortest digits + ECMAScript thresholds)
  independently produced the same 15 spellings.

## Test count added

3 unit tests (2 in `session/json.rs`, 1 in `session/records.rs`). Existing
golden-fixture integration tests in `tests/golden_sessions.rs` were **not**
edited.

## Remaining known divergences (not addressed by this slice)

1. **Integers beyond 2^53.** `format_number` preserves `u64`/`i64` tokens
   verbatim (the existing `numbers_match_v8` test pins
   `123456789012345678` → `123456789012345678`), whereas V8 parses them as a
   double and would write `123456789012345680`. Pre-existing behavior, out of
   scope, and unreachable for files actually written by pi. The five golden
   fixtures contain only integer tokens pi itself wrote, so they are unaffected.
2. **Out-of-range magnitudes.** serde_json rejects literals that round to
   infinity with `NumberOutOfRange`, while V8's `JSON.parse` yields `Infinity`
   and `JSON.stringify` emits `null`. Extremely exotic for session files;
   `format_f64` maps non-finite `f64`s to `null` defensively but the load fails
   first.
3. **`float_roundtrip` correctness boundary.** serde_json's vendored `lexical`
   parser is documented as "reliably round tripping all possible floating
   point values" rather than perfectly parsing arbitrarily long digit strings;
   for session-scale numbers this is indistinguishable from V8.
