//! Virtual models: catalog entries that route each request to a physical
//! model. The selection (`model_change`, `agent.state.model`) may name a
//! virtual model; everything below the routing step only sees physical
//! models — providers stream them and assistant messages record them.
//!
//! Ports `core/virtual-models.ts` (1.5.2) in full, plus the two consumers
//! this slice wires:
//! - `ModelRuntime.resolveModel` (`core/model-runtime.ts:1040-1077`) as
//!   [`resolve_virtual_model`], with the physical-catalog lookup and the
//!   configured-auth check injected so the port can run against the static
//!   catalog and the credential ladder.
//! - the `prepareRequest` routing wrapper (`core/agent-session.ts:850-907`,
//!   `_installAgentRequestProjection`) as [`VirtualRouteHook`].
//!
//! Deviations for this slice (see HANDOFF.md): dynamic providers
//! (name/config registrations, native providers, OAuth, `streamSimple`) are
//! slice 6, so routers can only target physical models of the static
//! catalog; `reason: "retry"` cannot fire (the port has no auto-retry
//! subsystem); the routed-model compaction-threshold re-prepare and the
//! `entry_appended` emission for state entries are not wired; route
//! requests carry no abort signal (the runner's calls are synchronous).

#[cfg(feature = "ext-js")]
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::Value::Null;
use serde_json::{Map, Value, json};

#[cfg(feature = "ext-js")]
use pi_agent::agent_loop::PrepareRequest;
use pi_agent::agent_message::AgentMessage;
#[cfg(feature = "ext-js")]
use pi_agent::types::{PrepareRequestContext, TurnUpdate, clamp_thinking_level};
use pi_agent::types::{StreamFn, StreamRequest, ThinkingLevel};
use pi_ai::types::{Model, ModelInputModality, StopReason};

use crate::session::{CustomEntry, SessionEntry};

/// API id of virtual catalog entries. Requests for it fail unless routed
/// first (`VIRTUAL_MODEL_API`).
pub const VIRTUAL_MODEL_API: &str = "pi-virtual";

/// Custom entry type that stores router state on the session branch
/// (`VIRTUAL_MODEL_STATE_ENTRY`).
pub const VIRTUAL_MODEL_STATE_ENTRY: &str = "pi.virtual-model-state";

/// Why a request is being routed (`ModelRouteReason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelRouteReason {
    /// First request after a message the user wrote.
    User,
    /// Any other request in the agent loop, e.g. after tool results.
    Continuation,
    /// Automatic retry after a failed request (unreachable until the port
    /// grows the retry subsystem).
    Retry,
    /// A request outside the agent loop, e.g. a compaction summary.
    Direct,
}

impl ModelRouteReason {
    /// The wire name (`ModelRouteReason`).
    pub fn as_str(self) -> &'static str {
        match self {
            ModelRouteReason::User => "user",
            ModelRouteReason::Continuation => "continuation",
            ModelRouteReason::Retry => "retry",
            ModelRouteReason::Direct => "direct",
        }
    }
}

/// Whether a model names a virtual model (`isVirtualModel`). Failed routing
/// leaves the virtual model on its message.
pub fn is_virtual_model(model: &Model) -> bool {
    model.api == VIRTUAL_MODEL_API
}

/// The current branch as typed entries (`getBranch`'s `SessionEntry[]`).
pub fn branch_entries(session: &crate::session::SessionManager) -> Vec<SessionEntry> {
    session
        .get_branch_records(None)
        .iter()
        .map(SessionEntry::from_record)
        .collect()
}

/// Latest successful response (`findLatestResponse`). Its model is physical:
/// failed or aborted requests, including failed routing, are skipped.
pub fn find_latest_response(messages: &[AgentMessage]) -> Option<&pi_ai::types::AssistantMessage> {
    messages.iter().rev().find_map(|message| match message {
        AgentMessage::Assistant(assistant) => {
            if matches!(
                assistant.stop_reason,
                StopReason::Error | StopReason::Aborted
            ) {
                None
            } else {
                Some(assistant)
            }
        }
        _ => None,
    })
}

/// Latest router state a session branch stores for a virtual model
/// (`getVirtualModelState`): the last `pi.virtual-model-state` custom entry
/// naming the provider and model id.
pub fn get_virtual_model_state(
    branch: &[SessionEntry],
    provider: &str,
    model_id: &str,
) -> Option<Value> {
    branch.iter().rev().find_map(|entry| {
        let SessionEntry::Custom(CustomEntry {
            custom_type, data, ..
        }) = entry
        else {
            return None;
        };
        if custom_type != VIRTUAL_MODEL_STATE_ENTRY {
            return None;
        }
        let data = data.as_ref()?;
        if data.get("provider").and_then(Value::as_str) == Some(provider)
            && data.get("modelId").and_then(Value::as_str) == Some(model_id)
        {
            data.get("state").cloned()
        } else {
            None
        }
    })
}

