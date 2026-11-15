//! Extensions host wiring (E2 slice 3).
//!
//! Ports the CLI loading path `main.ts` assembles around the runtime:
//! discovery with `--no-extensions` semantics (`resource-loader.ts:609-615`,
//! CLI `-e` paths still load), the pre-trust bootstrap pass
//! (`resource-loader.ts:533-553`) in which global/CLI extensions may handle
//! `project_trust` (`project-trust.ts:55-71`), the final load that reuses
//! bootstrap-loaded modules and never retries bootstrap failures
//! (`resource-loader.ts:791-833`), bind, extension flag claiming
//! (`agent-session-services.ts:82-129`), and the extension-tool half of the
//! tool loadout (`agent-session.ts:1600-1716`) reduced to the print-path
//! subset. Load failures surface as the upstream diagnostics
//! (`main.ts:861-869`); error-level diagnostics abort startup with the
//! load-failure hint (`main.ts:986-990`).
//!
//! Deliberately slice-3 scoped (§8 of `docs/EXTENSIONS.md`): commands
//! dispatch with the consuming hosts, session/agent event hooks and the
//! `session_start` emission land in slice 4, and the full active-loadout
//! machinery (`prepareLoadout`, pending tools) is E3.
//!
//! The `ext-js` feature gates the engine. Without it the host keeps
//! upstream's shape with zero extensions: nothing loads, trust resolves
//! without the hook, and every captured unknown flag stays unclaimed.

use std::path::Path;
use std::sync::Arc;

#[cfg(feature = "ext-js")]
use std::collections::HashSet;
#[cfg(feature = "ext-js")]
use std::path::PathBuf;
#[cfg(feature = "ext-js")]
use std::sync::Mutex;

use pi_agent::types::ToolExecutor;
#[cfg(feature = "ext-js")]
use pi_agent::types::{AgentToolCall, AgentToolResult, ToolSpec};
#[cfg(feature = "ext-js")]
use serde_json::{Value, json};

use crate::cli::args::{Args, Diagnostic, DiagnosticKind};
use crate::cli::project_trust::{
    DefaultProjectTrust, ProjectTrustStore, ResolveProjectTrustedOptions,
    has_trust_requiring_project_resources, resolve_project_trusted,
};
use crate::config::settings::SettingsManager;
#[cfg(feature = "ext-js")]
use crate::tools::index::BuiltinToolExecutor;
use crate::tools::index::{ToolDefinition, ToolRegistry};
#[cfg(feature = "ext-js")]
use crate::tools::renderers::ToolRenderers;
#[cfg(feature = "ext-js")]
use crate::tools::schema::validate_tool_arguments;

/// `EXTENSION_LOAD_FAILURE_HINT` (`main.ts:77`), printed after the
/// diagnostics when an extension failed to load (`main.ts:986-990`).
pub const EXTENSION_LOAD_FAILURE_HINT: &str = "Hint: Start without extensions using \"pi -ne\".";

/// Extension tools carry no `renderCall`/`renderResult` upstream; the
/// renderers are therefore undefined and the interactive component's
/// no-renderer fallback (plain text, `tool_execution.rs`) shapes them. The
/// definition field is not optional, so the placeholder pair here must not
/// be reached in E2 modes (only the interactive component renders, and it
/// resolves renderers through [`crate::tools::renderers::renderers_for`]).
#[cfg(feature = "ext-js")]
const EXTENSION_RENDERERS: ToolRenderers = ToolRenderers {
    render_call: |_args, _context| String::new(),
    render_result: |_args, _result, _context| String::new(),
};

pub fn error(message: String) -> Diagnostic {
    Diagnostic {
        kind: DiagnosticKind::Error,
        message,
    }
}

pub fn warning(message: String) -> Diagnostic {
    Diagnostic {
        kind: DiagnosticKind::Warning,
        message,
    }
}

/// The loaded extension host: the runner behind a mutex plus the extension
/// tool candidates for tool composition. Shared with the agent loop —
/// `ExtensionRunner` is `Send` (rquickjs's `parallel` feature; `Send`
/// error listeners and host actions), and the print/json/rpc modes run on
/// one thread, so the mutex never contends.
pub struct ExtensionHost {
    #[cfg(feature = "ext-js")]
    runner: Option<Arc<Mutex<pi_ext_js::ExtensionRunner>>>,
    /// Registered extension tools eligible for declaration — default
    /// active, non-hidden, exposure `direct`/`model-only` (the active rule
    /// the runner bind applies) — first-wins in load order.
    extension_tools: Vec<ToolDefinition>,
}

