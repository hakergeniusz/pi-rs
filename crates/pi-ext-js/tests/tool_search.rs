//! E1 exit gate, part 2: the bundled `tool-search` extension registers and
//! executes identically in the embedded host.
//!
//! The fixture under `tests/fixtures/tool-search/` is upstream's own
//! extension, vendored verbatim. These tests drive it end to end — load
//! (transpile + relative import + virtual modules + `Intl.Segmenter`
//! shim) → factory → `pi.registerTool` → `bind()` → `execute_tool` — and
//! assert the upstream behaviors from
//! `packages/coding-agent/test/tool-search.test.ts` and the tool contract
//! (`extensions/tool-search/tool.ts`): inactive until activated, searches
//! `codemode`/`deferred` tools only, loads matches into the active set,
//! exact-name queries rank first, and input validation throws.

use pi_ext_js::{ExtensionRunner, HostCore};
use serde_json::json;

const TOOL_SEARCH_INDEX: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/tool-search/index.ts"
);

/// The upstream relevance corpus (`tool-search.test.ts:59-67`), registered
/// with the exposures `tool_search` searches.
const RELEVANCE_CORPUS: &str = r#"
export default function (pi) {
  pi.registerTool({
    name: "mcp__github__list_issues", label: "mcp__github__list_issues",
    description: "List issues in a repository.",
    parameters: { type: "object", properties: { state: { type: "string", description: "open or closed" } } },
    exposure: "deferred",
    execute: async () => ({ content: [], details: {} }),
  });
  pi.registerTool({
    name: "mcp__github__create_pull_request", label: "mcp__github__create_pull_request",
    description: "Open a pull request.",
    parameters: { type: "object", properties: {} },
    exposure: "codemode",
    execute: async () => ({ content: [], details: {} }),
  });
  pi.registerTool({
    name: "mcp__linear__search_issues", label: "mcp__linear__search_issues",
    description: "Search Linear issues by text.",
    parameters: { type: "object", properties: {} },
    exposure: "deferred",
    execute: async () => ({ content: [], details: {} }),
  });
  pi.registerTool({
    name: "mcp__docs__search", label: "mcp__docs__search",
    description: "Search the documentation.",
    parameters: { type: "object", properties: {} },
    exposure: "codemode",
    execute: async () => ({ content: [], details: {} }),
  });
}
"#;

/// The #11331 name corpus (`tool-search.test.ts:107-127`).
const NAME_CORPUS: &str = r#"
export default function (pi) {
  pi.registerTool({
    name: "mcp__app__queue_read", label: "mcp__app__queue_read",
    description: "Read a queued message of this thread.",
    parameters: { type: "object", properties: {} },
    exposure: "deferred",
    execute: async () => ({ content: [], details: {} }),
  });
  pi.registerTool({
    name: "mcp__dev-app__thread_read", label: "mcp__dev-app__thread_read",
    description: "Read durable state and a paginated timeline from any thread. The default view returns user messages, assistant messages and plans. Continue with afterPosition until hasMore is false.",
    parameters: { type: "object", properties: {} },
    exposure: "deferred",
    execute: async () => ({ content: [], details: {} }),
  });
}
"#;

/// The namespace corpus (`tool-search.test.ts:187-195`).
const NAMESPACE_CORPUS: &str = r#"
export default function (pi) {
  pi.registerTool({
    name: "mcp__x__run", label: "mcp__x__run",
    description: "Run it.",
    parameters: { type: "object", properties: {} },
    namespace: { name: "mcp__x", description: "Kubernetes cluster tools" },
    exposure: "deferred",
    execute: async () => ({ content: [], details: {} }),
  });
}
"#;

fn runner_with(corpus: &str) -> ExtensionRunner {
    let mut runner = ExtensionRunner::new(HostCore {
        cwd: "/tmp".to_string(),
        mode: "print".to_string(),
        settings: json!({}),
        ..Default::default()
    })
    .expect("runner");
    runner
        .load_extension("<corpus>", Some(corpus))
        .expect("corpus loads");
    // The vendored extension loads from disk: transpile, relative import,
    // virtual modules, and the Intl shim all in the real path.
    runner
        .load_extension(TOOL_SEARCH_INDEX, None)
        .expect("tool-search loads");
    runner.bind().expect("bind");
    // `tool_search` is registered inactive (defaultActive: false);
    // activating it is the --tools / defaultTools / setActiveTools path.
    let mut active = runner.active_tools();
    active.push("tool_search".to_string());
    runner.set_active_tools(active);
    runner
}

