# RPC prompt-preflight slice — host wiring contract

This file is the handoff from the **RPC prompt preflight** slice to the
coordinator. It records the new `RpcHost` callback, what the default
`LocalRpcHost` does with existing port seams, the preflight ordering the port
now reproduces, and the upstream paragraphs intentionally **not** reproduced
by the default. It supersedes/extends the command-table notes in the earlier
RPC-command slice's `WIRING.md` (preserved at
`/tmp/opencode/pi-rs-wt-rpc-commands/WIRING.md`).

Ground truth: `packages/coding-agent/src/modes/rpc/rpc-mode.ts` (1.5.2,
`:409-435`), `packages/coding-agent/src/core/agent-session.ts`
(`:2162-2263`, `:1181-1198`, `:2268-2292`),
`packages/coding-agent/src/core/auth-guidance.ts` (`:18-25`), and
`packages/coding-agent/src/core/model-runtime.ts` (`:486-554`). All code
locations below are in those files unless stated otherwise.

## What changed

`crates/pi-coding-agent/src/modes/rpc.rs` (only file changed in this slice):

- `prompt_preflight()` now runs **before** the authoritative `response` is
  written. It (1) resolves a leading slash command through
  `RpcHost::get_commands`, (2) reads the model/auth gate through the new
  `RpcHost::check_auth`, and (3) returns the `PromptDisposition`.
- The response is emitted once, with `data.disposition` from the preflight.
  A preflight failure returns `Err`, which `run_rpc_mode` writes as the single
  `success: false` frame (`command: "prompt"`, the request `id`, no `data`
  key) and no run starts.
- Failures from `run_agent_turn` remain swallowed (the upstream `.catch`
  checks `preflightSucceeded` at `rpc-mode.ts:425-429`). A provider error
  surfaces as `agent_end`/message events, never as a second response for the
  same id.
- The delivery-time `queue_update` is re-emitted from a new `QueuedTexts`
  mirror on `RpcBridge`: `steer`/`follow_up` record the text at enqueue;
  `RpcEventSink::emit` removes it on the matching user `message_start` and
  writes the reduced `queue_update` *before* forwarding that event. This is
  upstream `_handleAgentEvent` (`:1181-1198`) — the agent loop drains the real
  queues before delivery, so the mirror is what still knows the text was
  queued. The mirror is cleared wherever `bridge.queues` is reset
  (`new_session`/`switch_session`/`fork`/`clone`/`clear_queue`).

No change is needed to `main.rs`, `lib.rs`, other modes, `session/**`,
`prompt.rs`, `cli/**`, `Cargo.toml`, or `Cargo.lock`.

## New host callback the coordinator must add

The `RpcHost` trait gained one **required** method. Any existing external
implementor (including the earlier RPC-command slice's example host in its
`WIRING.md` §"The seam to override") must add it:

```rust
pub struct RpcAuthCheck {
    pub configured: bool,     // hasConfiguredAuth || checkAuth !== undefined
    pub using_oauth: bool,    // isUsingOAuth — selects the verbatim refusal
}

fn check_auth(
    &self,
    runtime: &pi_coding_agent::modes::print_mode::PrintRuntime,
    provider: &str,
) -> Result<RpcAuthCheck, String>;
```

`configured: false` means preflight failure: no disposition, no run. `Err`
means a thrown preflight exception and is forwarded as the verbatim refusal.
`RpcAuthCheck`, `PromptDisposition`, `QueuedTexts`, `RpcHost`, `LocalRpcHost`
and `RpcBridge::queued_texts` are all `pub` from
`pi_coding_agent::modes::rpc`.

The existing `get_commands` callback is now also consulted by preflight (not
just by `get_commands`): a command object with a matching `name` and
`source: "extension"` is the upstream "extension command consumed the prompt"
case (`"handled"`). A full host should therefore return the **same** command
table from both call sites.

### Default `LocalRpcHost::check_auth`

Resolves the port's credential ladder again for the *selected* provider
(`print_mode.rs:689-705`, `resolve_wire_credential`) using the runtime's
flag credential and `crate::config::get_auth_path()`. `configured` is true when
a wire credential resolves or an access token is present; `using_oauth` is
derived from the provider's composed auth methods
(`oauth::provider_auth_methods`) or the access token. Re-resolving per
provider means a `set_model` switch to a different provider is reflected even
though `PrintRuntime::api_key` was resolved for the startup model.

