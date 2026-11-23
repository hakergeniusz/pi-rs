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
//! same sources. The coordinator registers [`host_package_modules`] in
//! `resolver::virtual_module_source` and [`HOST_PACKAGE_SPECIFIERS`] in the
//! availability check (see `WIRING.md`).
//!
//! ## Extension-surface patches
//!
//! Real-world extensions import more named exports from these packages than
//! the bundled ones do ([`composed_source`] pairs each patch with the base
//! shim). The patches add faithful data-shape implementations where the port
//! can supply them (`convertToLlm`, `matchesKey`, `AssistantMessageEventStream`,
//! `parseSessionEntries`, `Type.Literal`, …) and correct-shaped throwing stubs
//! where the underlying state is Rust-owned (provider catalogs, disk sessions,
//! streaming transports). Every patch cites its upstream source inline.
//! `ExtensionLoader` declares the composed source, so the unit tests in this
//! module exercise exactly what an importing extension sees.

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

/// Additional named exports appended to a base shim at load time.
///
/// The base shims live in `node/host/*.js` and are owned by the node host
/// workstream; these patches are the extension-host side of the same virtual
/// modules. Keeping them here means a host package can gain an export without
/// forking its Rust-owned state surface. [`composed_source`] pairs each patch
/// with the base source served by [`crate::resolver::virtual_module_source`].
///
/// Ground truth for each patch is cited inline. Where the port owns no state
/// for an upstream export (provider catalogs, disk sessions, streaming
/// transports) the patch returns an empty shape or throws the same
/// "not available in the embedded extensions host" error the base shim uses,
/// never a plausible-looking fake that could silently corrupt extension work.
pub const PI_AI_PATCH: &str = r#"
// --- extension-surface additions -------------------------------------------
// Upstream: packages/ai/src/utils/event-stream.ts:25-141 (EventStream /
// AssistantMessageEventStream / createAssistantMessageEventStream),
// packages/ai/src/utils/transcript.ts:99-103 (getCurrentSystemPrompt),
// packages/ai/src/api/openai-responses.lazy.ts:4 (openAIResponsesApi),
// packages/ai/src/providers/all.ts:137-181 (builtinProviders).

class __E4FifoQueue {
  constructor() {
    this.incoming = [];
    this.outgoing = [];
  }
  get length() {
    return this.incoming.length + this.outgoing.length;
  }
  enqueue(value) {
    this.incoming.push(value);
  }
  dequeue() {
    if (this.outgoing.length === 0) {
      while (this.incoming.length > 0) this.outgoing.push(this.incoming.pop());
    }
    return this.outgoing.pop();
  }
}

class AssistantMessageEventStream {
  constructor() {
    this.queue = new __E4FifoQueue();
    this.waiting = new __E4FifoQueue();
    this.done = false;
    this.startedAt = Date.now();
    this.startedAtMonotonic = typeof performance !== "undefined" ? performance.now() : Date.now();
    this.finalResultPromise = new Promise((resolve) => {
      this.resolveFinalResult = resolve;
    });
  }
  time(message) {
    if (this.done || message === undefined || message === null) return;
    if (message.durationMs !== undefined) return;
    if (typeof message.timestamp === "string" && Date.parse(message.timestamp) < this.startedAt) return;
    const now = typeof performance !== "undefined" ? performance.now() : Date.now();
    message.durationMs = Math.max(0, Math.round(now - this.startedAtMonotonic));
  }
  push(event) {
    if (this.done) return;
    if (event && (event.type === "done" || event.type === "error")) {
      this.done = true;
      this.resolveFinalResult(event.type === "done" ? event.message : event.error);
      this.time(event.type === "done" ? event.message : event.error);
    }
    const waiter = this.waiting.dequeue();
    if (waiter) waiter({ value: event, done: false });
    else this.queue.enqueue(event);
  }
  end(result) {
    if (this.done) return;
    this.done = true;
    if (result !== undefined) {
      this.time(result);
      this.resolveFinalResult(result);
    }
    while (this.waiting.length > 0) this.waiting.dequeue()({ value: undefined, done: true });
  }
  async *[Symbol.asyncIterator]() {
    while (true) {
      if (this.queue.length > 0) {
        yield this.queue.dequeue();
      } else if (this.done) {
        return;
      } else {
        const result = await new Promise((resolve) => this.waiting.enqueue(resolve));
        if (result.done) return;
        yield result.value;
      }
    }
  }
  result() {
    return this.finalResultPromise;
  }
}

/** Factory function for AssistantMessageEventStream (for use in extensions). */
export function createAssistantMessageEventStream() {
  return new AssistantMessageEventStream();
}

/** `getCurrentSystemPrompt` (transcript.ts:99): the leading system text. */
export function getCurrentSystemPrompt(messages) {
  const message = getCurrentSystemMessage(messages);
  return message ? contentText(message.content) : "";
}

/** `openAIResponsesApi` (openai-responses.lazy.ts:4): a provider stream bundle.
 *  Streaming is Rust-owned here, so each method throws the base shim's exact
 *  "not available" error instead of pretending to call a model. */
export const openAIResponsesApi = () => ({
  stream: unavailable("openAIResponsesApi.stream"),
  streamSimple: unavailable("openAIResponsesApi.streamSimple"),
  completeSimple: unavailable("openAIResponsesApi.completeSimple"),
});

/** `builtinProviders` (providers/all.ts:137): the catalog is Rust-owned; the
 *  embedded host exposes no built-in provider objects to JS. */
export function builtinProviders() {
  return [];
}

/** `complete` (compat.ts:269): one-shot completion over `stream`. The
 *  embedded host's `stream` is the documented "not available" stub, so the
 *  export exists for extension linkers but a call still fails explicitly
 *  instead of pretending to reach a model. */