impl ExtensionHost {
    /// The host compiled without the JS engine: no runner, no tools.
    #[cfg(not(feature = "ext-js"))]
    fn empty() -> Self {
        Self {
            #[cfg(feature = "ext-js")]
            runner: None,
            extension_tools: Vec::new(),
        }
    }

    /// Extension tool candidates for [`select_tools`] — unfiltered by the
    /// CLI tool flags; the selection applies the allowlist/exclude rules.
    #[must_use]
    pub fn tool_definitions(&self) -> &[ToolDefinition] {
        &self.extension_tools
    }

    /// The executor for the selected tool list: extension tool calls
    /// dispatch into the runner, every other name to the builtin executor.
    #[must_use]
    pub fn executor(&self, selected: &[ToolDefinition], cwd: &str) -> Arc<dyn ToolExecutor> {
        #[cfg(feature = "ext-js")]
        {
            let extension_selected: Vec<ToolDefinition> = {
                let names: Vec<&str> = self.extension_tools.iter().map(|d| d.name).collect();
                selected
                    .iter()
                    .filter(|definition| names.contains(&definition.name))
                    .cloned()
                    .collect()
            };
            Arc::new(ExtensionAwareExecutor {
                builtin: builtin_executor_over(selected, &self.extension_tools, cwd),
                runner: self.runner.clone(),
                extension: extension_selected,
            })
        }
        #[cfg(not(feature = "ext-js"))]
        ToolRegistry::new(selected.to_vec()).executor(cwd)
    }
}

/// The builtin executor over `selected` minus the extension names, so the
/// builtin registry never double-reports an extension tool.
#[cfg(feature = "ext-js")]
fn builtin_executor_over(
    selected: &[ToolDefinition],
    extension: &[ToolDefinition],
    cwd: &str,
) -> Arc<BuiltinToolExecutor> {
    let names: Vec<&str> = extension.iter().map(|definition| definition.name).collect();
    let builtin_defs: Vec<ToolDefinition> = selected
        .iter()
        .filter(|definition| !names.contains(&definition.name))
        .cloned()
        .collect();
    ToolRegistry::new(builtin_defs).executor(cwd)
}

/// The extension `ToolResult` → `AgentToolResult` (upstream `ToolResult`:
/// `content` as string-or-blocks, `details`, `structuredContent`, `usage`,
/// `isError`).
#[cfg(feature = "ext-js")]
fn tool_result_from_extension(value: &Value) -> AgentToolResult {
    let content = match value.get("content") {
        Some(Value::String(text)) => vec![pi_ai::types::ContentBlock::Text(
            pi_ai::types::TextContent {
                text: text.clone(),
                text_signature: None,
            },
        )],
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| serde_json::from_value(block.clone()).ok())
            .collect(),
        _ => Vec::new(),
    };
    AgentToolResult {
        content,
        details: value.get("details").cloned().unwrap_or_else(|| json!({})),
        structured_content: value
            .get("structuredContent")
            .cloned()
            .filter(|value| !value.is_null()),
        usage: value
            .get("usage")
            .and_then(|usage| serde_json::from_value(usage.clone()).ok()),
        is_error: value
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        terminate: false,
    }
}

/// The composite [`ToolExecutor`]: extension tool calls validate against
/// their schema and run through the runner (`ExtensionRunner::execute_tool`,
/// the `toolCall` dispatch session), everything else delegates to the
/// builtin executor.
#[cfg(feature = "ext-js")]
struct ExtensionAwareExecutor {
    builtin: Arc<BuiltinToolExecutor>,
    runner: Option<Arc<Mutex<pi_ext_js::ExtensionRunner>>>,
    /// The selected extension tools (the dispatch set).
    extension: Vec<ToolDefinition>,
}

#[cfg(feature = "ext-js")]
impl ToolExecutor for ExtensionAwareExecutor {
    fn tools(&self) -> Vec<ToolSpec> {
        let mut specs = self.builtin.tools();
        specs.extend(self.extension.iter().map(|definition| ToolSpec {
            name: definition.name.to_string(),
            description: definition.description.clone(),
            parameters: definition.parameters.clone(),
            execution_mode: definition.execution_mode,
            constrained_sampling: definition.constrained_sampling.clone(),
        }));
        specs
    }

    fn execution_mode(&self, name: &str) -> Option<pi_agent::types::ToolExecutionMode> {
        self.builtin.execution_mode(name).or_else(|| {
            self.extension
                .iter()
                .find(|definition| definition.name == name)
                .and_then(|definition| definition.execution_mode)
        })
    }