/// The model selection a session branch records (`getBranchSelection`). A
/// virtual `model_change` holds until the next `model_change`, because
/// responses name the physical models it routed to; otherwise the latest
/// physical response wins. A virtual model that is no longer registered does
/// not hold, so the selection falls back to the physical model that answered
/// last.
pub fn get_branch_selection(
    branch: &[SessionEntry],
    get_model: &dyn Fn(&str, &str) -> Option<Model>,
) -> Option<(String, String)> {
    for (index, entry) in branch.iter().enumerate().rev() {
        if let SessionEntry::ModelChange(change) = entry {
            return Some((change.provider.clone(), change.model_id.clone()));
        }
        let SessionEntry::Message(message) = entry else {
            continue;
        };
        let AgentMessage::Assistant(response) = &message.message else {
            continue;
        };
        // Failed routing leaves the virtual model on its message (api
        // `pi-virtual`), so it never wins the selection.
        if is_virtual_model_api(&response.api) {
            continue;
        }
        // The response names a physical model; the last `model_change` before
        // it only wins when it selected a (still registered) virtual model
        // (`change && model && isVirtualModel(model) ? change : response`).
        if let Some(change) = find_last_model_change(branch, index) {
            if let Some(model) = get_model(&change.0, &change.1) {
                if is_virtual_model(&model) {
                    return Some(change);
                }
            }
        }
        return Some((response.provider.clone(), response.model.clone()));
    }
    None
}

/// `isVirtualModel` against an api name (the message form of the check).
fn is_virtual_model_api(api: &str) -> bool {
    api == VIRTUAL_MODEL_API
}

fn find_last_model_change(branch: &[SessionEntry], before: usize) -> Option<(String, String)> {
    branch[..before].iter().rev().find_map(|entry| match entry {
        SessionEntry::ModelChange(change) => {
            Some((change.provider.clone(), change.model_id.clone()))
        }
        _ => None,
    })
}

/// Build the catalog entry of a virtual model (`createVirtualModel`):
/// thinking levels default to `["off"]`, input to text and images, limits
/// and cost to unknown (0).
pub fn create_virtual_model(definition: &Value) -> Model {
    let id = definition
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let levels: Vec<&str> = definition
        .get("thinkingLevels")
        .and_then(Value::as_array)
        .map_or_else(
            || vec!["off"],
            |array| array.iter().filter_map(Value::as_str).collect(),
        );
    let mut thinking_level_map = Map::new();
    for level in ThinkingLevel::EXTENDED_ORDER {
        thinking_level_map.insert(
            level.as_str().to_string(),
            if levels.contains(&level.as_str()) {
                json!(level.as_str())
            } else {
                Null
            },
        );
    }
    let input = definition
        .get("input")
        .and_then(Value::as_array)
        .map_or_else(
            || vec![ModelInputModality::Text, ModelInputModality::Image],
            |array| {
                array
                    .iter()
                    .filter_map(Value::as_str)
                    .filter_map(|name| match name {
                        "text" => Some(ModelInputModality::Text),
                        "image" => Some(ModelInputModality::Image),
                        _ => None,
                    })
                    .collect()
            },
        );
    Model {
        id,
        name: definition
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        api: VIRTUAL_MODEL_API.to_string(),
        provider: definition
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        reasoning: levels.iter().any(|level| *level != "off"),
        thinking_level_map: Some(thinking_level_map),
        input,
        context_window: definition
            .get("contextWindow")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        max_tokens: definition
            .get("maxTokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        ..Model::default()
    }
}

/// The virtual catalog models behind registered definitions
/// (`runner.virtual_models()` payloads), in registration order.
pub fn virtual_models_from_defs(defs: &[(Value, String)]) -> Vec<Model> {
    defs.iter()
        .map(|(definition, _)| create_virtual_model(definition))
        .collect()
}

/// `withVirtualModels`' list semantics (`virtual-models.ts:201-238`) for one
/// provider's models: a virtual model hides a physical chat model with the
/// same id, and the virtual models append after the physical ones.
pub fn with_virtual_model_listing(mut models: Vec<Model>, virtuals: &[Model]) -> Vec<Model> {
    models.retain(|model| {
        !virtuals.iter().any(|virtual_model| {
            virtual_model.provider == model.provider && virtual_model.id == model.id
        })
    });
    models.extend(virtuals.iter().cloned());
    models
}

/// The physical model and thinking level for one request (`ModelRoute`),
/// parsed from the router's reply.
#[derive(Debug, Clone)]
pub struct ModelRoute {
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    /// New router state, stored on the session branch unless it is the
    /// request's own state (`undefined` keeps the current state).
    pub state: Option<Value>,
}

/// The inputs of one routing decision (`ModelRouteRequest`, minus the
/// abort signal — the runner's calls are synchronous).
#[derive(Debug)]
pub struct RouteRequest<'a> {
    /// The selected virtual model.
    pub model: &'a Model,
    /// Conversation for this request, including system messages (agent
    /// shape; converted to LLM messages for the router).
    pub messages: &'a [AgentMessage],
    pub reason: ModelRouteReason,
    pub thinking_level: ThinkingLevel,
    /// Router state last returned on this session branch. `None` before the
    /// first state and for `direct` requests.
    pub state: Option<Value>,
    /// For `retry`: the failed request, which `messages` no longer contains
    /// (unreachable until the retry subsystem lands).
    pub failed: Option<&'a pi_ai::types::AssistantMessage>,
}