export async function complete(model, context, options) {
  const s = stream(model, context, options);
  return s.result();
}
"#;

pub const PI_TUI_PATCH: &str = r#"
// --- extension-surface additions -------------------------------------------
// Upstream: packages/tui/src/keys.ts:909-1060 (matchesKey), :163-245 (Key
// modifier helpers), packages/tui/src/utils.ts:313 (stripTerminalSequences),
// :953 (wrapTextWithAnsi), packages/tui/src/components/editor.ts:297 (Editor),
// packages/tui/src/components/mouse-region.ts:12 (MouseRegion).

/** Parses `ctrl+shift+p` / `Key.shift("tab")` into modifiers plus a key. */
function __parseKeyId(keyId) {
  const parts = String(keyId ?? "").split("+");
  const key = parts.pop();
  if (!key || parts.some((part) => !["ctrl", "shift", "alt", "super"].includes(part))) return null;
  return {
    key,
    ctrl: parts.includes("ctrl"),
    shift: parts.includes("shift"),
    alt: parts.includes("alt"),
    super: parts.includes("super"),
  };
}

const __ARROWS = {
  up: ["\u001b[A", "\u001bOA"],
  down: ["\u001b[B", "\u001bOB"],
  right: ["\u001b[C", "\u001bOC"],
  left: ["\u001b[D", "\u001bOD"],
};

/** `matchesKey(data, keyId)`: the raw-sequence cases extensions actually use.
 *  Kitty CSI-u / modifyOtherKeys variants are intentionally not synthesised
 *  by the headless host. */
export function matchesKey(data, keyId) {
  const parsed = __parseKeyId(keyId);
  if (!parsed) return false;
  const input = String(data ?? "");
  const { key, ctrl, shift, alt, super: superKey } = parsed;
  const plain = !(ctrl || shift || alt || superKey);
  switch (key) {
    case "escape":
    case "esc":
      return plain && input === "\u001b";
    case "enter":
    case "return":
      return plain && (input === "\r" || input === "\n" || input === "\u001bOM");
    case "tab":
      if (shift && !ctrl && !alt && !superKey) return input === "\u001b[Z";
      return plain && input === "\t";
    case "space":
      return plain && input === " ";
    case "backspace":
      return input === "\u007f" || input === "\b";
    case "delete":
      return input === "\u001b[3~" || input === "\u001b[3;5~" || (ctrl && input === "\u0004");
    case "home":
      return input === "\u001b[H" || input === "\u001bOH";
    case "end":
      return input === "\u001b[F" || input === "\u001bOF";
    case "pageUp":
      return input === "\u001b[5~";
    case "pageDown":
      return input === "\u001b[6~";
    default:
      break;
  }
  if (__ARROWS[key]) return __ARROWS[key].includes(input) && plain;
  if (/^f([1-9]|1[0-2])$/.test(key)) {
    const number = Number(key.slice(1));
    const prefixes = ["\u001bOP", "\u001bOQ", "\u001bOR", "\u001bOS"];
    if (number <= 4) return input === prefixes[number - 1];
    const csi = { 5: 15, 6: 17, 7: 18, 8: 19, 9: 20, 10: 21, 11: 23, 12: 24 }[number];
    return input === `\u001b[${csi}~`;
  }
  if (key.length !== 1) return false;
  if (ctrl) return input === String.fromCharCode(key.toUpperCase().charCodeAt(0) - 64);
  if (alt) return input === `\u001b${key}`;
  if (shift) return input === key.toUpperCase();
  return input === key;
}

// Key modifier helpers (`keys.ts:231-245`) are functions upstream; the base
// shim's Key object only carries the string constants, so add them in place.
Key.ctrl = (key) => `ctrl+${key}`;
Key.shift = (key) => `shift+${key}`;
Key.alt = (key) => `alt+${key}`;
Key.super = (key) => `super+${key}`;
Key.ctrlShift = (key) => `ctrl+shift+${key}`;
Key.shiftCtrl = (key) => `shift+ctrl+${key}`;
Key.ctrlAlt = (key) => `ctrl+alt+${key}`;
Key.altCtrl = (key) => `alt+ctrl+${key}`;
Key.shiftAlt = (key) => `shift+alt+${key}`;
Key.altShift = (key) => `alt+shift+${key}`;
Key.ctrlSuper = (key) => `ctrl+super+${key}`;
Key.superCtrl = (key) => `super+ctrl+${key}`;
Key.shiftSuper = (key) => `shift+super+${key}`;
Key.superShift = (key) => `super+shift+${key}`;

/** Strip ANSI CSI, OSC and APC sequences while keeping visible text. */
export function stripTerminalSequences(str) {
  return String(str ?? "")
    .replace(/\u001b\][^\u0007]*(?:\u0007|\u001b\\)/g, "")
    .replace(/\u001b_[^\u0007]*(?:\u0007|\u001b\\)/g, "")
    .replace(/\u001b\[[0-?]*[ -/]*[@-~]/g, "")
    .replace(/\u001b[@-Z\\-_]/g, "");
}

/** Word-wrap to `width` columns, keeping ANSI codes attached to their words. */
export function wrapTextWithAnsi(text, width) {
  if (!text) return [""];
  const limit = Math.max(1, width);
  const rows = [];
  for (const line of String(text).split(/\r\n|\r|\n/)) {
    if (visibleWidth(line) <= limit) {
      rows.push(line);
      continue;
    }
    const tokens = line.match(/\s+|\S+/g) ?? [];
    let row = "";
    for (const token of tokens) {
      if (!row.trim()) {
        row = token;
      } else if (visibleWidth(row + token) <= limit) {
        row += token;
      } else {
        rows.push(row.replace(/\s+$/, ""));
        row = token.replace(/^\s+/, "");
      }
    }
    rows.push(row);
  }
  return rows.length > 0 ? rows : [""];
}

