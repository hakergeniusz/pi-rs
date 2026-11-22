# Wiring: `mistral-conversations` + `pi-messages` adapters

This file is hand-off instructions for the coordinator. Per the slice contract I
did **not** edit `providers/mod.rs`, `lib.rs`, root `Cargo.toml`, `Cargo.lock`,
`HANDOFF.md`, or `AGENTS.md`. All changes are confined to:

- `crates/pi-ai/src/providers/mistral.rs` (new)
- `crates/pi-ai/src/providers/pi_messages.rs` (new)

Both files were formatted with single-file `rustfmt --edition 2024`. No cargo
command was run in this worktree (the coordinator gates serially).

## Registration lines for `crates/pi-ai/src/providers/mod.rs`

Add the two module declarations alongside the existing ones:

```rust
/// Mistral native Chat Completions adapter (TS `api/mistral-conversations.ts`).
pub mod mistral;
/// pi's own serialized assistant-message event protocol (TS `api/pi-messages.ts`).
pub mod pi_messages;
```

Extend `stream_fn_for`:

```rust
        "mistral-conversations" => Some(mistral::stream),
        "pi-messages" => Some(pi_messages::stream),
```

Extend the `dispatches_known_apis` unit test with:

```rust
        assert!(stream_fn_for("mistral-conversations").is_some());
        assert!(stream_fn_for("pi-messages").is_some());
```

`KnownApi::MistralConversations` / `KnownApi::PiMessages` and their wire names
already exist in `types.rs`, so no type changes are needed. `lib.rs` already
re-exports `providers`; no change needed there either.

## Dependencies

No new crate dependencies. The adapters use only what `pi-ai` already has:
`futures`, `serde`, `serde_json`, `reqwest` (including its re-exported `Url`),
`tokio`, and in-crate modules `event_stream`, `sse`, `streaming_json`,
`providers::common`, `cost`, `constrained_sampling`, `types`.

## Semantics implemented

### `pi_messages.rs` (TS `pi-messages.ts:1-465`)

- Request: `POST <baseUrl>/messages` with `{ model, context, options }`;
  `reasoning`, `cacheRetention` (incl. `PI_CACHE_RETENTION=long` fallback),
  `sessionId`, `toolChoice`, `temperature`, `maxTokens`; optional `?debug=1`.
- SSE framing: first `data:` line per event, `[DONE]` skipped, CR/LF
  normalized by the shared `SseParser`, trailing unterminated event flushed.
- `contentIndex` validation exactly as TS lines 198-206 (starts may append,
  other events must address the existing last block).
- Full converter: `start`, text/thinking/toolcall start–delta–end, `done`,
  `error`; wire `usage` copied into the partial; `responseId`,
  `responseModel`, `providerThinkingLevel` and rewrite diagnostics applied.
- **Terminal-event handling (the local-model bug):** after the SSE stream is
  exhausted, absence of a `done`/`error` wire event emits a terminal `error`
  event with message `<provider> stream ended without a terminal event`.
  A wire terminal event stops the loop immediately; later events are ignored.
- Non-2xx responses produce `PiMessagesResponseError` with the TS diagnostic
  shape (`version`, `provider`, `model`, `url`, `status`, `statusText`,
  `error`/truncated `body`, `timestampMs`) and a `pi_messages_response_failure`
  diagnostic on the error message.

### `mistral.rs` (TS `mistral-conversations.ts:1-924`)

- Request: `POST <base>/v1/chat/completions`, bearer auth, `x-affinity` for
  prompt caching, model/options header overrides, camelCase→snake_case wire
  remap (`maxTokens`→`max_tokens`, `toolChoice`→`tool_choice`,
  `imageUrl`→`image_url`, nested `response_format.json_schema`, …).
- Message replay: system text/updates, user text + `image_url` data URLs,
  assistant thinking blocks (`type: "thinking", thinking: [{type,text}]`),
  tool calls, tool results with `buildToolResultText`, 9-char alphanumeric
  tool-call id normalization with collision avoidance.
- Stream lifecycle: `start` after response headers; text/thinking block
  switching with `text_end`/`thinking_end`; tool-call accumulation by `index`
  with tolerant argument parsing and `toolcall_end` after the loop; usage with
  all six cached-token spellings, clamping and `calculateCost`.
- `[DONE]` is a *normal* end marker (unlike pi-messages), finish reasons map to
  stop/length/toolUse/error with the exact TS error strings, and a missing
  finish reason or an error stop reason becomes a terminal `error` event.

## Deviations from upstream (all intentional, Rust surface constraints)

1. **No abort signal.** `StreamOptions` has no `signal`; transport failures
   always use `StopReason::Error`. Wire `error` events still honor `aborted`.
2. **Timeouts.** `MistralOptions.timeoutMs` (60 s header-only) is not
   representable; a single 600 s reqwest client timeout covers the request.
3. **`sanitizeSurrogates` is a no-op.** Rust `String`/serde output is valid
   UTF-8 and cannot contain unpaired UTF-16 surrogates.
4. **Streaming JSON.** Upstream `ToolCallArgumentsStream` is replaced by this
   repo's tolerant `parse_tool_call_arguments` applied on each delta; the
   terminal `finish()` behavior (full parse + `argumentsError`) is preserved.
5. **Provider-specific options via `metadata`.** Rust `StreamOptions` has no
   `debug`, `toolChoice` variants beyond `auto`/`none`, `promptMode`, or
   `reasoningEffort`. Raw values are read from `options.metadata` under those
   exact keys; derived reasoning settings still come from
   `model.thinking_level_map` and `options.reasoning` as in `streamSimple`.
6. **Strict JSON schema.** `makeStrictJsonSchema` is not ported (no Rust
   equivalent in `constrained_sampling.rs`). Mistral's `strict` flag is only
   set for simple object schemas without unsupported keywords; `parameters`
   are passed through unchanged.
7. **`resolveTranscript` / identity headers.** Not present in `pi-ai`; omitted.
   `transformMessages` is reduced to the behaviors Mistral needs (image
   downgrade, foreign-thinking drop, tool-call id normalization, skipping
   errored turns, synthetic orphan results). Mid-conversation system messages
   held behind pending tool calls are not implemented.
8. **SSE framing.** The shared `SseParser` (`\n`, `\r\n`, `\r`) is used instead
   of Mistral's broader boundary regex; all observed Mistral-compatible
   relays use one of those three.
9. **Unknown wire event types** in pi-messages are rejected by serde (error
   event) rather than forwarded with an extra `partial` field.
10. **`streamSimple`** is not a separate entry point: the Rust
    `StreamOptions` already carries `reasoning` and `tool_choice`.
11. **HTTP status error bodies** from `read_sse_stream` are capped at 2048
    chars by the shared SSE helper; the explicit non-2xx paths keep upstream's
    Mistral 4000-char limit and pi-messages 8192-char diagnostic truncation.

## Tests

18 tests total (8 in `pi_messages.rs`, 10 in `mistral.rs`), covering canned
SSE streams for text, thinking, tool calls, terminal `done`/`error` events,
`[DONE]` handling, missing-terminal and missing-finish-reason errors, request
payload/wire remapping, tool-call id derivation, usage/cost mapping, error
formatting, `contentIndex` validation, and rewrite diagnostics. `mod tests` is
last in both files; clippy `too_many_lines` allowances mirror upstream and are
annotated in place.
