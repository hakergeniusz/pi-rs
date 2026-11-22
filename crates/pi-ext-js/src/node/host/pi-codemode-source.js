// `@earendil-works/pi-codemode/source` virtual module — the lightweight entry
// the bundled codemode extension imports (`extensions/codemode/tool.ts:33`).
// Upstream: `packages/codemode/src/source.ts:18-30`. The constant below is a
// byte-for-byte copy of `crates/pi-codemode/src/source.rs:17-25`
// (`CODEMODE_SOURCE_GRAMMAR`); the hostpkg unit test pins the two together.
// `parseCodemodeSource` and `CodemodeSourceError` remain Rust-only — the
// codemode extension uses `resolveSchemaRef` from `/declarations` (already
// served by the E1 shim) and this grammar string for constrained sampling.

export const CODEMODE_SOURCE_GRAMMAR = String.raw`
start: options_source | plain_source
options_source: OPTIONS_LINE NEWLINE SOURCE
plain_source: SOURCE

OPTIONS_LINE: /[ \t]*\/\/ @options:[^\r\n]*/
NEWLINE: /\r?\n/
SOURCE: /[\s\S]+/
`;

/** Prefix of the optional first-line directive (`source.ts:11`). */
export const CODEMODE_OPTIONS_PREFIX = "// @options:";

export default { CODEMODE_SOURCE_GRAMMAR, CODEMODE_OPTIONS_PREFIX };