/** Editor (editor.ts:297): the custom-prompt surface extensions drive via
 *  `setText` / `handleInput` / `render`, with `onSubmit` on Enter. */
export class Editor extends Input {
  constructor(tui, theme = {}) {
    super();
    this.tui = tui;
    this.theme = theme;
    this.onSubmit = undefined;
  }
  setText(text) {
    this.setValue(text);
  }
  getText() {
    return this.getValue();
  }
  handleInput(data) {
    if (data === "\r" || data === "\n") {
      const value = this.getValue();
      this.setValue("");
      if (this.onSubmit) this.onSubmit(value);
      return;
    }
    super.handleInput(data);
  }
}

/** MouseRegion (mouse-region.ts:12): forwards rendering, delegates events. */
export class MouseRegion extends Component {
  constructor(child, onMouse) {
    super();
    this.child = child;
    this.onMouse = onMouse;
  }
  render(width) {
    return this.child?.render?.(width) ?? [];
  }
  handleMouse(event) {
    return this.onMouse?.(event);
  }
  invalidate() {
    this.child?.invalidate?.();
  }
}
"#;

pub const PI_CODING_AGENT_PATCH: &str = r#"
// --- extension-surface additions -------------------------------------------
// Upstream: packages/coding-agent/src/config.ts:606,630 (CONFIG_DIR_NAME,
// getAgentDir), core/messages.ts:82-98,148-195 (convertToLlm),
// core/extensions/types.ts:1421 (isToolCallEventType),
// modes/interactive/theme/theme.ts:993,1065,937 (getLanguageFromPath,
// getMarkdownTheme, highlightCode), modes/interactive/interactive-mode.ts:469
// (InteractiveMode), core/session-manager.ts:988 (SessionManager).
// CONFIG_DIR_NAME follows the port's documented XDG deviation (`pi-rs`).

const __joinPath = (...parts) => {
  let out = "";
  for (const part of parts) {
    const piece = String(part ?? "");
    if (!piece) continue;
    if (piece.startsWith("/")) out = piece;
    else out = out ? `${out.replace(/\/$/, "")}/${piece.replace(/^\//, "")}` : piece;
  }
  return out || ".";
};
const __env = () => (typeof process !== "undefined" && process.env) || {};
const __home = () => __env().HOME || "/root";
const __expandTildePath = (path) => {
  const text = String(path);
  return text.startsWith("~/") ? __joinPath(__home(), text.slice(2)) : text;
};

/** The port's config leaf (`crates/pi-coding-agent/src/config.rs:42`). */
export const CONFIG_DIR_NAME = "pi-rs";

/** `$PI_CODING_AGENT_DIR` when set, else `$XDG_CONFIG_HOME`/`~/.config` +
 *  `pi-rs` (`config.rs:149-157`). */
export function getAgentDir() {
  const env = __env();
  const override = env.PI_CODING_AGENT_DIR;
  if (override) return __expandTildePath(override);
  const base = env.XDG_CONFIG_HOME && String(env.XDG_CONFIG_HOME).startsWith("/")
    ? env.XDG_CONFIG_HOME
    : __joinPath(__home(), ".config");
  return __joinPath(base, CONFIG_DIR_NAME);
}

const COMPACTION_SUMMARY_PREFIX = `The conversation history before this point was compacted into the following summary:\n\n<summary>\n`;
const COMPACTION_SUMMARY_SUFFIX = `\n</summary>`;
const BRANCH_SUMMARY_PREFIX = `The following is a summary of a branch that this conversation came back from:\n\n<summary>\n`;
const BRANCH_SUMMARY_SUFFIX = `\n</summary>`;

function __bashExecutionToText(msg) {
  let text = `Ran \`${msg.command}\`\n`;
  if (msg.output) text += `\`\`\`\n${msg.output}\n\`\`\``;
  else text += "(no output)";
  if (msg.cancelled) text += "\n\n(command cancelled)";
  else if (msg.exitCode !== null && msg.exitCode !== undefined && msg.exitCode !== 0) {
    text += `\n\nCommand exited with code ${msg.exitCode}`;
  }
  if (msg.truncated && msg.fullOutputPath) {
    text += `\n\n[Output truncated. Full output: ${msg.fullOutputPath}]`;
  }
  return text;
}

/** Convert AgentMessages (including custom types) to LLM-compatible messages. */
export function convertToLlm(messages) {
  return (messages ?? [])
    .map((message) => {
      switch (message?.role) {
        case "bashExecution":
          if (message.excludeFromContext) return undefined;
          return {
            role: "user",
            content: [{ type: "text", text: __bashExecutionToText(message) }],
            timestamp: message.timestamp,
          };
        case "custom":
          return {
            role: "user",
            content: typeof message.content === "string"
              ? [{ type: "text", text: message.content }]
              : message.content,
            timestamp: message.timestamp,
          };
        case "branchSummary":
          return {
            role: "user",
            content: [{ type: "text", text: BRANCH_SUMMARY_PREFIX + message.summary + BRANCH_SUMMARY_SUFFIX }],
            timestamp: message.timestamp,
          };
        case "compactionSummary":
          return {
            role: "user",
            content: [{ type: "text", text: COMPACTION_SUMMARY_PREFIX + message.summary + COMPACTION_SUMMARY_SUFFIX }],
            timestamp: message.timestamp,
          };
        case "system":
        case "user":
        case "assistant":
        case "toolResult":
          return message;
        default:
          return undefined;
      }
    })
    .filter((message) => message !== undefined);
}

