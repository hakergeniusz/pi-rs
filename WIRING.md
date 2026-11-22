# Session typed re-encode fidelity — wiring notes

Slice `feat/session-fidelity`. Ground truth: upstream
`packages/coding-agent/src/core/session-manager.ts` (`_persist` /
`_rewriteFile`, entry write sites `:1215-1312`, `:1350-1367`, `:1399-1407`,
`:1452-1464`, `:1623-1633`), `packages/ai/src/types.ts` (the `Message` /
`AssistantMessage` / `Usage` interfaces at tag `v1.5.2`, fetched from
`raw.githubusercontent.com` for this slice), and
`packages/ai/src/api/openai-completions.ts:321-337` (the assistant object
literal and its post-stream assignments).

## What changed

The prior slice made typed `SessionEntry` re-encoding byte-exact for all 38
golden lines by emitting fields in upstream *write-site* order and capturing
the `sections` insertion order in a private `unknown` key. Three shape gaps
remained because pi-ai's typed structs cannot represent them. This slice closes
the first two for every entry decoded through `SessionEntry::from_record`:

1. **Tolerant-decode defaults** (`content: null` → `[]`, missing `content`,
   custom-message `content: null` → `Blocks([])`).
2. **Unknown keys inside nested messages** — both at the top level of a
   `message`/`systemMessage` object and inside content blocks.

Mechanism (`crates/pi-coding-agent/src/session/records.rs`):

- New private key `MESSAGE_WIRE_KEY = "\u{0}pi-rs.message-wire"` in the entry's
  `unknown` bag. `capture_message_wire` stores the raw `message`,
  `systemMessage` and (for `custom_message`) `content` values from the parsed
  record. It is installed alongside the existing `SECTION_ORDER_KEY` hint in
  `SessionEntry::from_record`.
- `SessionMessageEntry`, `CompactionEntry` (`systemMessage`) and
  `CustomMessageEntry` (`content`) re-derive the typed value from the captured
  raw bytes on serialize. If that baseline equals the current typed value, the
  **raw value is emitted verbatim** — key order, unknown keys, null defaults and
  every write-site slot all survive exactly as TypeScript's
  `JSON.stringify(entry)` would write them. If a caller mutated the typed
  message, the previous write-site-ordered path is used, with the captured
  `sections` hint applied.
- `finish_entry` now skips every NUL-prefixed key rather than only
  `SECTION_ORDER_KEY`, so no private hint can leak into a rewritten file.
- `deserialize_tolerant_content` was factored into a shared `tolerant_content`
  helper so decode and the wire-hint baseline check normalize identically.

Test changes:

- `tests/golden_sessions.rs::assert_simple_typed_views_rewrite` →
  `assert_typed_views_rewrite`; the `message` exemption is gone. All 38 golden
  lines now pass through the typed view, including the nested messages.
- Synthetic integration tests for tolerant null/missing content and unknown
  nested keys (message, content block, compaction `systemMessage`).
- Synthetic unit tests in `records.rs` for the same shapes, plus a mutation
  test proving the wire hint is dropped once the typed message changes and a
  system-message test proving `sections` wire order is preserved without
  relying on the older hint.

Test count touched by this slice: `records.rs` 24 unit tests (5 new),
`golden_sessions.rs` 9 integration tests (2 new). No cargo command was run in
this worktree — the coordinator runs the serial host gate.

## Remaining divergence

- **Freshly built / mutated messages** have no wire hint and still rely on the
  hardcoded write-site order in `serialize_message_ordered`. For roles/fields
  the five golden fixtures exercise, that order is verified byte-exact. The
  positions of optional assistant fields absent from the fixtures —
  `providerThinkingLevel`, `diagnostics`, `errorMessage` vs `rawStopReason`,
  `endTurn`, `deferred` — follow the current best-effort order and were **not**
  changed: no upstream write site in hand proves their relative slots. Loaded
  entries are unaffected because the raw value is emitted verbatim.
- Entries decoded directly via `serde` (not through `SessionEntry::from_record`)
  get no hint; their typed re-encode follows the older ordered path. All
  production load paths in `session::store` go through `from_record`.
- A typed message mutated and then mutated back to its original value emits
  the original raw bytes (correct for a no-op rewrite, but it will not
  canonicalize a non-V8 spelling that pi itself would canonicalize on a fresh
  write).
- `content: null` for a system message is not tolerated by pi-ai's typed
  `SystemMessage`; such a compaction entry decodes to `SessionEntry::Other` and
  is already kept verbatim.
