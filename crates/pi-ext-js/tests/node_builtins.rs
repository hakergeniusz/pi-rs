//! E4 exit gate: an extension importing the `node:*` built-ins and host
//! packages loads in the embedded host and observes the upstream API shape.
//!
//! The fixture under `tests/fixtures/node-builtins/` exercises one
//! representative call per module (buffer, child_process, crypto, fs,
//! fs/promises, os, path, process, url, util) plus the state-backed
//! `@earendil-works/pi-coding-agent` and `@earendil-works/pi-mcp` packages.
//! A second test pins the E1 rejection string for packages the port does
//! not bundle.

use pi_ext_js::resolver::{is_unavailable_host_module, node, virtual_module_source};
use pi_ext_js::{HostCore, JsHost};
use serde_json::{Value, json};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/node-builtins/index.ts"
);

fn host() -> JsHost {
    let core = HostCore {
        cwd: "/tmp".to_string(),
        mode: "print".to_string(),
        settings: json!({"model": "glm-5.3-flash"}),
        all_tools: vec![json!({"name": "existing-tool"})],
        active_tools: vec!["existing-tool".to_string()],
        registered_tool_names: vec!["existing-tool".to_string()],
        ..Default::default()
    };
    JsHost::new(core).expect("host")
}

fn load_fixture(host: &JsHost) -> Value {
    let index = host.begin_extension(FIXTURE).expect("begin");
    host.load_extension_module(index, FIXTURE, None)
        .expect("load fixture");
    host.commit_extension(index).expect("commit");
    let raw = host
        .eval_for_tests("globalThis.__nodeResult")
        .expect("eval result");
    serde_json::from_str(&raw).expect("result JSON")
}

#[test]
fn fixture_extension_imports_and_calls_every_served_module() {
    let host = host();
    let result = load_fixture(&host);
    assert_eq!(result["buffer"], "6869");
    assert_eq!(result["bufferAlloc"], "aaa");
    assert_eq!(
        result["hash"],
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert!(result["uuidShape"].as_bool().unwrap_or(false));
    assert_eq!(result["fsRead"], "fixture");
    assert!(result["fsExists"].as_bool().unwrap_or(false));
    assert_eq!(result["fsAsync"], "fixture");
    assert_eq!(result["pathJoin"], "a/b/c");
    assert_eq!(result["pathResolve"], "/work/y");
    assert_eq!(result["pathRelative"], "../b");
    assert_eq!(result["osPlatform"], std::env::consts::OS);
    assert_eq!(result["osHome"], "string");
    assert_eq!(result["processCwd"], "/tmp");
    assert_eq!(result["urlFromPath"], "/tmp/a b.txt");
    assert_eq!(result["urlToPath"], "file:///tmp/a%20b.txt");
    assert_eq!(result["exec"], "hello");
    assert_eq!(result["spawnStatus"], 0);
    assert_eq!(result["utilFormat"], "x=3");
    assert_eq!(result["utilInspect"], "{ a: 1 }");
    assert!(result["utilTypes"].as_bool().unwrap_or(false));
    assert_eq!(result["settings"], "glm-5.3-flash");
    assert_eq!(result["sessionCwd"], "/tmp");
    assert_eq!(result["tools"], 1);
    assert_eq!(result["activeTools"], 1);
    assert_eq!(result["mcpNamespace"], "mcp__my_server");
    assert_eq!(result["mcpToolName"], "mcp__srv__do_thing");
    assert!(
        result["fsBasename"]
            .as_str()
            .unwrap_or_default()
            .starts_with("pi-ext-js-node-fixture-")
    );
    assert_eq!(result["fsExt"], ".txt");
}

#[test]
fn genuinely_unavailable_host_packages_keep_the_e1_error() {
    let host = host();
    let index = host.begin_extension("<unavailable>").expect("begin");
    let err = host
        .load_extension_module(
            index,
            "<unavailable>",
            Some(
                "import { Text } from \"@earendil-works/pi-tui\";\n\
                 export default function (): void { void Text; }\n",
            ),
        )
        .expect_err("unavailable host package must reject");
    assert!(
        err.to_string()
            .contains("node_modules resolution lands in E4"),
        "{err}"
    );
}

#[test]
fn resolver_table_matches_the_documented_coverage() {
    for specifier in node::NODE_BUILTINS {
        assert!(
            virtual_module_source(specifier).is_some(),
            "missing built-in shim for {specifier}"
        );
    }
    for specifier in node::HOST_PACKAGES {
        assert!(
            virtual_module_source(specifier).is_some(),
            "missing host-package shim for {specifier}"
        );
        assert!(!is_unavailable_host_module(specifier));
    }
    assert!(is_unavailable_host_module("@earendil-works/pi-tui"));
    assert!(virtual_module_source("node:sqlite").is_none());
    assert!(virtual_module_source("@earendil-works/pi-ai").is_none());
    // E1's already-served modules remain untouched.
    assert!(virtual_module_source("typebox").is_some());
    assert!(virtual_module_source("@earendil-works/pi-codemode/declarations").is_some());
}