/** `isToolCallEventType(toolName, event)` (types.ts:1421). */
export function isToolCallEventType(toolName, event) {
  return !!event && event.toolName === toolName;
}

const __EXT_TO_LANG = {
  ts: "typescript", tsx: "typescript", js: "javascript", jsx: "javascript",
  mjs: "javascript", cjs: "javascript", py: "python", rb: "ruby", rs: "rust",
  go: "go", java: "java", kt: "kotlin", swift: "swift", dart: "dart",
  groovy: "groovy", gradle: "groovy", nix: "nix", c: "c", h: "c",
  cpp: "cpp", cc: "cpp", cxx: "cpp", hpp: "cpp", cs: "csharp", php: "php",
  sh: "bash", bash: "bash", zsh: "bash", fish: "fish", ps1: "powershell",
  sql: "sql", html: "html", htm: "html", css: "css", scss: "scss",
  sass: "sass", less: "less", json: "json", yaml: "yaml", yml: "yaml",
  toml: "toml", xml: "xml", md: "markdown", markdown: "markdown",
  dockerfile: "dockerfile", makefile: "makefile", cmake: "cmake", lua: "lua",
  perl: "perl", r: "r", scala: "scala", clj: "clojure", ex: "elixir",
  exs: "elixir", erl: "erlang", hs: "haskell", ml: "ocaml", vim: "vim",
  graphql: "graphql", proto: "protobuf", tf: "hcl", hcl: "hcl",
};

/** `getLanguageFromPath(filePath)` (theme.ts:993). */
export function getLanguageFromPath(filePath) {
  const ext = String(filePath ?? "").split(".").pop()?.toLowerCase();
  return ext ? __EXT_TO_LANG[ext] : undefined;
}

/** `highlightCode` (theme.ts:937): no syntax renderer in the headless host,
 *  so lines are returned unwrapped rather than with guessed colours. */
export function highlightCode(code, _lang) {
  return String(code ?? "").split("\n");
}

/** `getMarkdownTheme` (theme.ts:1065): identity styling; the host's terminal
 *  renderer owns colours, so extension renderers keep plain text. */
export function getMarkdownTheme() {
  const identity = (text) => text;
  return {
    heading: identity, link: identity, linkUrl: identity, code: identity,
    codeBlock: identity, codeBlockBorder: identity, quote: identity,
    quoteBorder: identity, hr: identity, listBullet: identity,
    bold: identity, italic: identity, underline: identity,
    strikethrough: identity, highlightCode,
  };
}

/** `InteractiveMode` (interactive-mode.ts:469): only the prototype surface
 *  extensions patch. There is no interactive terminal in the embedded host. */
export class InteractiveMode {
  showLoadedResources(_options) {}
  setToolsExpanded(_expanded) {}
  getStartupExpansionState() {
    return false;
  }
}

/** `SessionManager` (session-manager.ts:988): sessions are Rust-owned. `open`
 *  throws rather than returning a fake session; `listAll` reports the empty
 *  set because the embedded host keeps no on-disk session registry in JS. */
export class SessionManager {
  static listAll() {
    return [];
  }
  static open(_path) {
    throw new Error("SessionManager.open is not available in the embedded extensions host; sessions are owned by the Rust host");
  }
}
"#;

/// Additional pure helpers the real-world sweep found extensions importing
/// from `@earendil-works/pi-coding-agent`. They are pure enough to port
/// faithfully; the tool factories are documented stubs because the embedded
/// host's tool execution is Rust-owned.
pub const PI_CODING_AGENT_HELPERS_PATCH: &str = r#"
// --- real-world helper imports ----------------------------------------------
// Upstream: core/session-manager.ts:355-369 (parseSessionEntries),
// utils/frontmatter.ts:29-45 (parseFrontmatter), core/tools/truncate.ts:11-12,
// :61-154 (DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, formatSize, truncateHead),
// core/tools/file-mutation-queue.ts:32-58 (withFileMutationQueue),
// core/tools/{bash,read,edit,write,grep,find,ls}.ts (createXTool factories).

/** Split a session JSONL file into entries, skipping malformed lines. */
export function parseSessionEntries(content) {
  const entries = [];
  for (const line of String(content ?? "").trim().split("\n")) {
    if (!line.trim()) continue;
    try {
      entries.push(JSON.parse(line));
    } catch {
      // Skip malformed lines, like upstream.
    }
  }
  return entries;
}

/** Minimal frontmatter parser: `---` YAML at the top of a markdown file.
 *  A tiny `key: value` parser covers the string frontmatter the bundled
 *  extensions use; no YAML dependency exists in the embedded host. */
export function parseFrontmatter(content) {
  const normalized = String(content ?? "").replace(/\r\n/g, "\n").replace(/\r/g, "\n").replace(/^\uFEFF/, "");
  if (!normalized.startsWith("---")) return { frontmatter: {}, body: normalized };
  const end = normalized.indexOf("\n---", 3);
  if (end === -1) return { frontmatter: {}, body: normalized };
  const frontmatter = {};
  for (const line of normalized.slice(4, end).split("\n")) {
    const separator = line.indexOf(":");
    if (separator < 0) continue;
    const key = line.slice(0, separator).trim();
    let value = line.slice(separator + 1).trim();
    if (!key) continue;
    if ((value.startsWith('"') && value.endsWith('"')) || (value.startsWith("'") && value.endsWith("'"))) {
      value = value.slice(1, -1);
    }
    frontmatter[key] = value;
  }
  return { frontmatter, body: normalized.slice(end + 4).trim() };
}

export const DEFAULT_MAX_LINES = 2000;
export const DEFAULT_MAX_BYTES = 50 * 1024;