/// The catalog and credential lookups `resolveModel` runs against the model
/// runtime, injected so the port runs against the static catalog and the
/// credential ladder.
#[derive(Clone, Copy)]
pub struct RouteResolvers<'a> {
    /// `getPhysicalModel`: a catalog model that is not virtual.
    pub get_model: &'a dyn Fn(&str, &str) -> Option<Model>,
    /// `hasConfiguredAuth`: whether a provider has configured credentials.
    pub has_credentials: &'a dyn Fn(&str) -> bool,
}

/// Ask a virtual model's router for the model and thinking level of one
/// request (`ModelRuntime.resolveModel`, `model-runtime.ts:1040-1071`). The
/// router must return a physical catalog model whose provider has
/// credentials; the thinking level is clamped to that model. Errors carry
/// the TS messages verbatim.
#[cfg(feature = "ext-js")]
#[allow(clippy::too_many_lines)] // mirrors ModelRuntime.resolveModel; split when extending
pub fn resolve_virtual_model(
    runner: &Mutex<pi_ext_js::ExtensionRunner>,
    request: &RouteRequest,
    resolvers: RouteResolvers,
) -> Result<ModelRoute, String> {
    let name = format!(
        "Virtual model {}/{}",
        request.model.provider, request.model.id
    );
    let registered = runner
        .lock()
        .expect("extension runner mutex poisoned")
        .virtual_models()
        .into_iter()
        .any(|(definition, _)| {
            definition.get("provider").and_then(Value::as_str)
                == Some(request.model.provider.as_str())
                && definition.get("id").and_then(Value::as_str) == Some(request.model.id.as_str())
        });
    if !registered {
        return Err(format!("{name} is not registered."));
    }

    // `previous`: the latest successful response's physical model; `failed`:
    // the failed response's physical model (the message keeps the agent
    // shape, like the TS request).
    let latest = find_latest_response(request.messages);
    let previous = latest
        .and_then(|response| (resolvers.get_model)(&response.provider, &response.model))
        .map(|model| {
            let mut entry = Map::new();
            entry.insert(
                "model".to_string(),
                serde_json::to_value(&model).unwrap_or(Null),
            );
            if let Some(level) = latest.and_then(|response| response.thinking_level) {
                // pi-ai levels serialize to their wire names (camelCase
                // rename): "off", "minimal", …
                entry.insert(
                    "thinkingLevel".to_string(),
                    serde_json::to_value(level).unwrap_or(Null),
                );
            }
            Value::Object(entry)
        });
    let failed = request.failed.and_then(|failed| {
        (resolvers.get_model)(&failed.provider, &failed.model).map(|model| {
            let mut entry = Map::new();
            entry.insert(
                "model".to_string(),
                serde_json::to_value(&model).unwrap_or(Null),
            );
            if let Some(level) = failed.thinking_level {
                entry.insert(
                    "thinkingLevel".to_string(),
                    serde_json::to_value(level).unwrap_or(Null),
                );
            }
            entry.insert(
                "message".to_string(),
                serde_json::to_value(AgentMessage::Assistant(failed.clone())).unwrap_or(Null),
            );
            Value::Object(entry)
        })
    });
    let messages: Vec<Value> = request
        .messages
        .iter()
        .filter_map(AgentMessage::to_llm_message)
        .map(|message| serde_json::to_value(&message).unwrap_or(Null))
        .collect();
    let mut request_json = Map::new();
    request_json.insert("reason".to_string(), json!(request.reason.as_str()));
    request_json.insert(
        "thinkingLevel".to_string(),
        json!(request.thinking_level.as_str()),
    );
    if let Some(state) = &request.state {
        request_json.insert("state".to_string(), state.clone());
    }
    request_json.insert(
        "model".to_string(),
        serde_json::to_value(request.model).unwrap_or(Null),
    );
    if let Some(previous) = previous {
        request_json.insert("previous".to_string(), previous);
    }
    if let Some(failed) = failed {
        request_json.insert("failed".to_string(), failed);
    }
    request_json.insert("messages".to_string(), Value::Array(messages));

    let reply = runner
        .lock()
        .expect("extension runner mutex poisoned")
        .route_model(
            &request.model.provider,
            &request.model.id,
            &Value::Object(request_json),
        )
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("{name} routing did not settle."))?;

    let route_model = reply.get("model").cloned().unwrap_or(Null);
    let route_provider = route_model
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let route_id = route_model
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let routed = format!("{name} routed to {route_provider}/{route_id}");
    let target = (resolvers.get_model)(&route_provider, &route_id)
        .ok_or_else(|| format!("{routed}, which is not a physical model."))?;
    if !(resolvers.has_credentials)(&target.provider) {
        return Err(format!("{routed}, which has no credentials."));
    }
    let thinking_level = match reply
        .get("thinkingLevel")
        .and_then(Value::as_str)
        .and_then(ThinkingLevel::parse)
    {
        Some(level) => clamp_thinking_level(&target, level),
        // A missing level rides `clampThinkingLevel`'s `indexOf === -1` arm:
        // the first supported level.
        None => pi_ai::get_supported_thinking_levels(&target)
            .first()
            .and_then(|name| ThinkingLevel::parse(name))
            .unwrap_or(ThinkingLevel::Off),
    };
    Ok(ModelRoute {
        model: target,
        thinking_level,
        state: reply.get("state").cloned().filter(|state| !state.is_null()),
    })
}

