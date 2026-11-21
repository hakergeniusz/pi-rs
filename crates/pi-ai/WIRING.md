# pi-ai adapter hook wiring (`feat/ai-stream-hooks`)

Ports the built-in adapters' `onPayload` / `onResponse` /
`onProviderStreamEvent` instrumentation (upstream `packages/ai/src/types.ts:155-232`,
`api/simple-options.ts:49-80`). Until this slice only the JS extension path
invoked hooks (`pi-ext-js/src/glue.rs:2421-2458`); the Rust adapters never did.

## Hook types (`crates/pi-ai/src/types.rs`)

- `ProviderResponse { status: u16, headers: BTreeMap<String, String> }` — TS
  `ProviderResponse`; header names lowercased, last value wins (`headersToRecord`).
- `PayloadHook` / `ResponseHook` / `StreamEventHook` — trait-object callbacks
  returning `HookFuture<'static, …>` (`futures::future::BoxFuture`), so both
  synchronous and promise-returning upstream hooks are representable. Arguments
  are owned (`JsonValue`, `Model`, `ProviderResponse`) to keep the future
  `'static`.
- `ProviderHook<T>` — optional `Arc<T>` slot implementing `Debug`/`Clone`/
  `Default` so `StreamOptions` keeps its derives. Hooks are opaque to `Debug`,
  as upstream hook functions are opaque to serialized options.
- `StreamOptions::{on_payload, on_response, on_provider_stream_event}` — new
  fields, defaulting to unset (no-op). All existing struct literals already use
  `..Default::default()`.

## Semantics ported (`crates/pi-ai/src/providers/common.rs`)

Shared helpers `apply_payload_hook`, `apply_response_hook`,
`apply_stream_event_hook`, `response_metadata`:

| Hook | Call point | Semantics |
|---|---|---|
| `on_payload` | After the request body is built, before `.send()` | Receives the built payload + model. `Some(replacement)` replaces the body; `None` keeps it; `Err` becomes the stream's terminal error and no request is sent. |
| `on_response` | After `.send().await`, before `read_sse_stream` / `start` | One-way status+headers observer; `Err` fails the stream before any protocol event is emitted. |
| `on_provider_stream_event` | Per parsed SSE event, before Pi normalization | One-way observer; `Err` fails the stream after `start`. |

Ordering: `on_payload` → (`send`) → `on_response` → `start` →
`on_provider_stream_event` × N → `done`/`error`, matching upstream.

## Adapters covered

All three built-in Rust adapters (the only ones the port has):

- `openai-completions` — `api/openai-completions.ts:375-378,392,545`.
  The OpenAI `[DONE]` sentinel is observed as `Value::String("[DONE]")`.
  `onResponse` is gated on 2xx because upstream's SDK throws before
  `withResponse()` returns on an error status.
- `anthropic-messages` — `api/anthropic-messages.ts:905-908,943,954`.
  `onResponse` likewise gated on 2xx (SDK `asResponse()` throws on errors).
  The port has no strict-tools retry loop, so `onPayload` runs once.
  SSE `error` events throw before any event is yielded, so the observer never
  sees them — matching `iterateAnthropicEvents`.
- `openai-codex-responses` — `api/openai-codex-responses.ts:287-290,433-436,772`.
  `onResponse` is unconditional (upstream calls it before `if (response.ok)`),
  including non-2xx statuses. SSE transport only (WebSocket deviation already
  documented in the module).

## Adapters remaining

None in this repository — `stream_fn_for` dispatches only those three APIs.
Upstream adapters with no Rust counterpart (openai-responses, azure,
google-generative-ai, google-vertex, bedrock-converse-stream, mistral,
pi-messages, openrouter-images, classifier/llama-cpp helpers, faux) are out of
scope; their hook call sites are documented in the upstream source but there is
no Rust adapter to wire.

## Call-site changes for the coordinator to verify in `pi-coding-agent`

- `crates/pi-coding-agent/src/modes/print_mode.rs:461-475` constructs
  `pi_ai::types::StreamOptions` with `..Default::default()`, so the built-in
  adapters now run with all three hooks unset (no-op). No compile change is
  needed. If the print path should surface extension hooks on built-in
  providers, it must populate the three slots; this slice deliberately does
  not change `pi-coding-agent`.
- `crates/pi-coding-agent/src/models_state.rs:464-517` documents that the
  fn-valued hooks cross in the stream glue, not in the marshalled JSON. That
  remains true for the JS custom-provider path; the new Rust slots are a
  separate, Rust-callable mechanism and do not affect the JS bridge.
- `crates/pi-ext-js/src/glue.rs:2421-2458` decorates JS stream options with
  `onPayload`/`onResponse`/`onProviderStreamEvent` for extension-provided
  stream functions. Those handlers call the hooks themselves in JS; the
  built-in Rust adapters do not go through this glue, so they now use the Rust
  slots instead. No change needed in `pi-ext-js`.

## Tests

Ten tests total:

- `crates/pi-ai/tests/streaming.rs` — eight async tests over mock SSE servers:
  call order + argument shapes on all three adapters, payload replacement
  reaching the wire (request body captured), default no-op behavior, and error
  propagation for each hook (including "no request sent" on payload rejection).
- `crates/pi-ai/src/providers/common.rs` — two unit tests for the shared
  helpers: unset slots are no-ops, a returned replacement wins, and hook
  errors propagate through the `Result`.