export function formatSize(bytes) {
  if (bytes < 1024) return `${bytes}B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)}KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)}MB`;
}

const __byteLength = (text) => {
  let count = 0;
  for (const character of String(text)) {
    const code = character.codePointAt(0);
    if (code <= 0x7f) count += 1;
    else if (code <= 0x7ff) count += 2;
    else if (code <= 0xffff) count += 3;
    else count += 4;
  }
  return count;
};

/** `truncateHead` (truncate.ts:78): keep the first complete lines that fit. */
export function truncateHead(content, options = {}) {
  const maxLines = options.maxLines ?? DEFAULT_MAX_LINES;
  const maxBytes = options.maxBytes ?? DEFAULT_MAX_BYTES;
  const text = String(content ?? "");
  const allLines = text.length === 0 ? [] : text.split("\n");
  if (text.endsWith("\n")) allLines.pop();
  const totalLines = allLines.length;
  const totalBytes = __byteLength(text);
  const base = {
    truncated: false, truncatedBy: null, totalLines, totalBytes,
    lastLinePartial: false, maxLines, maxBytes,
  };
  if (totalLines <= maxLines && totalBytes <= maxBytes) {
    return { ...base, content: text, outputLines: totalLines, outputBytes: totalBytes, firstLineExceedsLimit: false };
  }
  if (totalLines > 0 && __byteLength(allLines[0]) > maxBytes) {
    return { ...base, content: "", truncated: true, truncatedBy: "bytes",
      outputLines: 0, outputBytes: 0, firstLineExceedsLimit: true };
  }
  const kept = [];
  let bytes = 0;
  let truncatedBy = "lines";
  for (let index = 0; index < allLines.length && index < maxLines; index++) {
    const lineBytes = __byteLength(allLines[index]) + (index > 0 ? 1 : 0);
    if (bytes + lineBytes > maxBytes) {
      truncatedBy = "bytes";
      break;
    }
    kept.push(allLines[index]);
    bytes += lineBytes;
  }
  const output = kept.join("\n");
  return { ...base, content: output, truncated: true, truncatedBy,
    outputLines: kept.length, outputBytes: __byteLength(output), firstLineExceedsLimit: false };
}

/** Serializes per-file mutations. The embedded host has no parallel JS file
 *  writers, so a simple keyed promise chain is faithful enough. */
const __fileMutationQueues = new Map();
export async function withFileMutationQueue(filePath, fn) {
  const previous = __fileMutationQueues.get(filePath) ?? Promise.resolve();
  let release;
  const next = new Promise((resolve) => { release = resolve; });
  __fileMutationQueues.set(filePath, previous.then(() => next));
  await previous;
  try {
    return await fn();
  } finally {
    release();
    if (__fileMutationQueues.get(filePath) === next) __fileMutationQueues.delete(filePath);
  }
}

/** `createBashToolDefinition` (bash.ts:461): a ToolDefinition shape with a
 *  throwing execute, since shell execution is owned by the Rust host. */
const __unavailableTool = (name, label, description, parameters) => {
  const fail = () => {
    throw new Error(`${name} tool execution is not available in the embedded extensions host; tools are owned by the Rust host`);
  };
  return { name, label, description, parameters, execute: fail };
};

const __object = (properties) => ({ type: "object", properties, required: Object.keys(properties) });
const __string = () => ({ type: "string" });

export function createBashToolDefinition(_cwd, _options) {
  return __unavailableTool("bash", "bash", "Execute a bash command", __object({ command: __string() }));
}
export function createBashTool(cwd, options) {
  return createBashToolDefinition(cwd, options);
}
export function createLocalBashOperations(_options) {
  const fail = () => {
    throw new Error("local bash operations are not available in the embedded extensions host; tools are owned by the Rust host");
  };
  return { exec: fail, spawn: fail };
}
export function createReadToolDefinition(_cwd, _options) {
  return __unavailableTool("read", "read", "Read a file", __object({ path: __string() }));
}
export function createReadTool(cwd, options) { return createReadToolDefinition(cwd, options); }
export function createEditToolDefinition(_cwd, _options) {
  return __unavailableTool("edit", "edit", "Edit a file", __object({ path: __string(), oldText: __string(), newText: __string() }));
}
export function createEditTool(cwd, options) { return createEditToolDefinition(cwd, options); }
export function createWriteToolDefinition(_cwd, _options) {
  return __unavailableTool("write", "write", "Write a file", __object({ path: __string(), content: __string() }));
}
export function createWriteTool(cwd, options) { return createWriteToolDefinition(cwd, options); }
export function createGrepToolDefinition(_cwd, _options) {
  return __unavailableTool("grep", "grep", "Search file contents", __object({ pattern: __string() }));
}
export function createGrepTool(cwd, options) { return createGrepToolDefinition(cwd, options); }
export function createFindToolDefinition(_cwd, _options) {
  return __unavailableTool("find", "find", "Find files", __object({ pattern: __string() }));
}
export function createFindTool(cwd, options) { return createFindToolDefinition(cwd, options); }
export function createLsToolDefinition(_cwd, _options) {
  return __unavailableTool("ls", "ls", "List directory", __object({ path: __string() }));
}
export function createLsTool(cwd, options) { return createLsToolDefinition(cwd, options); }

/** `defineTool` (types.ts:711): identity at runtime, preserves inference. */
export function defineTool(tool) {
  return tool;
}

/** `SettingsManager` (settings-manager.ts:272): settings are Rust-owned, so
 *  the shell getters read the host's settings snapshot through `getSettings`. */