fn loaded(runner: &mut ExtensionRunner, args: &serde_json::Value) -> (Vec<String>, String) {
    let result = runner
        .execute_tool("tool_search", "t1", args)
        .expect("tool_search executes");
    let text = result["content"][0]["text"]
        .as_str()
        .expect("text")
        .to_string();
    let loaded = result["details"]["loaded"]
        .as_array()
        .map(|names| {
            names
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    (loaded, text)
}

#[test]
fn tool_search_is_registered_inactive_and_activation_declares_it() {
    let runner = runner_with(RELEVANCE_CORPUS);
    // Before activation: only the declared exposures are active; the
    // deferred/codemode candidates and tool_search are not.
    assert!(runner.tool("tool_search").is_some());
    let tool_search = runner.tool("tool_search").expect("tool_search");
    assert_eq!(tool_search.exposure, "model-only");
    assert!(!tool_search.default_active);
}

#[test]
fn tool_search_ranks_loads_and_activates_matches() {
    let mut runner = runner_with(RELEVANCE_CORPUS);
    assert!(
        !runner
            .active_tools()
            .contains(&"mcp__linear__search_issues".to_string())
    );
    let (loaded_names, text) = loaded(&mut runner, &json!({ "query": "issue" }));
    assert_eq!(
        loaded_names,
        vec![
            "mcp__linear__search_issues".to_string(),
            "mcp__github__list_issues".to_string()
        ]
    );
    assert_eq!(
        text,
        "Loaded 2 tools. They are available from your next call:\n\
         - mcp__linear__search_issues: Search Linear issues by text.\n\
         - mcp__github__list_issues: List issues in a repository."
    );
    // Activation is recorded in the active set, so the matches are
    // declared for the next model call.
    assert!(
        runner
            .active_tools()
            .contains(&"mcp__linear__search_issues".to_string())
    );
    assert!(
        runner
            .active_tools()
            .contains(&"mcp__github__list_issues".to_string())
    );
    // Already-active candidates leave the searchable pool.
    let (loaded_again, text_again) = loaded(&mut runner, &json!({ "query": "issue" }));
    assert_eq!(loaded_again, Vec::<String>::new());
    assert_eq!(text_again, "No matching tools found.");
}

#[test]
fn tool_search_ranks_by_relevance_respects_limits_and_propeties() {
    // "pull requests" finds the pull-request tool first.
    let mut runner = runner_with(RELEVANCE_CORPUS);
    let (loaded_names, _) = loaded(&mut runner, &json!({ "query": "pull requests" }));
    assert_eq!(loaded_names[0], "mcp__github__create_pull_request");
    // The limit caps the results.
    let mut runner = runner_with(RELEVANCE_CORPUS);
    let (loaded_names, text) = loaded(&mut runner, &json!({ "query": "search", "limit": 1 }));
    assert_eq!(loaded_names.len(), 1);
    assert!(text.starts_with("Loaded 1 tool. "));
    // Property names and descriptions are searchable.
    let mut runner = runner_with(RELEVANCE_CORPUS);
    let (loaded_names, _) = loaded(&mut runner, &json!({ "query": "closed" }));
    assert_eq!(loaded_names, vec!["mcp__github__list_issues".to_string()]);
}

#[test]
fn tool_search_exact_name_queries_rank_first() {
    // The five #11331 queries at the upstream test's limit of 1, each
    // against a fresh host (a loaded match leaves the searchable pool).
    for query in [
        "thread_read",
        "dev_app__thread_read",
        "mcp__dev-app__thread_read",
        "Thread_Read",
        "thread read",
    ] {
        let mut runner = runner_with(NAME_CORPUS);
        let (loaded_names, _) = loaded(&mut runner, &json!({ "query": query, "limit": 1 }));
        assert_eq!(
            loaded_names,
            vec!["mcp__dev-app__thread_read".to_string()],
            "query: {query}"
        );
    }
    // Partial names still rank by BM25.
    let mut runner = runner_with(NAME_CORPUS);
    let (loaded_names, _) = loaded(&mut runner, &json!({ "query": "read", "limit": 1 }));
    assert_eq!(loaded_names, vec!["mcp__app__queue_read".to_string()]);
}

#[test]
fn tool_search_includes_the_namespace_in_the_search_text() {
    let mut runner = runner_with(NAMESPACE_CORPUS);
    let (loaded_names, _) = loaded(&mut runner, &json!({ "query": "kubernetes" }));
    assert_eq!(loaded_names, vec!["mcp__x__run".to_string()]);
}

#[test]
fn tool_search_validates_input_like_upstream() {
    let mut runner = runner_with(RELEVANCE_CORPUS);
    let error = runner
        .execute_tool("tool_search", "t1", &json!({ "query": "   " }))
        .expect_err("empty query rejects");
    assert!(
        error.to_string().contains("query must not be empty"),
        "{error}"
    );
    let error = runner
        .execute_tool("tool_search", "t1", &json!({ "query": "x", "limit": 0 }))
        .expect_err("limit 0 rejects");
    assert!(
        error
            .to_string()
            .contains("limit must be a positive integer"),
        "{error}"
    );
}