/// State shared between the routing hook and the routed stream function:
/// a routing failure surfaces as this request's stream error, which ends the
/// run with an error response (upstream: "A routing failure rejects, which
/// ends the run with an error response", `agent-session.ts:879-881`).
#[derive(Default)]
pub struct RouteErrorSlot(Mutex<Option<String>>);

impl RouteErrorSlot {
    /// Record a routing failure for the request about to stream.
    fn set(&self, message: String) {
        *self.0.lock().expect("route error slot") = Some(message);
    }

    /// Take the pending failure, if any.
    pub fn take(&self) -> Option<String> {
        self.0.lock().expect("route error slot").take()
    }
}

/// The stream function installed under a virtual selection
/// (`withVirtualModels`' `unroutedStream` plus the routed dispatch): each
/// request streams through its own model's provider adapter with that
/// provider's credentials, and an unrouted virtual model fails with the TS
/// error message.
pub struct RoutedStreamFn {
    /// The provider the `--api-key` flag belongs to (the selection at
    /// startup); the flag never applies to other providers.
    pub selected_provider: String,
    pub flag_api_key: Option<String>,
    pub auth_path: PathBuf,
    pub route_error: Arc<RouteErrorSlot>,
}

impl RoutedStreamFn {
    /// Builds the terminal error event the loop finalizes into an error
    /// response.
    fn error_stream(
        request: &StreamRequest,
        message: String,
    ) -> futures::stream::BoxStream<'static, pi_agent::types::AssistantMessageEvent> {
        let error = pi_ai::types::AssistantMessage {
            api: request.model.api.clone(),
            provider: request.model.provider.clone(),
            model: request.model.id.clone(),
            usage: pi_ai::types::Usage::default(),
            stop_reason: StopReason::Error,
            error_message: Some(message),
            timestamp: pi_agent::agent_loop::now_millis(),
            ..pi_agent::empty_assistant_message()
        };
        Box::pin(futures::stream::once(async move {
            pi_agent::types::AssistantMessageEvent::Error {
                reason: StopReason::Error,
                error,
            }
        }))
    }
}

impl StreamFn for RoutedStreamFn {
    fn stream(
        &self,
        request: StreamRequest,
    ) -> futures::stream::BoxStream<'static, pi_agent::types::AssistantMessageEvent> {
        if let Some(message) = self.route_error.take() {
            return Self::error_stream(&request, message);
        }
        if is_virtual_model(&request.model) {
            return Self::error_stream(
                &request,
                format!(
                    "Virtual model {}/{} must be routed before streaming",
                    request.model.provider, request.model.id
                ),
            );
        }
        let Some(inner) = pi_ai::providers::stream_fn_for(&request.model.api) else {
            return Self::error_stream(
                &request,
                format!(
                    "No provider adapter for api \"{}\" (model {}/{}).",
                    request.model.api, request.model.provider, request.model.id
                ),
            );
        };
        // The request's resolved credential belongs to the selected provider;
        // re-resolve through the ladder for the routed one. Quietly: the
        // environment notice prints once at startup.
        let flag = if request.model.provider == self.selected_provider {
            self.flag_api_key.as_deref()
        } else {
            None
        };
        let (api_key, access_token) = match crate::modes::print_mode::resolve_wire_credential(
            &request.model.provider,
            flag,
            &self.auth_path,
        ) {
            Some((crate::modes::print_mode::WireCredential::ApiKey(key), _)) => (Some(key), None),
            Some((crate::modes::print_mode::WireCredential::AccessToken(token), _)) => {
                (None, Some(token))
            }
            None => (None, None),
        };
        let request = StreamRequest {
            api_key,
            access_token,
            ..request
        };
        // The resolved adapter rides `ProviderStreamFn`'s adaptation
        // (LLM-shaped transcript, thinking-level mapping).
        crate::modes::print_mode::ProviderStreamFn(inner).stream(request)
    }
}