A host with a live `ModelRuntime` should override this with
`hasConfiguredAuth(provider) || (await checkAuth(provider)) !== undefined` and
`isUsingOAuth(provider)` (`:2247-2256`) so dynamic/OAuth refreshes are
observed exactly as upstream does.

## Preflight semantics reproduced

| Case | Disposition / response | Run? |
|---|---|---|
| Missing `message` | error `Cannot read properties of undefined (reading 'startsWith')` | no |
| `/<known extension command> ...` | `success: true`, `disposition: "handled"` | no |
| `/<prompt template>` / `/skill:<name>` (registry match) | `success: true`, `disposition: "started"` | yes |
| `/<unknown>` or plain text | `success: true`, `disposition: "started"` | yes |
| No credential (`using_oauth: false`) | error `formatNoApiKeyFoundMessage(provider)` | no |
| No credential (`using_oauth: true`) | error `Authentication failed for "<provider>"... Run '/login <provider>'...` | no |
| Any failure after the response | nothing extra — swallowed | run already accepted |

Both refusal strings are verbatim from `auth-guidance.ts:18-25` and
`agent-session.ts:2252-2259`; `format_no_api_key_message` renders the
`unknown` provider id as "the selected model" and uses
`crate::prompt::get_docs_path()` for the two docs links (the port's XDG path
root, per the standing deviation).

## Deliberate deviations (default host)

1. **No extension-command handler execution.** Upstream's
   `_tryExecuteExtensionCommand` (`:2268-2292`) invokes the registered
   handler, which manages its own `pi.sendMessage()` traffic. The port has no
   command-execution seam in RPC mode; the default reports `"handled"` and
   starts no run. A full host should execute the handler behind `RpcHost` and
   let it drive the session before reporting the disposition.
2. **No template/skill body expansion.** Upstream expands `/template` and
   `/skill:name` (`:2219-2224`, `:2294-2323`) into the text sent to the model.
   The default resolves the registry `source` but sends the original text;
   the expansion needs `session.promptTemplates` bodies and the skill file
   contents, which `RpcSlashCommand` does not carry. A full host should
   expand with `crate::prompt_templates::expand_prompt_template` and the skill
   reader before the run. Recommended follow-up host callback:
   `fn expand_prompt(&self, runtime: &PrintRuntime, message: &str)
   -> Result<Option<String>, String>` (default `Ok(None)`), called by
   `prompt_preflight` between resolution and the auth gate.
3. **No input handlers / no `"handled"` from them.** `_runInputHandlers`
   (`:2206-2216`) can consume a prompt before template expansion. The port's
   extension host has no RPC input-handler surface, so this disposition arm is
   reachable only through a future host override.
4. **No `"queued"` disposition.** The port handles one command per thread and
   never streams during a `prompt` command, so
   `streamingBehavior`/`_preparePrompt`'s streaming branch
   (`:2226-2240`) is unreachable. `PromptDisposition::Queued` is defined for
   enum parity and for a future concurrent host.
5. **No no-model branch.** `PrintRuntime::model` is never absent, so
   `formatNoModelSelectedMessage()` (`:2242-2245`) is unreachable;
   `set_model` already validates names against the snapshot.
6. **Queue mirror is text-based.** Upstream matches a delivered user message
   to the queue by `contentText` (`:1185-1197`). The port records the
   preflight-time text; a host that transforms queued text mid-run should
   update `RpcBridge::queued_texts` accordingly. Images are not part of the
   match, exactly as upstream's `contentText` ignores them. The port drains a
   whole `all`-mode queue at once, so one `queue_update` covers the batch
   rather than one per delivered message.

## Tests

Six new tests in `mod tests` (file total: 25 `#[test]` functions, 19 before
this slice) cover: a known extension
command (`"handled"`, no run — guarded by a `NeverStream` that would panic),
template/skill/unknown slash forms (`"started"`), the API-key refusal, the
OAuth refusal, response-before-events ordering with a post-acceptance provider
error producing no second response, and the enqueue/delivery `queue_update`
pair with the reduced queue crossing before `message_start`. Existing host
doubles gained the required `check_auth` method.