    fn execute(
        &self,
        call: AgentToolCall,
        updates: tokio::sync::mpsc::UnboundedSender<AgentToolResult>,
    ) -> pi_agent::types::BoxFuture<'_, Result<AgentToolResult, String>> {
        if self
            .extension
            .iter()
            .any(|definition| definition.name == call.name)
        {
            let name = call.name.clone();
            let call_id = call.id.clone();
            let arguments = Value::Object(call.arguments);
            Box::pin(async move { self.execute_extension(&name, &call_id, &arguments) })
        } else {
            self.builtin.execute(call, updates)
        }
    }
}

#[cfg(feature = "ext-js")]
impl ExtensionAwareExecutor {
    /// One extension tool call: argument validation like the builtins
    /// (`BuiltinToolExecutor::run`), then the runner dispatch. The runner
    /// lock is held only across the synchronous JS execution.
    fn execute_extension(
        &self,
        name: &str,
        call_id: &str,
        arguments: &Value,
    ) -> Result<AgentToolResult, String> {
        let definition = self
            .extension
            .iter()
            .find(|definition| definition.name == name)
            .expect("extension dispatch set holds the tool");
        let arguments = validate_tool_arguments(
            name,
            &definition.parameters,
            arguments,
            definition.prepare_arguments,
        )?;
        let runner = self
            .runner
            .as_ref()
            .expect("extension tools exist only with a runner");
        let mut runner = runner.lock().expect("extension runner mutex poisoned");
        let value = runner
            .execute_tool(name, call_id, &arguments)
            .map_err(|error| error.to_string())?;
        Ok(tool_result_from_extension(&value))
    }
}

/// The load outcome: the host plus the resolved trust decision.
pub struct Loaded {
    pub host: ExtensionHost,
    pub project_trusted: bool,
}

/// Discovery + load + bind + flag claiming
/// (`createAgentSessionServices` + `resource-loader.reload`). Returns the
/// diagnostics in report order; the caller prints warnings and aborts on
/// error-level entries.
///
/// # Errors
/// Runtime creation, bind, and trust-store failures propagate; load
/// failures come back as error diagnostics instead.
pub fn load(
    args: &Args,
    cwd: &Path,
    agent_dir: &Path,
    trust_store: &ProjectTrustStore,
    default_project_trust: Option<DefaultProjectTrust>,
    startup_settings: &SettingsManager,
    mode: &str,
) -> Result<(Loaded, Vec<Diagnostic>), String> {
    #[cfg(feature = "ext-js")]
    {
        load_with_engine(
            args,
            cwd,
            agent_dir,
            trust_store,
            default_project_trust,
            startup_settings,
            mode,
        )
    }
    #[cfg(not(feature = "ext-js"))]
    {
        let _ = (agent_dir, startup_settings, mode);
        let mut diagnostics = Vec::new();
        let project_trusted = resolve_project_trusted(&mut ResolveProjectTrustedOptions {
            cwd,
            store: trust_store,
            trust_override: args.project_trust_override,
            default_project_trust,
            extension_trust: None,
        })?;
        // Without the engine nothing claims flags: every captured unknown
        // flag stays unclaimed (`agent-session-services.ts:123-128`).
        let unclaimed: Vec<&str> = args
            .unknown_flags
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        if let Some(diagnostic) = unknown_options_diagnostic(&unclaimed) {
            diagnostics.push(diagnostic);
        }
        Ok((
            Loaded {
                host: ExtensionHost::empty(),
                project_trusted,
            },
            diagnostics,
        ))
    }
}

