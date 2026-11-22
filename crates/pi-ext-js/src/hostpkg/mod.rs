//! Virtual modules for the remaining `@earendil-works/*` host packages (E4).
//!
//! Upstream hands every extension a fixed virtual-module table
//! (`core/extensions/virtual-modules.ts:14-42`). The port already serves the
//! state-backed `@earendil-works/pi-coding-agent` and `@earendil-works/pi-mcp`
//! packages from [`crate::node`]; this module fills the rest of that table so
//! bundled extensions can import the packages they reference at runtime:
//!
//! - `@earendil-works/pi-ai` (and `/compat`, `/models`, `/oauth`,
//!   `/providers/all`, `/utils/model-operations`, `/utils/oauth-page`) — the
//!   extension view of `pi-ai-extension-view.ts:1-28` plus the pure helpers
//!   from `packages/ai/src/{models.ts,utils/*}`. The composed catalog stays
//!   Rust-owned; `getModel()` reads the current model from `HostCore` through
//!   the existing `getModel` op. Streaming entry points are documented stubs.
//! - `@earendil-works/pi-tui` — faithful data-shape constructors for the
//!   component surface the bundled MCP extension imports
//!   (`extensions/mcp/ui.ts:6-18`, `:25`; `extensions/codemode/renderer.ts:10`).
//!   There is no terminal in `QuickJS`, so `render(width)` returns plain rows
//!   for a host-side painter rather than driving stdout.
//! - `@earendil-works/pi-agent-core` — type-only surfaces (erased upstream)
//!   and `AgentToolResult` / content shape helpers
//!   (`packages/agent/src/types.ts:424-440`); no live agent loop.
//! - `@earendil-works/pi-codemode/source` — `CODEMODE_SOURCE_GRAMMAR`, copied
//!   byte-for-byte from `crates/pi-codemode/src/source.rs:17-25`
//!   (upstream `packages/codemode/src/source.ts:18-30`).
//!
//! The `@mariozechner/*` aliases from `virtual-modules.ts:34-41` map to the
//! same sources. Nothing here edits the existing resolver or engine: the
//! coordinator registers [`host_package_modules`] in
//! `resolver::virtual_module_source` and [`HOST_PACKAGE_SPECIFIERS`] in the
//! availability check (see `WIRING.md`).

/// Every specifier this module serves, paired with its JavaScript source.
///
/// The coordinator can pair this table into
/// `resolver::virtual_module_source` (match or linear scan); [`source`] is the
/// ready-made `Option` adapter and [`HOST_PACKAGE_SPECIFIERS`] lists just the
/// names for the availability check.
pub fn host_package_modules() -> Vec<(&'static str, &'static str)> {
    HOST_PACKAGE_SPECIFIERS
        .iter()
        .map(|specifier| {
            (
                *specifier,
                source(specifier).expect("every listed specifier has a source"),
            )
        })
        .collect()
}

/// Specifiers served here, including the upstream `@mariozechner/*` aliases.
pub const HOST_PACKAGE_SPECIFIERS: &[&str] = &[
    "@earendil-works/pi-ai",
    "@earendil-works/pi-ai/compat",
    "@earendil-works/pi-ai/models",
    "@earendil-works/pi-ai/oauth",
    "@earendil-works/pi-ai/providers/all",
    "@earendil-works/pi-ai/utils/model-operations",
    "@earendil-works/pi-ai/utils/oauth-page",
    "@earendil-works/pi-tui",
    "@earendil-works/pi-agent-core",
    "@earendil-works/pi-codemode/source",
    "@mariozechner/pi-ai",
    "@mariozechner/pi-ai/compat",
    "@mariozechner/pi-ai/models",
    "@mariozechner/pi-ai/oauth",
    "@mariozechner/pi-ai/providers/all",
    "@mariozechner/pi-tui",
    "@mariozechner/pi-agent-core",
    "@mariozechner/pi-codemode/source",
];