export class SettingsManager {
  static create(cwd, agentDir, options = {}) {
    const instance = new SettingsManager();
    instance.cwd = cwd;
    instance.agentDir = agentDir;
    instance.options = options;
    return instance;
  }
  drainErrors() {
    return [];
  }
  getShellPath() {
    return getSettings()?.shell?.path ?? getSettings()?.shellPath ?? undefined;
  }
  getShellCommandPrefix() {
    return getSettings()?.shell?.commandPrefix ?? getSettings()?.shellCommandPrefix ?? undefined;
  }
}

// --- compaction helpers ----------------------------------------------------
// Upstream: core/compaction/compaction.ts:276-500 and
// core/session-manager.ts:439-461. pi-sol-pi's online-context-compact uses
// all three at runtime; the embedded host stores sessions as JSON, so the
// pure projection/estimation functions port directly.

const __ESTIMATED_IMAGE_CHARS = 4800;

function __estimateTextAndImageContentChars(content) {
  if (typeof content === "string") return content.length;
  let chars = 0;
  for (const block of content ?? []) {
    if (block.type === "text" && block.text) chars += block.text.length;
    else if (block.type === "image") chars += __ESTIMATED_IMAGE_CHARS;
  }
  return chars;
}

/** `estimateTokens` (compaction.ts:298): chars/4, conservative overestimate. */
export function estimateTokens(message) {
  let chars = 0;
  switch (message.role) {
    case "system":
      chars = __estimateTextAndImageContentChars(message.content);
      for (const section of Object.values(message.sections ?? {})) {
        if (section) chars += section.length;
      }
      if (message.toolsAdded) chars += JSON.stringify(message.toolsAdded).length;
      return Math.ceil(chars / 4);
    case "user":
      return Math.ceil(__estimateTextAndImageContentChars(message.content) / 4);
    case "assistant":
      for (const block of message.content ?? []) {
        if (block.type === "text") chars += block.text.length;
        else if (block.type === "thinking") chars += block.thinking.length;
        else if (block.type === "toolCall") chars += block.name.length + JSON.stringify(block.arguments).length;
      }
      return Math.ceil(chars / 4);
    case "custom":
    case "toolResult":
      return Math.ceil(__estimateTextAndImageContentChars(message.content) / 4);
    case "bashExecution":
      chars = message.command.length + message.output.length;
      return Math.ceil(chars / 4);
    case "branchSummary":
    case "compactionSummary":
      return Math.ceil(message.summary.length / 4);
  }
  return 0;
}

/** `sessionEntryToContextMessages` (session-manager.ts:439). */
export function sessionEntryToContextMessages(entry) {
  if (entry.type === "message") {
    const message = entry.message;
    if (message.role === "system" && message.content == null) return [{ ...message, content: "" }];
    if (
      (message.role === "user" || message.role === "assistant" || message.role === "toolResult") &&
      message.content == null
    ) {
      return [{ ...message, content: [] }];
    }
    return [message];
  }
  if (entry.type === "custom_message") {
    return [{
      role: "custom",
      customType: entry.customType,
      content: entry.content ?? [],
      display: entry.display,
      details: entry.details,
      timestamp: entry.timestamp,
    }];
  }
  if (entry.type === "branch_summary" && entry.summary) {
    return [{ role: "branchSummary", summary: entry.summary, fromId: entry.fromId, timestamp: entry.timestamp }];
  }
  if (entry.type === "compaction") {
    const summary = {
      role: "compactionSummary",
      summary: entry.summary,
      tokensBefore: entry.tokensBefore,
      timestamp: entry.timestamp,
    };
    return entry.systemMessage ? [entry.systemMessage, summary] : [summary];
  }
  return [];
}

function __isCutPointMessage(message) {
  return ["user", "assistant", "bashExecution", "custom", "branchSummary", "compactionSummary"].includes(message.role);
}

function __isTurnStartMessage(message) {
  return ["user", "bashExecution", "custom", "branchSummary", "compactionSummary"].includes(message.role);
}

function __isTurnStartEntry(entry) {
  if (entry.type === "compaction") return false;
  return sessionEntryToContextMessages(entry).some(__isTurnStartMessage);
}

function __findValidCutPoints(entries, startIndex, endIndex) {
  const points = [];
  for (let i = startIndex; i < endIndex; i++) {
    const entry = entries[i];
    if (entry.type === "compaction") continue;
    if (sessionEntryToContextMessages(entry).some(__isCutPointMessage)) points.push(i);
  }
  return points;
}

function __findTurnStartIndex(entries, entryIndex, startIndex) {
  for (let i = entryIndex; i >= startIndex; i--) {
    if (__isTurnStartEntry(entries[i])) return i;
  }
  return -1;
}

/** `findCutPoint` (compaction.ts:446): newest-first token budget walk. */
export function findCutPoint(entries, startIndex, endIndex, keepRecentTokens) {
  const cutPoints = __findValidCutPoints(entries, startIndex, endIndex);
  if (cutPoints.length === 0) {
    return { firstKeptEntryIndex: startIndex, turnStartIndex: -1, isSplitTurn: false };
  }
  let accumulatedTokens = 0;
  let cutIndex = cutPoints[0];
  for (let i = endIndex - 1; i >= startIndex; i--) {
    const messageTokens = sessionEntryToContextMessages(entries[i]).reduce(
      (sum, message) => sum + estimateTokens(message),
      0,
    );
    if (messageTokens === 0) continue;
    accumulatedTokens += messageTokens;
    if (accumulatedTokens >= keepRecentTokens) {
      cutIndex = cutPoints.find((candidate) => candidate >= i) ?? cutPoints[cutPoints.length - 1];
      break;
    }
  }
  while (cutIndex > startIndex) {
    const previous = entries[cutIndex - 1];
    if (previous.type === "compaction" || sessionEntryToContextMessages(previous).length > 0) break;
    cutIndex--;
  }
  const startsTurn = __isTurnStartEntry(entries[cutIndex]);
  const turnStartIndex = startsTurn ? -1 : __findTurnStartIndex(entries, cutIndex, startIndex);
  return {
    firstKeptEntryIndex: cutIndex,
    turnStartIndex,
    isSplitTurn: !startsTurn && turnStartIndex !== -1,
  };
}
"#;