/// The real path with the engine.
#[cfg(feature = "ext-js")]
#[allow(clippy::too_many_lines)] // the upstream reload() sequence, kept in one pass
fn load_with_engine(
    args: &Args,
    cwd: &Path,
    agent_dir: &Path,
    trust_store: &ProjectTrustStore,
    default_project_trust: Option<DefaultProjectTrust>,
    startup_settings: &SettingsManager,
    mode: &str,
) -> Result<(Loaded, Vec<Diagnostic>), String> {
    use pi_ext_js::loader::{discover_cli_paths, discover_extensions, load_paths, split_for_trust};

    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    let configured: Vec<String> = args.extensions.clone().unwrap_or_default();

    // `--no-extensions` still loads CLI extensions
    // (`resource-loader.ts:609-610`).
    let discovered = if args.no_extensions {
        discover_cli_paths(&configured, cwd)
    } else {
        discover_extensions(&configured, cwd, agent_dir)
    };

    // Local CLI paths that do not exist error without attempting a load
    // (`resource-loader.ts:620-627`).
    let mut missing: HashSet<PathBuf> = HashSet::new();
    for configured in &configured {
        if !is_local_path(configured) {
            continue;
        }
        let resolved = if Path::new(configured).is_absolute() {
            PathBuf::from(configured)
        } else {
            cwd.join(configured)
        };
        if !resolved.exists() {
            missing.insert(resolved.clone());
            diagnostics.push(error(format!(
                "Failed to load extension \"{}\": Extension path does not exist: {}",
                resolved.display(),
                resolved.display()
            )));
        }
    }

    // The bootstrap runner sees the untrusted settings, exactly like the
    // forced-untrusted pass (`resource-loader.ts:536-538`).
    let mut runner = pi_ext_js::ExtensionRunner::new(pi_ext_js::HostCore {
        cwd: cwd.to_string_lossy().into_owned(),
        mode: mode.to_string(),
        settings: Value::Object(startup_settings.get_settings()),
        ..Default::default()
    })
    .map_err(|err| format!("Failed to initialize the extension runtime: {err}"))?;

    // The pre-trust pass runs only when trust actually needs resolving
    // (`main.ts:804-808`): no `--approve` override and trust-requiring
    // project resources.
    let should_resolve =
        args.project_trust_override.is_none() && has_trust_requiring_project_resources(cwd);
    let bootstrap: Vec<PathBuf> = split_for_trust(&discovered, false)
        .0
        .into_iter()
        .filter(|path| !missing.contains(path))
        .collect();
    let mut bootstrap_errors: Vec<pi_ext_js::LoadError> = Vec::new();
    if should_resolve {
        let result = load_paths(&mut runner, &bootstrap);
        bootstrap_errors = result.errors;
        for load_error in &bootstrap_errors {
            diagnostics.push(error(format!(
                "Failed to load extension \"{}\": {}",
                load_error.path, load_error.error
            )));
        }
    }

    // The trust hook fires `project_trust` in the bootstrap-loaded
    // extensions (`project-trust.ts:55-71`); handler errors report as
    // warnings.
    let project_trusted = {
        let mut hook = |hook_cwd: &Path| -> Result<Option<(bool, bool)>, String> {
            let (result, errors) = runner
                .emit_project_trust(&hook_cwd.to_string_lossy())
                .map_err(|err| err.to_string())?;
            for ext_error in errors {
                diagnostics.push(warning(format!(
                    "Extension \"{}\" project_trust error: {}",
                    ext_error.extension_path, ext_error.error
                )));
            }
            Ok(result.map(|decision| (decision.trusted == "yes", decision.remember)))
        };
        resolve_project_trusted(&mut ResolveProjectTrustedOptions {
            cwd,
            store: trust_store,
            trust_override: args.project_trust_override,
            default_project_trust,
            extension_trust: Some(&mut hook),
        })?
    };

    // The settings view flips with the trust decision
    // (`setProjectTrusted` + `reload`).
    {
        let trusted_settings = SettingsManager::create(
            cwd,
            agent_dir,
            crate::config::settings::SettingsManagerCreateOptions {
                project_trusted: Some(project_trusted),
            },
        );
        runner.core().lock().expect("host core").settings =
            Value::Object(trusted_settings.get_settings());
    }

    // The final pass: everything trusted allows, minus already-loaded
    // modules and never-retried bootstrap failures
    // (`loadFinalExtensionSet`, `resource-loader.ts:791-833`).
    let loaded: HashSet<String> = runner.extension_paths().into_iter().collect();
    let failed: HashSet<String> = bootstrap_errors.iter().map(|e| e.path.clone()).collect();
    let remaining: Vec<PathBuf> = discovered
        .iter()
        .filter(|entry| project_trusted || entry.origin != pi_ext_js::ExtensionOrigin::Project)
        .map(|entry| entry.path.clone())
        .filter(|path| {
            let key = path.to_string_lossy().into_owned();
            !loaded.contains(&key) && !failed.contains(&key) && !missing.contains(path)
        })
        .collect();
    let result = load_paths(&mut runner, &remaining);
    for load_error in &result.errors {
        diagnostics.push(error(format!(
            "Failed to load extension \"{}\": {}",
            load_error.path, load_error.error
        )));
    }

    runner
        .bind()
        .map_err(|err| format!("Failed to bind the extension runtime: {err}"))?;
    // Bind-time registration failures report without failing the load
    // (the bindCore `emitError` reports, `runner.ts:470-521`).
    for bind_error in runner.provider_registration_errors() {
        diagnostics.push(warning(format!(
            "Extension \"{}\" {} error: {}",
            bind_error.extension_path, bind_error.event, bind_error.error
        )));
    }

    // Flag claiming (`applyExtensionFlagValues`,
    // `agent-session-services.ts:82-129`).
    let registered = runner.flags();
    let mut unclaimed: Vec<&str> = Vec::new();
    for (name, value) in &args.unknown_flags {
        match registered.iter().find(|flag| flag.name == *name) {
            None => unclaimed.push(name.as_str()),
            // Booleans set `true` regardless of the captured value.
            Some(flag) if flag.kind == "boolean" => runner.set_flag_value(name, json!(true)),
            Some(_) => match value {
                Some(value) => runner.set_flag_value(name, json!(value)),
                None => diagnostics.push(error(format!(
                    "Extension flag \"--{name}\" requires a value"
                ))),
            },
        }
    }
    if let Some(diagnostic) = unknown_options_diagnostic(&unclaimed) {
        diagnostics.push(diagnostic);
    }

    // Tool candidates: default-active, non-hidden, `direct`/`model-only`
    // exposure — the runner bind()'s active rule (`agent-session.ts`
    // `_applyToolLoadout` reduces to this for the print path).
    let extension_tools = runner
        .all_registered_tools()
        .into_iter()
        .filter(|tool| {
            tool.default_active && matches!(tool.exposure.as_str(), "direct" | "model-only")
        })
        .map(tool_definition)
        .collect::<Vec<_>>();

    Ok((
        Loaded {
            host: ExtensionHost {
                runner: Some(Arc::new(Mutex::new(runner))),
                extension_tools,
            },
            project_trusted,
        },
        diagnostics,
    ))
}