/// The `prepareRequest` routing wrapper (`_installAgentRequestProjection`,
/// `agent-session.ts:850-907`), installed per turn when the selected model
/// is virtual. Every request under the selection routes through the model's
/// router: the reason is `user` when a user message follows the last
/// assistant message and `continuation` otherwise, the router state rides
/// the session branch, and the routed model + clamped thinking level replace
/// the request's.
///
/// State handling deviates from the TS host in mechanism only: the branch
/// store is not shared with the loop thread, so the hook keeps an in-memory
/// overlay seeded from the branch at construction and hands the decided
/// entries to [`VirtualRouteHook::take_pending_state`] — the host appends
/// them (`appendCustomEntry`) when the turn ends. Within a turn the overlay
/// reads exactly as the branch would.
/// A catalog lookup crossing into the host (`getPhysicalModel`).
#[cfg(feature = "ext-js")]
type ModelLookup = Box<dyn Fn(&str, &str) -> Option<Model> + Send + Sync>;

/// A configured-credential check (`hasConfiguredAuth`).
#[cfg(feature = "ext-js")]
type CredentialCheck = Box<dyn Fn(&str) -> bool + Send + Sync>;

#[cfg(feature = "ext-js")]
pub struct VirtualRouteHook {
    runner: Arc<Mutex<pi_ext_js::ExtensionRunner>>,
    /// Catalog lookup for `previous`/`failed` and the routed target.
    get_model: ModelLookup,
    /// `hasConfiguredAuth`: configured credentials (auth.json or provider
    /// environment), not the `--api-key` flag.
    has_credentials: CredentialCheck,
    /// Router state per (provider, id), seeded from the branch.
    state: Mutex<HashMap<(String, String), Value>>,
    /// `pi.virtual-model-state` payloads decided this turn, awaiting the
    /// host's appends.
    pending_state: Mutex<Vec<Value>>,
    /// Where a routing failure lands for the stream to surface.
    route_error: Arc<RouteErrorSlot>,
}

#[cfg(feature = "ext-js")]
impl VirtualRouteHook {
    /// Seeds the hook: the router state from the branch for the selected
    /// model, the catalog lookup, and the configured-auth check. The
    /// `--api-key` flag is deliberately not part of the check: configured
    /// credentials (auth.json or provider environment) are, the flag is not.
    pub fn new(
        runner: Arc<Mutex<pi_ext_js::ExtensionRunner>>,
        model: &Model,
        auth_path: PathBuf,
        branch: &[SessionEntry],
    ) -> Self {
        let mut state = HashMap::new();
        if let Some(value) = get_virtual_model_state(branch, &model.provider, &model.id) {
            state.insert((model.provider.clone(), model.id.clone()), value);
        }
        Self {
            get_model: Box::new(crate::models_state::lookup),
            has_credentials: Box::new(move |provider| {
                crate::modes::print_mode::resolve_wire_credential(provider, None, &auth_path)
                    .is_some()
            }),
            runner,
            state: Mutex::new(state),
            pending_state: Mutex::new(Vec::new()),
            route_error: Arc::new(RouteErrorSlot::default()),
        }
    }

    /// The stream error slot to hand the routed stream function.
    pub fn route_error(&self) -> Arc<RouteErrorSlot> {
        Arc::clone(&self.route_error)
    }

    /// The state entries decided this turn; the host appends them
    /// (`appendCustomEntry(VIRTUAL_MODEL_STATE_ENTRY, data)`) when the turn
    /// ends.
    pub fn take_pending_state(&self) -> Vec<Value> {
        std::mem::take(&mut *self.pending_state.lock().expect("pending state"))
    }
}

/// Per-turn routing wiring: the prepare hook, the routed stream function,
/// and the state persistence, installed when the selected model is virtual.
/// Upstream installs the wrapper unconditionally; the port installs per
/// turn, where the model is fixed for the turn's config — the observable
/// behavior is identical.
#[cfg(feature = "ext-js")]
pub struct TurnRouting {
    pub hook: Arc<VirtualRouteHook>,
    stream_fn: Arc<dyn StreamFn>,
}

#[cfg(feature = "ext-js")]
impl TurnRouting {
    /// Install for the turn when `model` is virtual and a runner exists;
    /// `None` leaves the host's own stream function and no prepare hook.
    pub fn install(
        model: &Model,
        runner: Option<&Arc<Mutex<pi_ext_js::ExtensionRunner>>>,
        flag_api_key: Option<String>,
        session: &crate::session::SessionManager,
    ) -> Option<Self> {
        let runner = runner?;
        if !is_virtual_model(model) {
            return None;
        }
        let branch = branch_entries(session);
        let auth_path = crate::config::get_auth_path();
        let hook = Arc::new(VirtualRouteHook::new(
            Arc::clone(runner),
            model,
            auth_path.clone(),
            &branch,
        ));
        let stream_fn: Arc<dyn StreamFn> = Arc::new(RoutedStreamFn {
            selected_provider: model.provider.clone(),
            flag_api_key,
            auth_path,
            route_error: hook.route_error(),
        });
        Some(Self { hook, stream_fn })
    }