pub const TYPEBOX_LITERAL_PATCH: &str = r#"
// TypeBox 1.x `Literal` (`Type.Literal(value, options)`): a closed schema with
// a `const` value and a concrete `type`. The E1 shim declared it as an
// unsupported builder; extensions that define literal-union tool schemas need
// the real shape at module-evaluation time.
Type.Literal = (value, options) => {
  const type = value === null ? "null" : Array.isArray(value) ? "array" : typeof value;
  return setHidden({ type, const: value, ...(options ?? {}) }, "~kind", "Literal");
};

// `Integer` / `Tuple` (TypeBox 1.x) are used by tool parameter schemas in the
// real-world extension set; both are simple shapes with no validator behind
// them, so the embedded host can provide them faithfully.
Type.Integer = (options) => setHidden({ type: "integer", ...(options ?? {}) }, "~kind", "Integer");
Type.Tuple = (items, options) => {
  const schema = { type: "array", prefixItems: items, minItems: items.length, maxItems: items.length };
  return setHidden({ ...schema, ...(options ?? {}) }, "~kind", "Tuple");
};
"#;

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

/// The extra named exports for one virtual specifier, if any.
///
/// `@earendil-works/pi-coding-agent` is served by [`crate::node`] (the
/// Rust-owned state surface); its patch still lives here so the node host owns
/// only the base shim. Every alias gets the same patch as its canonical name.
pub fn patch_sources(specifier: &str) -> &'static [&'static str] {
    match specifier {
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
        | "@mariozechner/pi-ai/providers/all" => &[PI_AI_PATCH],
        "@earendil-works/pi-tui" | "@mariozechner/pi-tui" => &[PI_TUI_PATCH],
        "@earendil-works/pi-coding-agent" | "@mariozechner/pi-coding-agent" => {
            &[PI_CODING_AGENT_PATCH, PI_CODING_AGENT_HELPERS_PATCH]
        }
        "typebox" | "@sinclair/typebox" => &[TYPEBOX_LITERAL_PATCH],
        _ => &[],
    }
}

