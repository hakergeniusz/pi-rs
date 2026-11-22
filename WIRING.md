# Cache warmer — coordinator wiring notes

The state machine lives in
`crates/pi-coding-agent/src/session/cache_warmer.rs` and is registered from
`session/mod.rs`. It is **not** wired into any host yet: this slice ships the
self-contained port plus tests. The coordinator must add the calls below.

All line numbers are from the worktree at the time of the port; upstream sites
are from `earendil-works/pi` 1.5.2.

## 1. Implement `CacheWarmerHost` once, in the CLI layer

`CacheWarmerHost` (`session/cache_warmer.rs`) is the only injection point. A
host implementation belongs next to the existing runtime assembly in
`crates/pi-coding-agent/src/modes/print_mode.rs` (`PrintRuntime`) or a new
`modes/cache_warming.rs`, and must provide:

| Method | Port source / upstream site | What to do |
|---|---|---|
| `now_ms()` | `Date.now()` | `chrono::Utc::now().timestamp_millis()` (or the loop's monotonic clock). |
| `mode()` | `getCacheWarmingMode`, `config/settings.rs:1823-1829` | `CacheWarmingMode::from_mode_str(&settings.get_cache_warming_mode())`. The mode is **global-only** upstream (settings-manager.ts:941). |
| `last_prompt_tokens()` | `lastPromptTokens(getBranch())`, cache-warmer.ts:23-33 | `last_prompt_tokens(&session.get_branch(None).unwrap_or_default())`. The pure helper is exported from the module. |
| `decide(event)` | `extensionRunner.emitCacheWarmingDecision`, sdk.ts:338 | If the extension runner has the handler, emit `cache_warming_decision` with the four event fields and return `Ok(action)`; otherwise return `Ok(event.action)`. Return `Err(())` on handler failure so the warmer falls back to pi's decision. Wrap the result in `Box::pin(futures::future::ready(...))` (or a real future). |
| `stream_warm(request)` | `modelRuntime.streamSimple(...).result()`, cache-warmer.ts:260-262 | Call the existing `StreamFn` with `request.model`, `request.context`, `request.options`. Honour `request.max_retries == 0` (disable retries) and `request.aborted` (abort an in-flight warm). Map the resulting `AssistantMessage` to `WarmOutcome { provider, model: response_model.unwrap_or(model), usage, stop_reason }`; return `Err` on transport error. |
| `append_usage(entry)` | `sessionManager.appendUsage`, store.rs:1611-1631 | `session.append_usage(&entry.kind, &entry.provider, &entry.model, entry.usage, entry.note.as_deref())`. The `cache_warm` kind is what `session/cache_stats.rs:159` already recognises. |
| `on_warmed(entry)` | `AgentSession` `onWarmed` hook, agent-session.ts:528-529 | Emit an `AgentEvent::EntryAppended` for the persisted usage entry so the interactive transcript can render the "Cache warmed: $…" line (`format_cache_warming_usage`). |

The crate already has a blanket impl for `&T`, so `CacheWarmer::new(&host)`
works when the host is owned elsewhere.

## 2. Construct after the session/runtime exist

Upstream: `new CacheWarmer(...)` at `core/sdk.ts:334-339`, immediately after
`buildRequestOptions` and before the `Agent` is created.

Port: construct once after both the `SessionManager` and the resolved
`stream_fn` exist — `main.rs:508-557` (`build_print_runtime`) is the natural
place; store the warmer (and the host) in `PrintRuntime`
(`modes/print_mode.rs:47`) / the interactive equivalent so all mode loops can
reach it. The warmer is per-session; recreate it when the session is replaced.

## 3. Start from the agent's stream function

Upstream: `streamFn` in sdk.ts:412-423 starts warming only for session
requests, with the `cacheContextIsCurrent(model)` predicate (sdk.ts:359-373):

```ts
if (options?.sessionId === sessionManager.getSessionId()) {
  cacheWarmer.start({ model, context, options: requestOptions }, cacheContextIsCurrent(model));
}
```

Port: wrap the `StreamFn` that is handed to `run_agent_loop`
(`modes/json_event.rs:399-405`, `modes/rpc.rs:1249-1254`, interactive
equivalent). Before delegating to `runtime.stream_fn`, when
`options.session_id == session.get_session_id()`:

1. build `CacheWarmRequest { model, context, options }` from the loop request;
2. build a `cacheContextIsCurrent` closure — capture the request's model plus
   the full message slice at start; return true only while the current model's
   `provider`/`id` still match, the captured slice is a prefix (element-wise
   identity, not just length) of the current messages, and the current
   transcript has not shrunk (sdk.ts:366-371);
3. call `warmer.start(request, is_current)`.

Requests from compaction/summaries use different routing ids and must **not**
call `start` — only session requests replace the cache entry.

## 4. Forward the lifecycle hooks

| Upstream | Port site | Call |
|---|---|---|
| `_emitAgentSettled`, agent-session.ts:1139-1140 | `modes/json_event.rs:562-566` and `modes/rpc.rs:1410-…` (just before the `AgentSettled` emit), interactive equivalent | `warmer.on_agent_settled()` |
| `setCacheWarmingMode`, agent-session.ts:1519-1523 | `modes/interactive/interactive_mode.rs:5350-5354` (`SettingsSelectorEvent::CacheWarmingModeChange`) after `settings.set_cache_warming_mode(&mode)` | `warmer.on_mode_changed()` |
| `dispose`, agent-session.ts:1493-1496 | session teardown / mode shutdown | `warmer.cancel()` (and clear `on_warmed`) |
| `status` getter, agent-session.ts:1514-1517 | `/session` info: `modes/interactive/interactive_mode.rs:1242` area (already prints `Mode: <cache_warming_mode>`) and the status line at ~6932 | `format_cache_warming_status(&warmer.status(), now_ms)` and `format_cache_warming_usage(total_cost, note)` for notices |

## 5. Drive the timer (the one structural deviation)

Upstream's `CacheWarmer` owns a `setTimeout` and calls its own private
`refresh()`. The port has no runtime dependency in the session layer, so the
host owns the timer:

```text
loop {
    let Some(at) = warmer.next_warm_at() else { break };
    sleep_until(at).await;          // tokio::time::sleep_until
    warmer.refresh().await;         // re-arms via schedule() on success/failure
}
```

`next_warm_at()` returns `None` when no run is armed or the context changed;
re-read it after every `refresh()`. A late call is safe: `refresh()` re-checks
mode, `is_current`, and the refresh deadline and stops with the same upstream
reasons (`"cache refresh deadline missed"`, `"one-hour safety limit reached"`,
`"30-minute idle safety limit reached"`).

## 6. Semantics preserved

* Replayability gate (Anthropic budget thinking without adaptive thinking).
* TTL/delay policy: 90% of TTL with a 10 s margin; no refresh at ≤ 10 s.
* Economics: `continuation × missCost − warmCost` with a $0.05 threshold;
  streaming probability 1.0, idle 0.15; `cacheWrite` price preferred when the
  model reports it, otherwise `input`.
* Decision hook: extension `action != pi.action` sets `extension_override`;
  `"stop"` ends warming with the upstream reason and is recorded on the
  inactive status; extension errors fall back to pi's decision.
* Refresh: one-token cap, no continuation, no retries, abort flag set by
  `clear_run`; `error`/`aborted` stop reasons and thrown errors never persist
  usage; successful refreshes append `cache_warm` usage with the optional
  `"extension override"` note and notify `on_warmed`.
* Fixed safety windows measured from the original request (`started_at`).
* All stop reasons and `/session` / transcript strings are byte-for-byte ported.

## 7. Known deviations

* **Timer ownership** — see §5 (host drives `refresh()` via `next_warm_at()`).
* **`PI_CACHE_RETENTION` env override** — `pi_ai::types::StreamOptions` has no
  `env` field; the default retention tier is `short`.
* **`cacheRetention: "none"`** — the Rust `CacheRetention` enum has only
  `Short`/`Long`, so the "request disabled prompt caching" stop reason is not
  reachable; such requests report "cache lifetime unavailable".
* **Retry/abort transport** — `StreamOptions` has no `max_retries` or
  `AbortSignal`; those are carried on `WarmStreamRequest` and the provider
  adapter must honour them.
