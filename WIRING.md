# Wiring: Google Generative AI + Google Vertex streaming adapters

This file is the handoff to the coordinator. It lists the exact registration
changes needed in files this slice was not allowed to touch, plus every
deliberate deviation from upstream.

## New files

- `crates/pi-ai/src/providers/google.rs` — the two `StreamFn` entry points,
  `reqwest` request construction, credential resolution, endpoint construction,
  and the shared `run` wrapper that emits `Start`/`Done`/`Error` like the other
  adapters.
- `crates/pi-ai/src/providers/google_shared.rs` — thinking-level/budget mapping,
  message and tool conversion, stop-reason/usage mapping, and the SSE chunk →
  `AssistantMessageEvent` state machine (identical upstream loop body).

No other files were modified.

## Exact registration lines

In `crates/pi-ai/src/providers/mod.rs`, add the module declarations next to the
existing ones:

```rust
/// Google Generative AI + Google Vertex adapters (TS `api/google-generative-ai.ts`,
/// `api/google-vertex.ts`, `api/google-shared.ts`).
pub mod google;
pub mod google_shared;
```

and add these two arms inside `stream_fn_for`:

```rust
        "google-generative-ai" => Some(google::stream_google_generative_ai),
        "google-vertex" => Some(google::stream_google_vertex),
```

`google_shared.rs` is a private implementation detail of `google.rs` but must be
declared as a module for Rust to compile it; `pub mod` is used above for
consistency with the existing `pub mod common;`. If the coordinator prefers,
`mod google_shared;` also works (all its API is `pub(crate)`).

The existing `dispatches_known_apis` test in `mod.rs` can be extended with:

```rust
        assert!(stream_fn_for("google-generative-ai").is_some());
        assert!(stream_fn_for("google-vertex").is_some());
```

## Dependencies

**No new dependencies.** Everything uses `reqwest`, `serde_json`, `futures`,
`tokio`, and `tokio-stream` already declared in `crates/pi-ai/Cargo.toml`.
No changes to root `Cargo.toml` or `Cargo.lock` are needed.

## Semantics ported

- Event sequence per chunk: `Start` → `TextStart`/`TextDelta`/`TextEnd`,
  `ThinkingStart`/`ThinkingDelta`/`ThinkingEnd`, `ToolcallStart`/`ToolcallDelta`/
  `ToolcallEnd` → `Done` (or terminal `Error`). Text/thinking blocks close and
  reopen when a chunk flips `thought`, and a `functionCall` closes the open
  text/thinking block before emitting its three events (`google-*.ts:106-221`).
- Thought signatures: `thought: true` alone marks thinking;
  `retainThoughtSignature` keeps the last non-empty signature across deltas;
  signatures are validated (base64, length % 4) and only replayed for the same
  provider/model (`google-shared.ts:115-162`).
- Tool IDs: a provided call ID is reused unless it is empty or already present;
  otherwise `{name}_{Date.now()}_{counter}` is generated
  (`google-generative-ai.ts:195-201`).
- Finish reasons: `STOP` → `stop`, `MAX_TOKENS` → `length`, everything else
  (safety, blocklist, recitation, …) → `error`; a tool call with `STOP` becomes
  `toolUse` (`google-shared.ts:500-528`). Missing finish reason, `SAFETY`-style
  stops, and streamed `error` chunks become terminal errors with upstream's
  message strings.
- Usage: `input = promptTokenCount - cachedContentTokenCount`,
  `output = candidatesTokenCount + thoughtsTokenCount`,
  `cacheRead = cachedContentTokenCount`, `reasoning = thoughtsTokenCount`,
  `totalTokens = totalTokenCount`, followed by catalog cost calculation
  (`google-generative-ai.ts:232-251`).
- Thinking config: `includeThoughts`, `thinkingLevel` (Gemini 3+/4/Gemma 4) or
  `thinkingBudget` (2.5 family) with the exact per-family budget tables, plus
  `getDisabledGoogleThinkingConfig` (`google-shared.ts:39-113`,
  `google-generative-ai.ts:426-466`).
- Request body follows the SDK converter: `contents`, `systemInstruction`,
  `tools`, `toolConfig` at the top level and `temperature`, `maxOutputTokens`,
  `thinkingConfig` inside `generationConfig`
  (`@google/genai _models_converters.ts:2241-2322`). Tool schemas are emitted as
  `parametersJsonSchema` with `const` rewritten to single-value `enum` and
  JSON-Schema meta declarations stripped for the legacy path
  (`google-shared.ts:347-460`).