/// The full JavaScript source a virtual module should be declared with:
/// the base source plus the extension-surface patch, when one exists.
///
/// Both the real [`crate::resolver::ExtensionLoader`] and the unit-test loader
/// use this so tests exercise the exact module an importing extension sees.
pub fn composed_source(specifier: &str) -> Option<String> {
    let base = crate::resolver::virtual_module_source(specifier)?;
    let patches = patch_sources(specifier);
    let mut source = String::with_capacity(base.len());
    source.push_str(base);
    for patch in patches {
        source.push('\n');
        source.push_str(patch);
    }
    Some(source)
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
            if crate::resolver::virtual_module_source(name).is_some() {
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
            let module_source = composed_source(specifier).ok_or(rquickjs::Error::Unknown)?;
            Module::declare(ctx.clone(), name, module_source)
        }
    }

    /// Evaluates one TypeScript fixture in a standalone `QuickJS` runtime whose
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

    #[test]
    fn composed_source_appends_patches_to_the_right_base_modules() {
        let tui = composed_source("@earendil-works/pi-tui").expect("tui source");
        assert!(tui.starts_with(source("@earendil-works/pi-tui").expect("base")));
        assert!(tui.contains("export function matchesKey"));
        assert!(tui.contains("export class Editor"));
        assert!(tui.contains("export class MouseRegion"));

        let coding_agent =
            composed_source("@earendil-works/pi-coding-agent").expect("agent source");
        assert!(coding_agent.contains("export function convertToLlm"));
        assert!(coding_agent.contains("export function parseSessionEntries"));
        assert!(coding_agent.contains("export class SessionManager"));

        let ai = composed_source("@earendil-works/pi-ai/providers/all").expect("ai source");
        assert!(ai.contains("export function builtinProviders"));
        let typebox = composed_source("typebox").expect("typebox source");
        assert!(typebox.contains("Type.Literal ="));
        assert!(
            composed_source("@earendil-works/pi-mcp")
                .expect("mcp")
                .contains("createMcpToolName")
        );
        assert!(
            !composed_source("@earendil-works/pi-mcp")
                .expect("mcp")
                .contains("matchesKey")
        );
    }

    #[test]
    fn pi_tui_real_world_exports_resolve() {
        let result = evaluate(
            r#"
            import {
                Editor, Key, MouseRegion, matchesKey, stripTerminalSequences,
                wrapTextWithAnsi,
            } from "@earendil-works/pi-tui";
            const editor = new Editor(undefined, {});
            editor.setText("custom");
            editor.handleInput("!");
            const submit = [];
            editor.onSubmit = (value) => submit.push(value);
            editor.handleInput("\r");
            globalThis.__result = JSON.stringify({
                editor: typeof Editor,
                text: editor.getText(),
                key: Key.shift("tab"),
                enter: matchesKey("\r", Key.enter),
                ctrlP: matchesKey("\u0010", "ctrl+p"),
                noMatch: matchesKey("x", "ctrl+p"),
                mouse: typeof MouseRegion,
                stripped: stripTerminalSequences("\u001b[31mred\u001b[0m"),
                wrapped: wrapTextWithAnsi("hello world", 5),
            });
            "#,
        );
        assert_eq!(result["editor"], "function");
        assert_eq!(result["text"], "");
        assert_eq!(result["key"], "shift+tab");
        assert_eq!(result["enter"], true);
        assert_eq!(result["ctrlP"], true);
        assert_eq!(result["noMatch"], false);
        assert_eq!(result["mouse"], "function");
        assert_eq!(result["stripped"], "red");
        assert_eq!(result["wrapped"], json!(["hello", "world"]));
    }

    #[test]
    fn pi_ai_event_stream_and_compat_exports_resolve() {
        let result = evaluate(
            r#"
            import {
                createAssistantMessageEventStream, getCurrentSystemPrompt,
            } from "@earendil-works/pi-ai";
            import { openAIResponsesApi } from "@earendil-works/pi-ai/compat";
            import { builtinProviders } from "@earendil-works/pi-ai/providers/all";
            const stream = createAssistantMessageEventStream();
            stream.push({ type: "text", text: "hi" });
            stream.push({ type: "done", message: { role: "assistant", content: [] } });
            let threw = false;
            try { openAIResponsesApi().streamSimple({}, {}, {}); } catch (error) { threw = /not available/.test(error.message); }
            globalThis.__result = JSON.stringify({
                done: stream.done,
                hasResult: typeof stream.result().then === "function",
                prompt: getCurrentSystemPrompt([{ role: "system", content: "base" }]),
                providers: builtinProviders().length,
                threw,
            });
            "#,
        );
        assert_eq!(result["done"], true);
        assert_eq!(result["hasResult"], true);
        assert_eq!(result["prompt"], "base");
        assert_eq!(result["providers"], 0);
        assert_eq!(result["threw"], true);
    }

    #[test]
    fn pi_coding_agent_real_world_exports_resolve() {
        let result = evaluate(
            r#"
            import {
                CONFIG_DIR_NAME, InteractiveMode, SessionManager,
                convertToLlm, createBashTool, createLocalBashOperations,
                defineTool, getAgentDir, getLanguageFromPath, getMarkdownTheme,
                isToolCallEventType, parseFrontmatter, parseSessionEntries,
                truncateHead, withFileMutationQueue,
            } from "@earendil-works/pi-coding-agent";
            const converted = convertToLlm([
                { role: "custom", content: "x" },
                { role: "bashExecution", command: "echo hi", output: "hi" },
                { role: "assistant", content: [] },
                { role: "unknown" },
            ]);
            const trunc = truncateHead("a\nb\nc", { maxLines: 2 });
            globalThis.__result = JSON.stringify({
                configDir: CONFIG_DIR_NAME,
                agentDir: getAgentDir(),
                interactive: typeof InteractiveMode,
                session: typeof SessionManager,
                listAll: SessionManager.listAll().length,
                roles: converted.map((message) => message.role),
                bashText: converted[1].content[0].text,
                tool: createBashTool("/tmp").name,
                ops: typeof createLocalBashOperations().exec,
                identity: defineTool({ name: "x" }).name,
                lang: getLanguageFromPath("a.ts"),
                theme: typeof getMarkdownTheme().heading,
                toolEvent: isToolCallEventType("bash", { toolName: "bash" }),
                frontmatter: parseFrontmatter("---\nname: hi\n---\nbody").frontmatter.name,
                entries: parseSessionEntries('{"type":"session"}\nbroken\n').length,
                trunc: trunc.content,
                queue: typeof withFileMutationQueue,
            });
            "#,
        );
        assert_eq!(result["configDir"], "pi-rs");
        assert!(
            result["agentDir"]
                .as_str()
                .expect("path")
                .ends_with("pi-rs")
        );
        assert_eq!(result["interactive"], "function");
        assert_eq!(result["session"], "function");
        assert_eq!(result["listAll"], 0);
        assert_eq!(result["roles"], json!(["user", "user", "assistant"]));
        assert!(
            result["bashText"]
                .as_str()
                .expect("text")
                .contains("echo hi")
        );
        assert_eq!(result["tool"], "bash");
        assert_eq!(result["ops"], "function");
        assert_eq!(result["identity"], "x");
        assert_eq!(result["lang"], "typescript");
        assert_eq!(result["theme"], "function");
        assert_eq!(result["toolEvent"], true);
        assert_eq!(result["frontmatter"], "hi");
        assert_eq!(result["entries"], 1);
        assert_eq!(result["trunc"], "a\nb");
        assert_eq!(result["queue"], "function");
    }

    #[test]
    fn typebox_literal_integer_and_tuple_builders_resolve() {
        let result = evaluate(
            r#"
            import { Type } from "typebox";
            const literal = Type.Literal("bar", { description: "d" });
            const integer = Type.Integer({ minimum: 1 });
            const tuple = Type.Tuple([Type.String(), Type.Number()]);
            globalThis.__result = JSON.stringify({
                literal, literalKind: literal["~kind"],
                integer, integerKind: integer["~kind"],
                tuple, tupleKind: tuple["~kind"],
            });
            "#,
        );
        assert_eq!(result["literal"]["type"], "string");
        assert_eq!(result["literal"]["const"], "bar");
        assert_eq!(result["literal"]["description"], "d");
        assert_eq!(result["literalKind"], "Literal");
        assert_eq!(result["integer"]["type"], "integer");
        assert_eq!(result["integer"]["minimum"], 1);
        assert_eq!(result["integerKind"], "Integer");
        assert_eq!(result["tuple"]["type"], "array");
        assert_eq!(result["tuple"]["minItems"], 2);
        assert_eq!(result["tuple"]["maxItems"], 2);
        assert_eq!(result["tupleKind"], "Tuple");
    }
}