    /// The stream function: routed dispatch under a virtual selection.
    pub fn stream_fn(&self) -> Arc<dyn StreamFn> {
        Arc::clone(&self.stream_fn)
    }

    /// Append the decided `pi.virtual-model-state` entries to the branch
    /// (`appendCustomEntry`); entries of a cancelled turn still land, like
    /// upstream's mid-turn appends.
    pub fn persist_state(&self, session: &mut crate::session::SessionManager) {
        for data in self.hook.take_pending_state() {
            let _ = session.append_custom_entry(VIRTUAL_MODEL_STATE_ENTRY, Some(data));
        }
    }
}

#[cfg(feature = "ext-js")]
impl PrepareRequest for VirtualRouteHook {
    fn call<'a>(
        &'a self,
        request: PrepareRequestContext<'a>,
    ) -> pi_agent::types::BoxFuture<'a, TurnUpdate> {
        Box::pin(async move {
            self.route_error.take();
            let model = &request.model;
            if !is_virtual_model(model) {
                // Not a virtual selection: the wrapper is a no-op, like the
                // TS `if (!isVirtualModel(model)) return …` arm.
                return TurnUpdate::default();
            }
            // Only messages the user wrote start a turn; extension messages
            // can follow them (`agent-session.ts:881-883`).
            let last_assistant = request
                .context
                .messages
                .iter()
                .rposition(|message| matches!(message, AgentMessage::Assistant(_)));
            let user_turn = last_assistant.is_some_and(|index| {
                request.context.messages[index + 1..]
                    .iter()
                    .any(|message| matches!(message, AgentMessage::User(_)))
            });
            let key = (model.provider.clone(), model.id.clone());
            let state = self.state.lock().expect("router state").get(&key).cloned();
            let route = resolve_virtual_model(
                &self.runner,
                &RouteRequest {
                    model,
                    messages: &request.context.messages,
                    reason: if user_turn {
                        ModelRouteReason::User
                    } else {
                        ModelRouteReason::Continuation
                    },
                    thinking_level: request.thinking_level,
                    state: state.clone(),
                    failed: None,
                },
                RouteResolvers {
                    get_model: &self.get_model,
                    has_credentials: &self.has_credentials,
                },
            );
            match route {
                Ok(route) => {
                    if let Some(new_state) = &route.state {
                        if Some(new_state) != state.as_ref() {
                            self.state
                                .lock()
                                .expect("router state")
                                .insert(key, new_state.clone());
                            self.pending_state
                                .lock()
                                .expect("pending state")
                                .push(json!({
                                    "provider": model.provider,
                                    "modelId": model.id,
                                    "state": new_state,
                                }));
                        }
                    }
                    TurnUpdate {
                        model: Some(route.model),
                        thinking_level: Some(route.thinking_level),
                        context: None,
                    }
                }
                Err(message) => {
                    // The route stands failed: the stream function turns the
                    // stored message into the request's error response.
                    self.route_error.set(message);
                    TurnUpdate::default()
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::EntryBase;
    use serde_json::json;

    fn physical(provider: &str, id: &str, levels: &[&str]) -> Model {
        let mut map = Map::new();
        for level in ThinkingLevel::EXTENDED_ORDER {
            map.insert(
                level.as_str().to_string(),
                if levels.contains(&level.as_str()) {
                    json!(level.as_str())
                } else {
                    Null
                },
            );
        }
        Model {
            id: id.to_string(),
            name: id.to_string(),
            api: "test-api".to_string(),
            provider: provider.to_string(),
            reasoning: levels.iter().any(|level| *level != "off"),
            thinking_level_map: Some(map),
            ..Model::default()
        }
    }

    fn assistant(provider: &str, model: &str, api: &str, stop_reason: StopReason) -> AgentMessage {
        AgentMessage::Assistant(pi_ai::types::AssistantMessage {
            api: api.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            stop_reason,
            ..pi_agent::empty_assistant_message()
        })
    }

    fn custom_entry(custom_type: &str, data: Value) -> SessionEntry {
        SessionEntry::Custom(CustomEntry {
            custom_type: custom_type.to_string(),
            data: Some(data),
            base: EntryBase::default(),
            unknown: Map::new(),
        })
    }

    fn state_entry(provider: &str, model_id: &str, state: &Value) -> SessionEntry {
        custom_entry(
            VIRTUAL_MODEL_STATE_ENTRY,
            json!({ "provider": provider, "modelId": model_id, "state": state }),
        )
    }

    fn model_change(provider: &str, model_id: &str) -> SessionEntry {
        SessionEntry::ModelChange(crate::session::ModelChangeEntry {
            base: EntryBase::default(),
            provider: provider.to_string(),
            model_id: model_id.to_string(),
            unknown: Map::new(),
        })
    }

    fn message_entry(message: AgentMessage) -> SessionEntry {
        SessionEntry::Message(crate::session::SessionMessageEntry {
            base: EntryBase::default(),
            message,
            unknown: Map::new(),
        })
    }

    #[test]
    fn create_virtual_model_applies_definition_defaults() {
        let model = create_virtual_model(&json!({ "provider": "acme", "id": "router" }));
        assert_eq!(model.provider, "acme");
        assert_eq!(model.id, "router");
        assert_eq!(model.api, VIRTUAL_MODEL_API);
        assert_eq!(model.name, "");
        assert!(!model.reasoning);
        assert_eq!(
            model.input,
            vec![ModelInputModality::Text, ModelInputModality::Image]
        );
        assert_eq!(model.context_window, 0);
        assert_eq!(model.max_tokens, 0);
        let map = model.thinking_level_map.as_ref().expect("level map");
        // Levels default to ["off"]: "off" maps to itself, the rest to null.
        assert_eq!(map.get("off"), Some(&json!("off")));
        for level in ThinkingLevel::EXTENDED_ORDER {
            if level.as_str() == "off" {
                continue;
            }
            assert_eq!(map.get(level.as_str()), Some(&Null), "{}", level.as_str());
        }
        assert!(is_virtual_model(&model));
    }

    #[test]
    fn create_virtual_model_reads_the_definition() {
        let model = create_virtual_model(&json!({
            "provider": "acme",
            "id": "router",
            "name": "Acme Router",
            "thinkingLevels": ["off", "high"],
            "input": ["text"],
            "contextWindow": 100_000,
            "maxTokens": 8_192,
        }));
        assert_eq!(model.name, "Acme Router");
        assert!(model.reasoning);
        assert_eq!(model.input, vec![ModelInputModality::Text]);
        assert_eq!(model.context_window, 100_000);
        assert_eq!(model.max_tokens, 8_192);
        let map = model.thinking_level_map.expect("level map");
        assert_eq!(map.get("off"), Some(&json!("off")));
        assert_eq!(map.get("high"), Some(&json!("high")));
        assert_eq!(map.get("max"), Some(&Null));
    }

    #[test]
    fn find_latest_response_skips_errors_abort_and_non_assistant() {
        let messages = vec![
            assistant("p", "m", "api", StopReason::Stop),
            assistant("p", "m", "api", StopReason::Error),
            assistant("p", "m", "api", StopReason::Aborted),
            pi_agent::transcript::user_message("hi", 0),
            assistant("p", "m", "api", StopReason::ToolUse),
        ];
        let latest = find_latest_response(&messages).expect("a successful response");
        assert_eq!(latest.model, "m");
        assert!(matches!(latest.stop_reason, StopReason::ToolUse));
        assert!(find_latest_response(&[]).is_none());
    }

    #[test]
    fn get_virtual_model_state_takes_the_last_matching_entry() {
        let branch = vec![
            state_entry("acme", "router", &json!({ "n": 1 })),
            custom_entry(
                "pi.other",
                json!({ "provider": "acme", "modelId": "router" }),
            ),
            state_entry("acme", "other", &json!({ "n": 2 })),
            state_entry("acme", "router", &json!({ "n": 3 })),
        ];
        assert_eq!(
            get_virtual_model_state(&branch, "acme", "router"),
            Some(json!({ "n": 3 }))
        );
        assert_eq!(get_virtual_model_state(&branch, "acme", "ghost"), None);
        assert_eq!(get_virtual_model_state(&[], "acme", "router"), None);
    }

    #[test]
    fn get_branch_selection_prefers_a_trailing_model_change() {
        let branch = vec![
            message_entry(assistant("p", "m", "api", StopReason::Stop)),
            model_change("acme", "router"),
        ];
        assert_eq!(
            get_branch_selection(&branch, &|_, _| None),
            Some(("acme".to_string(), "router".to_string()))
        );
    }

    #[test]
    fn get_branch_selection_lets_a_registered_virtual_change_hold() {
        let virtual_model = create_virtual_model(&json!({ "provider": "acme", "id": "router" }));
        let branch = vec![
            model_change("acme", "router"),
            message_entry(assistant("p", "m", "api", StopReason::Stop)),
        ];
        let lookup =
            |provider: &str, _id: &str| (provider == "acme").then(|| virtual_model.clone());
        assert_eq!(
            get_branch_selection(&branch, &lookup),
            Some(("acme".to_string(), "router".to_string()))
        );
    }

    #[test]
    fn get_branch_selection_falls_back_to_the_last_physical_response() {
        // An unregistered or non-virtual model change never holds: the
        // response's own model wins (upstream `change && model &&
        // isVirtualModel(model) ? change : response`).
        for registered in [None, Some(physical("other", "physical", &["off", "high"]))] {
            let branch = vec![
                model_change("acme", "router"),
                message_entry(assistant("p", "m", "api", StopReason::Stop)),
            ];
            let lookup = |provider: &str, _id: &str| {
                (provider == "acme").then(|| registered.clone()).flatten()
            };
            assert_eq!(
                get_branch_selection(&branch, &lookup),
                Some(("p".to_string(), "m".to_string()))
            );
        }
    }

    #[test]
    fn get_branch_selection_skips_virtual_and_failed_responses() {
        let branch = vec![
            message_entry(assistant(
                "acme",
                "router",
                VIRTUAL_MODEL_API,
                StopReason::Error,
            )),
            message_entry(assistant("p", "m", "api", StopReason::Stop)),
        ];
        assert_eq!(
            get_branch_selection(&branch, &|_, _| None),
            Some(("p".to_string(), "m".to_string()))
        );
        assert_eq!(get_branch_selection(&[], &|_, _| None), None);
    }

    #[test]
    fn with_virtual_model_listing_hides_same_ids_and_appends() {
        let physical_a = physical("p1", "x", &["off"]);
        let physical_b = physical("p1", "y", &["off"]);
        let virtual_x = create_virtual_model(&json!({ "provider": "p1", "id": "x" }));
        let virtual_z = create_virtual_model(&json!({ "provider": "p2", "id": "z" }));
        let listing = with_virtual_model_listing(
            vec![physical_a, physical_b.clone()],
            &[virtual_x, virtual_z],
        );
        assert_eq!(
            listing,
            vec![
                physical_b,
                create_virtual_model(&json!({ "provider": "p1", "id": "x" })),
                create_virtual_model(&json!({ "provider": "p2", "id": "z" })),
            ]
        );
    }

    const ROUTE_EXTENSION: &str = r#"
export default function (pi) {
  pi.registerVirtualModel({
    provider: "acme",
    id: "router",
    name: "Acme Router",
    route: () => ({
      model: { provider: "acme", id: "slow" },
      thinkingLevel: "xhigh",
      state: { n: 1 },
    }),
  });
}
"#;

    #[cfg(feature = "ext-js")]
    fn route_runner() -> pi_ext_js::ExtensionRunner {
        let mut runner = pi_ext_js::ExtensionRunner::new(pi_ext_js::HostCore {
            cwd: "/tmp".to_string(),
            mode: "print".to_string(),
            settings: json!({}),
            ..Default::default()
        })
        .expect("runner");
        runner
            .load_extension("<test>", Some(ROUTE_EXTENSION))
            .expect("load");
        runner.bind().expect("bind");
        runner
    }

    #[cfg(feature = "ext-js")]
    fn route_request(model: &Model, state: Option<Value>) -> RouteRequest<'_> {
        RouteRequest {
            model,
            messages: &[],
            reason: ModelRouteReason::User,
            thinking_level: ThinkingLevel::High,
            state,
            failed: None,
        }
    }

    #[cfg(feature = "ext-js")]
    #[test]
    fn resolve_virtual_model_routes_clamps_and_round_trips_state() {
        let runner = std::sync::Mutex::new(route_runner());
        let model = create_virtual_model(&json!({ "provider": "acme", "id": "router" }));
        let resolvers = RouteResolvers {
            get_model: &|provider, id| {
                (provider == "acme" && id == "slow")
                    .then(|| physical("acme", "slow", &["off", "high"]))
            },
            has_credentials: &|provider| provider == "acme",
        };
        let route = resolve_virtual_model(
            &runner,
            &route_request(&model, Some(json!({ "n": 0 }))),
            resolvers,
        )
        .expect("route");
        // "xhigh" clamps down to the target's highest supported level.
        assert_eq!(route.thinking_level, ThinkingLevel::High);
        assert_eq!(route.model.id, "slow");
        assert_eq!(route.state, Some(json!({ "n": 1 })));
    }

    #[cfg(feature = "ext-js")]
    #[test]
    fn resolve_virtual_model_carries_the_ts_error_messages() {
        let runner = std::sync::Mutex::new(route_runner());
        let model = create_virtual_model(&json!({ "provider": "acme", "id": "router" }));
        let unregistered = create_virtual_model(&json!({ "provider": "acme", "id": "missing" }));
        let no_model = RouteResolvers {
            get_model: &|_, _| None,
            has_credentials: &|_| true,
        };
        let no_credentials = RouteResolvers {
            get_model: &|_, _| Some(physical("acme", "slow", &["off"])),
            has_credentials: &|_| false,
        };
        assert_eq!(
            resolve_virtual_model(&runner, &route_request(&unregistered, None), no_model)
                .expect_err("unregistered"),
            "Virtual model acme/missing is not registered."
        );
        assert_eq!(
            resolve_virtual_model(&runner, &route_request(&model, None), no_model)
                .expect_err("not a physical model"),
            "Virtual model acme/router routed to acme/slow, which is not a physical model."
        );
        assert_eq!(
            resolve_virtual_model(&runner, &route_request(&model, None), no_credentials)
                .expect_err("no credentials"),
            "Virtual model acme/router routed to acme/slow, which has no credentials."
        );
    }
}