/// The JavaScript source for one host-package specifier, if served here.
pub fn source(specifier: &str) -> Option<&'static str> {
    Some(match specifier {
        "@earendil-works/pi-ai"
        | "@earendil-works/pi-ai/compat"
        | "@earendil-works/pi-ai/models"
        | "@earendil-works/pi-ai/oauth"
        | "@earendil-works/pi-ai/providers/all"
        | "@earendil-works/pi-ai/utils/model-operations"
        | "@earendil-works/pi-ai/utils/oauth-page"
        | "@mariozechner/pi-ai"
        | "@mariozechner/pi-ai/compat"
        | "@mariozechner/pi-ai/models"
        | "@mariozechner/pi-ai/oauth"
        | "@mariozechner/pi-ai/providers/all" => include_str!("../node/host/pi-ai.js"),
        "@earendil-works/pi-tui" | "@mariozechner/pi-tui" => {
            include_str!("../node/host/pi-tui.js")
        }
        "@earendil-works/pi-agent-core" | "@mariozechner/pi-agent-core" => {
            include_str!("../node/host/pi-agent-core.js")
        }
        "@earendil-works/pi-codemode/source" | "@mariozechner/pi-codemode/source" => {
            include_str!("../node/host/pi-codemode-source.js")
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use rquickjs::{Context, Function, Module, Object, Runtime};
    use serde_json::{Value, json};

    /// Copies `crates/pi-codemode/src/source.rs:17-25` so the embedded JS
    /// grammar can be pinned without a cross-crate dependency.
    const GRAMMAR: &str = r"
start: options_source | plain_source
options_source: OPTIONS_LINE NEWLINE SOURCE
plain_source: SOURCE

OPTIONS_LINE: /[ \t]*\/\/ @options:[^\r\n]*/
NEWLINE: /\r?\n/
SOURCE: /[\s\S]+/
";

    struct TestResolver;

    impl rquickjs::loader::Resolver for TestResolver {
        fn resolve(
            &mut self,
            _ctx: &rquickjs::Ctx<'_>,
            _base: &str,
            name: &str,
            _attributes: Option<rquickjs::loader::ImportAttributes<'_>>,
        ) -> rquickjs::Result<String> {
            if source(name).is_some() || name == "typebox" {
                Ok(format!("virtual:{name}"))
            } else {
                Err(rquickjs::Error::Unknown)
            }
        }
    }

    struct TestLoader;

    impl rquickjs::loader::Loader for TestLoader {
        fn load<'js>(
            &mut self,
            ctx: &rquickjs::Ctx<'js>,
            name: &str,
            _attributes: Option<rquickjs::loader::ImportAttributes<'js>>,
        ) -> rquickjs::Result<Module<'js, rquickjs::module::Declared>> {
            let specifier = name.strip_prefix("virtual:").unwrap_or(name);
            let module_source = if specifier == "typebox" {
                crate::glue::TYPEBOX_SHIM
            } else {
                source(specifier).ok_or(rquickjs::Error::Unknown)?
            };
            Module::declare(ctx.clone(), name, module_source)
        }
    }

    /// Evaluates one TypeScript fixture in a standalone QuickJS runtime whose
    /// loader serves only the table under test (plus the E1 typebox shim).
    /// The fixture assigns a JSON-serialisable value to `__result`.
    fn evaluate(fixture: &str) -> Value {
        let runtime = Runtime::new().expect("runtime");
        runtime.set_loader(TestResolver, TestLoader);
        let context = Context::full(&runtime).expect("context");
        let module_source = crate::transpile::transpile(Path::new("fixture.ts"), fixture)
            .expect("transpile fixture");
        let result: String = context
            .with(|ctx| -> rquickjs::Result<String> {
                let ops = Object::new(ctx.clone())?;
                ops.set(
                    "getModel",
                    Function::new(ctx.clone(), || -> String {
                        json!({
                            "provider": "test",
                            "id": "test-model",
                            "type": "language",
                            "reasoning": true,
                            "thinkingLevelMap": { "xhigh": "high", "max": "max" },
                        })
                        .to_string()
                    })?,
                )?;
                ctx.globals().set("__piOps", ops)?;
                let (_module, _promise) =
                    Module::declare(ctx.clone(), "fixture", module_source)?.eval()?;
                ctx.globals().get("__result")
            })
            .expect("evaluate fixture");
        serde_json::from_str(&result).expect("fixture result is JSON")
    }

    #[test]
    fn table_covers_every_documented_specifier() {
        assert_eq!(host_package_modules().len(), HOST_PACKAGE_SPECIFIERS.len());
        for (specifier, module_source) in host_package_modules() {
            assert_eq!(Some(module_source), source(specifier));
            assert!(!module_source.is_empty(), "empty shim for {specifier}");
            assert!(
                HOST_PACKAGE_SPECIFIERS.contains(&specifier),
                "undeclared specifier {specifier}"
            );
        }
        assert!(source("@earendil-works/pi-codemode").is_none());
        assert!(source("node:fs").is_none());
    }

    #[test]
    fn pi_ai_exports_model_helpers_and_typebox_view() {
        let result = evaluate(
            r#"
            import {
                getModel, modelsAreEqual, getSupportedThinkingLevels,
                clampThinkingLevel, contentText, getSystemMessageText,
                retryDelayMs, StringEnum, Type, imageErrorResult,
                oauthSuccessHtml, PI_ATTRIBUTION,
            } from "@earendil-works/pi-ai";
            const model = getModel();
            const err = imageErrorResult(model, new Error("boom"));
            globalThis.__result = JSON.stringify({
                id: model.id,
                levels: getSupportedThinkingLevels(model),
                clamped: clampThinkingLevel(model, "xhigh"),
                equal: modelsAreEqual(model, { type: "language", provider: "test", id: "test-model" }),
                text: contentText([{ type: "text", text: "a" }, { type: "image" }, { type: "text", text: "b" }], "-"),
                systemText: getSystemMessageText({ content: "base", sections: { notes: "extra" } }),
                delay: retryDelayMs({ baseDelayMs: 100, maxAgentDelayMs: 500 }, 5),
                stringEnum: StringEnum(["a", "b"], { description: "choice" }),
                typeKind: Type.String({ minLength: 1 }).type,
                error: err.stopReason,
                oauth: oauthSuccessHtml("done").includes("done"),
                attribution: PI_ATTRIBUTION.name,
            });
            "#,
        );
        assert_eq!(result["id"], "test-model");
        assert_eq!(
            result["levels"],
            json!(["off", "minimal", "low", "medium", "high", "xhigh", "max"])
        );
        assert_eq!(result["clamped"], "xhigh");
        assert_eq!(result["equal"], true);
        assert_eq!(result["text"], "a-b");
        assert_eq!(result["systemText"], "base\n\nextra");
        assert_eq!(result["delay"], 500);
        assert_eq!(result["stringEnum"]["type"], "string");
        assert_eq!(result["stringEnum"]["enum"], json!(["a", "b"]));
        assert_eq!(result["stringEnum"]["description"], "choice");
        assert_eq!(result["typeKind"], "string");
        assert_eq!(result["error"], "error");
        assert_eq!(result["oauth"], true);
        assert_eq!(result["attribution"], "pi");
    }

    #[test]
    fn pi_tui_constructors_render_and_helpers_work() {
        let result = evaluate(
            r#"
            import {
                Container, Spacer, Text, Input, SelectList, Box,
                visibleWidth, truncateToWidth, Key,
            } from "@earendil-works/pi-tui";
            const container = new Container();
            container.addChild(new Text("hello", 1, 0));
            container.addChild(new Spacer(2));
            const list = new SelectList([{ value: "a", label: "Alpha" }], 5, {});
            list.onSelect = (item) => { globalThis.__selected = item.value; };
            list.handleInput("\r");
            const input = new Input();
            input.handleInput("hi");
            const box = new Box("title");
            box.setChild(new Text("body", 0, 0));
            globalThis.__result = JSON.stringify({
                rows: container.render(12).length,
                spacer: new Spacer(3).render(5).length,
                text: new Text("hi", 0, 0).render(6)[0].trim(),
                width: visibleWidth("\x1b[31mred\x1b[0m"),
                truncated: truncateToWidth("abcdef", 3, ""),
                selected: globalThis.__selected,
                input: input.getValue(),
                boxRows: box.render(10).length,
                key: Key.up,
            });
            "#,
        );
        assert_eq!(result["rows"], 3);
        assert_eq!(result["spacer"], 3);
        assert_eq!(result["text"], "hi");
        assert_eq!(result["width"], 3);
        assert_eq!(result["truncated"], "abc");
        assert_eq!(result["selected"], "a");
        assert_eq!(result["input"], "hi");
        assert_eq!(result["boxRows"], 3);
        assert_eq!(result["key"], "up");
    }

    #[test]
    fn pi_agent_core_tool_result_shapes_match_upstream() {
        let result = evaluate(
            r#"
            import {
                textContent, imageContent, toolResult, textToolResult,
                errorToolResult, isToolResult, isErrorToolResult,
                createToolResultMessage,
            } from "@earendil-works/pi-agent-core";
            const result = toolResult([textContent("ok")], { count: 1 }, { isError: false });
            const failed = errorToolResult("bad");
            const message = createToolResultMessage({ id: "call-1", name: "read" }, result);
            globalThis.__result = JSON.stringify({
                content: result.content,
                details: result.details,
                isError: result.isError,
                text: textToolResult("hi").content[0].text,
                image: imageContent("aGk=", "image/png"),
                valid: isToolResult(result),
                invalid: isToolResult({ nope: true }),
                failed: isErrorToolResult(failed),
                messageRole: message.role,
                messageId: message.toolCallId,
            });
            "#,
        );
        assert_eq!(result["content"], json!([{ "type": "text", "text": "ok" }]));
        assert_eq!(result["details"], json!({ "count": 1 }));
        assert_eq!(result["isError"], false);
        assert_eq!(result["text"], "hi");
        assert_eq!(
            result["image"],
            json!({ "type": "image", "data": "aGk=", "mimeType": "image/png" })
        );
        assert_eq!(result["valid"], true);
        assert_eq!(result["invalid"], false);
        assert_eq!(result["failed"], true);
        assert_eq!(result["messageRole"], "toolResult");
        assert_eq!(result["messageId"], "call-1");
    }

    #[test]
    fn codemode_source_grammar_matches_the_rust_crate() {
        let result = evaluate(
            r#"
            import { CODEMODE_SOURCE_GRAMMAR, CODEMODE_OPTIONS_PREFIX } from "@earendil-works/pi-codemode/source";
            globalThis.__result = JSON.stringify({
                grammar: CODEMODE_SOURCE_GRAMMAR,
                prefix: CODEMODE_OPTIONS_PREFIX,
            });
            "#,
        );
        assert_eq!(result["grammar"], GRAMMAR);
        assert_eq!(result["prefix"], "// @options:");
    }

    #[test]
    fn type_only_imports_are_erased_and_harmless() {
        let result = evaluate(
            r#"
            import type { AgentToolResult } from "@earendil-works/pi-agent-core";
            import type { SelectItem } from "@earendil-works/pi-tui";
            import type { Model } from "@earendil-works/pi-ai";
            const value: AgentToolResult = { content: [], details: {} };
            const item: SelectItem = { value: "v", label: "L" };
            const model: Model = getModel();
            globalThis.__result = JSON.stringify({ value, item, model: model.id });
            function getModel() { return { id: "typed" }; }
            "#,
        );
        assert_eq!(result["value"]["content"], json!([]));
        assert_eq!(result["item"]["label"], "L");
        assert_eq!(result["model"], "typed");
    }
}