- Message conversion: leading system prompt → `systemInstruction`; user text /
  images → `user` parts with `inlineData`; assistant text/thinking/tool calls →
  `model` parts with signature replay; tool results → merged `functionResponse`
  parts (multimodal on Gemini 3+); orphan tool calls get synthetic "No result
  provided" responses; errored/aborted assistant turns are skipped
  (`google-shared.ts:193-345`, `transform-messages.ts:179-255`).
- Endpoint/auth: Gemini uses `https://generativelanguage.googleapis.com/v1beta`
  (or `model.base_url` verbatim) with `x-goog-api-key`; Vertex uses
  `{location}-aiplatform.googleapis.com/v1` with
  `projects/{project}/locations/{location}/publishers/google/models/{id}` and
  either `x-goog-api-key` or `Authorization: Bearer` from `access_token`
  (`google-vertex.ts:358-446`).

## Deviations from upstream (all deliberate)

1. **No custom `fetch`, `signal`, `env`, `project`, `location`, `thinking`, or
   `maxRetries` fields** exist on the Rust `StreamOptions`. Consequently:
   - Abort checks after the stream are omitted (upstream
     `google-generative-ai.ts:272-274`). Callers cancel by dropping the event
     stream / reqwest future.
   - `retryGoogleRequest` (SDK 408/409/429/5xx backoff) is not implemented in
     the adapter; the port's retry policy lives in `pi-ai::retry` and is applied
     by the caller (same as the existing OpenAI/Anthropic adapters).
   - Project/location resolve from `StreamOptions.metadata["project"]` /
     `["location"]`, then `GOOGLE_CLOUD_PROJECT` / `GCLOUD_PROJECT` /
     `GOOGLE_CLOUD_LOCATION`. An additive `metadata["toolChoice"]` seam exposes
     `"any"` (the Rust `ToolChoice` enum only has `Auto`/`None`).
   - `metadata["thinkingLevel"]` and `metadata["thinkingBudgets"]` map to the
     exact upstream `GoogleOptions.thinking.level`/`budgetTokens` knobs; absent
     those, `options.reasoning` drives thinking like the Anthropic adapter.
2. **Vertex service-account ADC is not minted locally.** Upstream's
   `GOOGLE_APPLICATION_CREDENTIALS` path requires an OAuth JWT signer which the
   crate does not depend on. Callers pass the resulting OAuth token through
   `StreamOptions.access_token` (bearer) or a Vertex API key through
   `options.api_key`. The `gcp-vertex-credentials` marker and `<placeholder>`
   keys are treated as "no API key", matching upstream's `resolveApiKey`.
3. **Grounding metadata and safety ratings are not normalized into content.**
   Upstream only observes them through `onProviderStreamEvent`; the whole raw
   chunk is handed to that hook before mapping, so pass-through is preserved.
   Safety *finish reasons* are mapped to `error` as upstream. An additive
   `metadata["safetySettings"]` seam lets callers send the SDK-shaped settings
   array even though upstream's `buildParams` never sets one.
4. **Custom base URL with `{location}`** is ignored in favour of the regional
   host (upstream delegates the template to the SDK); a concrete custom base
   URL is used verbatim, with `/v1` appended only when it has no `vN` segment.
5. **`sanitizeSurrogates` is a no-op** — Rust strings cannot hold lone UTF-16
   surrogates.
6. **Strict tool sampling** is ported at the decision level (`VALIDATED` mode
   for Gemini 3+ JSON-schema tools that declare `constrainedSampling`). The
   schema rewriting is a faithful subset: it rejects the upstream unsupported
   keyword list and normalises object roots, but does not implement upstream's
   optional-null expansion for every property.
7. **System-message collapse** renders the first system message with
   `system_text` and later ones with `system_update_text`, joined with `\n\n`.
   Upstream's `getCurrentSystemMessage` replays named sections by name (last
   write wins); the Rust port has no equivalent replay helper in `common.rs`.

## Tests

15 unit tests total (10 in `google_shared.rs`, 5 in `google.rs`): canned SSE
text-only, thinking-with-signature-retention, tool-call, streamed error,
safety finish reason, missing finish reason, thinking budgets/levels, request
body shape, message/tool conversion, grounding pass-through, endpoint URLs,
credential precedence, and API-version detection.
