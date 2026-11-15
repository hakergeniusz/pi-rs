/**
 * Vendored verbatim from earendil-works/pi 1.5.2
 * (packages/coding-agent/src/extensions/tool-search/index.ts), MIT License,
 * Copyright (c) Mario Zechner and contributors.
 *
 * E1 exit-gate fixture: this is upstream's own bundled extension, loaded
 * unmodified by the embedded host — the crate tests assert it registers and
 * executes identically to upstream.
 */

/**
 * The `tool_search` tool as an extension. The CLI loads it as a built-in extension; SDK users add
 * `createToolSearchExtension()` to their extension factories.
 *
 * `tool_search` is registered inactive. Activate it with `--tools`, the `defaultTools` setting, or
 * `setActiveTools()`.
 */

import type { ExtensionFactory } from "../../core/extensions/types.ts";
import { createToolSearchToolDefinition } from "./tool.ts";

export function createToolSearchExtension(): ExtensionFactory {
	return (pi) => {
		pi.registerTool({ ...createToolSearchToolDefinition({ tools: pi }), defaultActive: false });
	};
}

export default createToolSearchExtension();