/// `isLocalPath` (`utils/paths.ts:94-109`): the known non-local prefixes
/// are package-manager sources (P5); everything else is a filesystem path.
fn is_local_path(value: &str) -> bool {
    let trimmed = value.trim();
    !trimmed.starts_with("npm:")
        && !trimmed.starts_with("git:")
        && !trimmed.starts_with("github:")
        && !trimmed.starts_with("http:")
        && !trimmed.starts_with("https:")
        && !trimmed.starts_with("ssh:")
        && !trimmed.starts_with("builtin:")
}

/// The unclaimed-flags diagnostic (`agent-session-services.ts:123-128`).
fn unknown_options_diagnostic(names: &[&str]) -> Option<Diagnostic> {
    if names.is_empty() {
        return None;
    }
    let list = names
        .iter()
        .map(|name| format!("--{name}"))
        .collect::<Vec<_>>()
        .join(", ");
    Some(error(format!(
        "Unknown option{}: {list}",
        if names.len() == 1 { "" } else { "s" }
    )))
}

/// The extension `ToolInfo` → `ToolDefinition`. Tool names leak to
/// `'static` on purpose: upstream keeps them for the process lifetime too,
/// and the count is bounded by the loaded extensions.
#[cfg(feature = "ext-js")]
fn tool_definition(info: &pi_ext_js::ToolInfo) -> ToolDefinition {
    let name: &'static str = Box::leak(info.name.clone().into_boxed_str());
    ToolDefinition {
        name,
        label: name,
        description: info.description.clone(),
        parameters: info
            .parameters
            .clone()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
        output_schema: None,
        prompt_snippet: None,
        prompt_guidelines: Vec::new(),
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        render_shell: None,
        renderers: EXTENSION_RENDERERS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- engine-free pieces ----

    #[test]
    fn unknown_options_diagnostic_singular_and_plural() {
        let one = unknown_options_diagnostic(&["flag-a"]).expect("one");
        assert_eq!(one.kind, DiagnosticKind::Error);
        assert_eq!(one.message, "Unknown option: --flag-a");
        let many = unknown_options_diagnostic(&["flag-a", "flag-b"]).expect("many");
        assert_eq!(many.message, "Unknown options: --flag-a, --flag-b");
        assert!(unknown_options_diagnostic(&[]).is_none());
    }

    #[test]
    fn local_path_gate_matches_upstream_prefixes() {
        assert!(is_local_path("/abs/path.ts"));
        assert!(is_local_path("./rel.ts"));
        assert!(!is_local_path("git:github.com/x/y"));
        assert!(!is_local_path("npm:pkg"));
        assert!(!is_local_path("builtin:mcp"));
        assert!(!is_local_path("https://example.com/x.ts"));
    }
}
