//! E4 CJS interop exit gate: CommonJS extensions load through the same CLI
//! path as ESM ones (`loader::load_paths`), with jiti-style `module.exports`,
//! `exports.x`, and `require()` semantics (upstream
//! `core/extensions/loader.ts:42, 571-602, 659`).
//!
//! Fixtures are synthetic temp files; no live pi data root is touched.

use std::path::{Path, PathBuf};

use pi_ext_js::{ExtensionRunner, HostCore, loader::load_paths};
use serde_json::{Value as Json, json};

fn temp_tree(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pi-ext-js-cjs-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir parent");
    }
    std::fs::write(path, contents).expect("write");
}

fn runner() -> ExtensionRunner {
    ExtensionRunner::new(HostCore {
        cwd: "/tmp".to_string(),
        mode: "print".to_string(),
        settings: json!({}),
        ..Default::default()
    })
    .expect("runner")
}

fn tool_text(runner: &mut ExtensionRunner, name: &str) -> String {
    let result: Json = runner
        .execute_tool(name, "call-1", &json!({}))
        .expect("execute tool");
    result["content"][0]["text"]
        .as_str()
        .expect("text content")
        .to_string()
}

#[test]
fn cjs_entry_registers_a_tool_and_a_command() {
    let dir = temp_tree("entry");
    let entry = dir.join("cjs-entry.js");
    write(
        &entry,
        r#"
const { Type } = require("typebox");
module.exports = function (pi) {
  pi.registerTool({
    name: "cjs_tool",
    label: "cjs_tool",
    description: "Registered by a CommonJS extension.",
    parameters: { type: "object", properties: {} },
    execute: async () => ({ content: [{ type: "text", text: Type.String({}).type }], details: {} }),
  });
  pi.registerCommand("cjs-cmd", { description: "CJS command", handler: () => {} });
};
"#,
    );

    let mut runner = runner();
    let result = load_paths(&mut runner, &[entry.clone()]);
    assert_eq!(
        result.extensions,
        vec![entry.to_string_lossy().into_owned()]
    );
    assert_eq!(result.errors.len(), 0, "{:?}", result.errors);
    assert!(
        runner.tool("cjs_tool").is_some(),
        "CJS tool was not registered"
    );
    assert!(
        runner
            .resolved_commands()
            .iter()
            .any(|command| command.name == "cjs-cmd"),
        "CJS command was not registered"
    );
    runner.bind().expect("bind");
    assert_eq!(tool_text(&mut runner, "cjs_tool"), "string");
}

#[test]
fn mixed_esm_cjs_requires_share_the_resolver_and_virtual_table() {
    let dir = temp_tree("mixed");
    let cjs_helper = dir.join("helper-cjs.js");
    let esm_helper = dir.join("helper-esm.js");
    let cjs_entry = dir.join("bridge.js");
    let cjs_factory = dir.join("cjs-factory.js");
    let esm_entry = dir.join("esm-entry.ts");
    write(&cjs_helper, "exports.greet = (name) => \"cjs:\" + name;\n");
    write(&esm_helper, "export const tag = \"esm\";\n");
    write(
        &cjs_entry,
        r#"
const helper = require("./helper-cjs.js");
const esm = require("./helper-esm.js");
module.exports = function (pi) {
  pi.registerTool({
    name: "cjs_bridge",
    label: "cjs_bridge",
    description: "CJS requiring CJS and ESM.",
    parameters: { type: "object", properties: {} },
    execute: async () => ({ content: [{ type: "text", text: helper.greet(esm.tag) }], details: {} }),
  });
};
"#,
    );
    write(
        &cjs_factory,
        r#"
module.exports = function (pi) {
  pi.registerTool({
    name: "from_esm_import",
    label: "from_esm_import",
    description: "CJS factory imported by ESM.",
    parameters: { type: "object", properties: {} },
    execute: async () => ({ content: [{ type: "text", text: "esm-to-cjs" }], details: {} }),
  });
};
"#,
    );
    write(
        &esm_entry,
        "import make from \"./cjs-factory.js\";\nexport default function (pi) { make(pi); }\n",
    );

    let mut runner = runner();
    let result = load_paths(&mut runner, &[cjs_entry, esm_entry]);
    assert_eq!(result.errors.len(), 0, "{:?}", result.errors);
    assert_eq!(result.extensions.len(), 2);
    runner.bind().expect("bind");
    assert_eq!(tool_text(&mut runner, "cjs_bridge"), "cjs:esm");
    assert_eq!(tool_text(&mut runner, "from_esm_import"), "esm-to-cjs");
}

#[test]
fn invalid_modules_report_upstream_factory_and_resolution_errors() {
    let dir = temp_tree("invalid");
    let bad_factory = dir.join("bad-factory.js");
    let unknown = dir.join("unknown.js");
    let good = dir.join("good.js");
    write(&bad_factory, "module.exports = 42;\n");
    write(
        &unknown,
        "const missing = require(\"no-such-pkg\");\nmodule.exports = function () { void missing; };\n",
    );
    write(
        &good,
        "export default function (pi) { pi.on(\"after\", () => 1); }\n",
    );

    let mut runner = runner();
    let result = load_paths(&mut runner, &[bad_factory, unknown.clone(), good.clone()]);
    assert_eq!(result.extensions, vec![good.to_string_lossy().into_owned()]);
    assert_eq!(result.errors.len(), 2);
    assert_eq!(
        result.errors[0].error,
        format!(
            "Extension does not export a valid factory function: {}",
            result.errors[0].path
        )
    );
    assert!(
        result.errors[1]
            .error
            .contains("could not be resolved by the extensions host"),
        "{:?}",
        result.errors[1]
    );
    assert!(runner.has_handlers("after"), "the valid ESM still loads");
}
