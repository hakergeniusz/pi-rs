# pi-rs

A Rust rewrite of [Pi](https://github.com/earendil-works/pi) — the minimal,
extensible agent harness — aiming for behavioral and on-disk compatibility
(session JSONL v3, print/JSON/RPC output schemas, `~/.pi` config directories).

**Status: pre-alpha, phase P0 (skeleton).** See [PLAN.md](PLAN.md) for the full
phased plan.

## Crates

| Crate | Port of | Status |
|---|---|---|
| `pi-schema` | `@earendil-works/pi-schema` | stub |
| `pi-ai` | `@earendil-works/pi-ai` | stub |
| `pi-agent` | `@earendil-works/pi-agent-core` | stub |
| `pi-tui` | `@earendil-works/pi-tui` | stub |
| `pi-mcp` | `@earendil-works/pi-mcp` | stub |
| `pi-codemode` | `@earendil-works/pi-codemode` | stub |
| `pi-coding-agent` (`pi` binary) | `@earendil-works/pi-coding-agent` | stub |

Deferred (not in the core scope): `chord`, `pi-durable`, `apps/p2`.

## Build

```sh
cargo build
cargo test
cargo clippy --workspace --all-targets -- -D warnings
```

## Ground rules

- MIT licensed (original Pi by Mario Zechner is MIT; attribution preserved).
- Session files, CLI output schemas and config dirs stay byte/shape-compatible
  with the TypeScript implementation.
- Provider integration tests run only against free or local endpoints.
