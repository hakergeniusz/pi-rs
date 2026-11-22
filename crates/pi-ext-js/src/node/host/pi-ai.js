// `@earendil-works/pi-ai` virtual module — the surface bundled extensions
// import at runtime. Upstream serves `pi-ai/compat` plus TypeBox's `Type`,
// `Static` and `TSchema` through `pi-ai-extension-view.ts:1-28`; this shim
// follows that shape:
//
//   `@earendil-works/pi-ai`                 -> extension view (compat + Type)
//   `@earendil-works/pi-ai/compat`           -> same superset
//   `@earendil-works/pi-ai/models`           -> same (model helpers)
//   `@earendil-works/pi-ai/oauth`            -> same
//   `@earendil-works/pi-ai/providers/all`    -> same (catalog is Rust-owned)
//   `@earendil-works/pi-ai/utils/model-operations` -> same
//   `@earendil-works/pi-ai/utils/oauth-page` -> same (oauth HTML helpers)
//
// Ground truth for the ported helpers:
//   `packages/ai/src/models.ts:1304-1345`     (thinking levels, modelsAreEqual)
//   `packages/ai/src/utils/text.ts:6-22`      (contentText, getSystemMessageText)
//   `packages/ai/src/utils/transcript.ts:26-95` (normalizeContext, replay helpers)
//   `packages/ai/src/utils/retry.ts:149-153,273-278`
//   `packages/ai/src/utils/model-operations.ts:55-83`
//   `packages/ai/src/utils/oauth-page.ts:94-112`
//   `packages/ai/src/utils/identity.ts:75-83`
//   `packages/ai/src/utils/uuid.ts:8-40`
//   `packages/ai/src/providers/radius-config.ts:5,53-...`
//
// DEVIATION: the composed model/provider catalog and the streaming functions
// are Rust-owned (HostCore + the provider registry). `getModel()` reads the
// current model from HostCore through the existing `getModel` op; the
// `stream`/`streamSimple`/`completeSimple` entry points throw a documented
// error instead of performing a live model call. Type-only exports (`Static`,
// `TSchema`, `Model`, `Message`, …) are erased by upstream's transpiler and
// are intentionally absent at runtime.

import { Type } from "typebox";

const ops = globalThis.__piOps;

// ---------------------------------------------------------------------------
// Model catalog — the one piece that crosses from HostCore.
// ---------------------------------------------------------------------------

/** The current model the host is using (`HostCore.actions.get_model`). */
export function getModel() {
  if (!ops || typeof ops.getModel !== "function") return undefined;
  const raw = ops.getModel();
  if (!raw) return undefined;
  try {
    return JSON.parse(raw);
  } catch {
    return undefined;
  }
}

/** Providers registered through `pi.registerProvider` are Rust-side; no JS view. */
export function getApiProvider(_api) {
  return undefined;
}
export function getApiProviders() {
  return [];
}

// ---------------------------------------------------------------------------
// Model helpers (ported verbatim from `models.ts`).
// ---------------------------------------------------------------------------

const EXTENDED_THINKING_LEVELS = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

export function getSupportedThinkingLevels(model) {
  if (!model || !model.reasoning) return ["off"];
  return EXTENDED_THINKING_LEVELS.filter((level) => {
    const mapped = model.thinkingLevelMap?.[level];
    if (mapped === null) return false;
    if (level === "xhigh" || level === "max") return mapped !== undefined;
    return true;
  });
}

export function clampThinkingLevel(model, level) {
  const available = getSupportedThinkingLevels(model);
  if (available.includes(level)) return level;
  const requested = EXTENDED_THINKING_LEVELS.indexOf(level);
  if (requested === -1) return available[0] ?? "off";
  for (let i = requested; i < EXTENDED_THINKING_LEVELS.length; i++) {
    if (available.includes(EXTENDED_THINKING_LEVELS[i])) return EXTENDED_THINKING_LEVELS[i];
  }
  for (let i = requested - 1; i >= 0; i--) {
    if (available.includes(EXTENDED_THINKING_LEVELS[i])) return EXTENDED_THINKING_LEVELS[i];
  }
  return available[0] ?? "off";
}

const modelType = (model) => model?.type ?? model?.modelType ?? "language";

export function modelsAreEqual(a, b) {
  if (!a || !b) return false;
  return modelType(a) === modelType(b) && a.id === b.id && a.provider === b.provider;
}
export const modelsEqual = modelsAreEqual;

// ---------------------------------------------------------------------------
// Text / message helpers (`text.ts`, `transcript.ts`).
// ---------------------------------------------------------------------------

export function contentText(content, separator = "\n") {
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  return content
    .filter((block) => block && block.type === "text")
    .map((block) => block.text)
    .join(separator);
}

export function getSystemMessageText(message) {
  if (!message) return "";
  const parts = [contentText(message.content)];
  for (const text of Object.values(message.sections ?? {})) {
    if (text !== null && text !== undefined) parts.push(text);
  }
  return parts.filter((part) => part.length > 0).join("\n\n");
}

export function normalizeContext(context) {
  if (!context) return { messages: [] };
  const initial = createInitialSystemMessage(context.systemPrompt, context.tools);
  return { messages: initial ? [initial, ...(context.messages ?? [])] : [...(context.messages ?? [])] };
}

function createInitialSystemMessage(systemPrompt, tools) {
  if (!systemPrompt && !(tools && tools.length)) return undefined;
  const content = typeof systemPrompt === "string" ? systemPrompt : contentText(systemPrompt);
  return { role: "system", content, timestamp: Date.now(), toolsAdded: tools ?? [] };
}

export function getCurrentTools(messages) {
  const tools = new Map();
  for (const message of messages ?? []) {
    if (!message || message.role !== "system") continue;
    for (const tool of message.toolsRemoved ?? []) tools.delete(tool.name);
    for (const tool of message.toolsAdded ?? []) tools.set(tool.name, tool);
  }
  return [...tools.values()];
}

export function getCurrentSystemMessage(messages) {
  const content = [];
  const sections = new Map();
  let timestamp;
  for (const message of messages ?? []) {
    if (!message || message.role !== "system") continue;
    timestamp ??= message.timestamp;
    const text = contentText(message.content);
    if (text.length > 0) content.push(text);
    for (const [name, value] of Object.entries(message.sections ?? {})) {
      if (value === null) sections.delete(name);
      else sections.set(name, value);
    }
  }
  if (content.length === 0) return undefined;
  const sectionsObject = {};
  for (const [name, value] of sections) sectionsObject[name] = value;
  return {
    role: "system",
    content: content.join("\n"),
    sections: sectionsObject,
    timestamp,
    toolsAdded: getCurrentTools(messages ?? []),
  };
}

export function withoutInitialSystemMessage(messages) {
  const first = messages?.[0];
  return first && first.role === "system" ? messages.slice(1) : [...(messages ?? [])];
}

// ---------------------------------------------------------------------------
// Retry / error helpers (`retry.ts`, `overflow.ts`).
// ---------------------------------------------------------------------------

export const DEFAULT_MAX_AGENT_RETRY_DELAY_MS = 60000;

export function retryDelayMs(policy, attempt) {
  const delay = (policy?.baseDelayMs ?? 0) * 2 ** Math.max(0, attempt - 1);
  const safe = Number.isSafeInteger(delay) ? delay : Number.MAX_SAFE_INTEGER;
  return Math.min(safe, policy?.maxAgentDelayMs ?? DEFAULT_MAX_AGENT_RETRY_DELAY_MS);
}

const NON_RETRYABLE_LIMIT = /quota|credit|billing|insufficient|invalid (request|api key)|unauthorized|forbidden/i;
const RETRYABLE_ERROR = /timeout|timed out|rate.?limit|overload|unavailable|connection|socket|network|500|502|503|529/i;

export function isRetryableAssistantError(message) {
  if (!message || message.stopReason !== "error" || !message.errorMessage) return false;
  if (NON_RETRYABLE_LIMIT.test(message.errorMessage)) return false;
  return RETRYABLE_ERROR.test(message.errorMessage);
}

export function isContextOverflow(message, contextWindow) {
  if (!message || message.stopReason !== "error" || !message.errorMessage) return false;
  if (/context|token|length|too long/i.test(message.errorMessage)) return true;
  const usage = message.usage;
  return !!contextWindow && !!usage && usage.totalTokens >= contextWindow;
}

export class ModelsError extends Error {
  constructor(scope, message) {
    super(typeof message === "string" ? message : scope);
    this.name = "ModelsError";
    this.scope = typeof message === "string" ? scope : undefined;
  }
}

// ---------------------------------------------------------------------------
// `utils/model-operations` error results.
// ---------------------------------------------------------------------------

export function imageErrorResult(model, error, aborted = false) {
  return {
    api: model?.api,
    provider: model?.provider,
    model: model?.id,
    output: [],
    stopReason: aborted ? "aborted" : "error",
    errorMessage: error instanceof Error ? error.message : String(error),
    timestamp: Date.now(),
  };
}

export function classifierErrorResult(model, error, aborted = false) {
  return {
    api: model?.api,
    provider: model?.provider,
    model: model?.id,
    answers: {},
    stopReason: aborted ? "aborted" : "error",
    errorMessage: error instanceof Error ? error.message : String(error),
    timestamp: Date.now(),
  };
}

// ---------------------------------------------------------------------------
// `utils/oauth-page` — the bundled MCP extension renders these on a local
// callback server. The markup is a faithful minimal page (same titles and
// message text as upstream, not byte-identical CSS).
// ---------------------------------------------------------------------------

function renderPage(title, heading, message, details) {
  const escape = (value) =>
    String(value ?? "")
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;");
  const detail = details
    ? `<details><summary>Details</summary><pre>${escape(details)}</pre></details>`
    : "";
  return `<!doctype html><html><head><meta charset="utf-8"><title>${escape(title)}</title></head>` +
    `<body><h1>${escape(heading)}</h1><p>${escape(message)}</p>${detail}</body></html>`;
}

export function oauthSuccessHtml(message) {
  return renderPage("Authentication successful", "Authentication successful", message);
}

export function oauthErrorHtml(message, details) {
  return renderPage("Authentication failed", "Authentication failed", message, details);
}

// ---------------------------------------------------------------------------
// Identity / constants.
// ---------------------------------------------------------------------------

export const PI_ATTRIBUTION = { name: "pi", url: "https://pi.dev", categories: ["cli-agent"] };
export const DEFAULT_RADIUS_GATEWAY = "https://radius.pi.dev";

export function normalizeRadiusGatewayUrl(value) {
  let url = String(value ?? DEFAULT_RADIUS_GATEWAY).trim().replace(/\/+$/, "");
  if (!/^https?:\/\//i.test(url)) url = `https://${url}`;
  return url.replace(/^http:\/\//i, "https://");
}

export function formatUserAgent(identity) {
  const mode = identity?.userAgent ?? "detailed";
  if (typeof mode === "object") return mode.custom;
  if (mode === "short") return "pi";
  return `pi/1.5.2 (browser)`;
}

/** UUIDv7-shaped id. Uses Math.random — QuickJS has no WebCrypto here. */
export function uuidv7(timestampMs) {
  const requested = timestampMs ?? Date.now();
  if (!Number.isInteger(requested) || requested < 0 || requested > 0xffffffffffff) {
    throw new RangeError("UUIDv7 timestamp out of range");
  }
  const bytes = new Uint8Array(16);
  for (let i = 0; i < 16; i++) bytes[i] = Math.floor(Math.random() * 256);
  for (let i = 5; i >= 0; i--) bytes[i] = Math.floor(requested / 256 ** (5 - i)) & 0xff;
  bytes[6] = (bytes[6] & 0x0f) | 0x70;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;
  const hex = [...bytes].map((b) => b.toString(16).padStart(2, "0"));
  return `${hex.slice(0, 4).join("")}-${hex.slice(4, 6).join("")}-${hex.slice(6, 8).join("")}-${hex.slice(8, 10).join("")}-${hex.slice(10).join("")}`;
}

// ---------------------------------------------------------------------------
// Provider catalog / streaming — Rust-owned, documented stubs.
// ---------------------------------------------------------------------------

/** Providers/all is a catalog built in Rust; JS sees only an empty shape. */
export const providers = {};
export const models = { getModel, getSupportedThinkingLevels, clampThinkingLevel, modelsAreEqual };

export function createModels() {
  return {};
}

export function personalizeOpenRouterCatalog(catalog) {
  return catalog;
}

export function resetApiProviders() {}

function unavailable(name) {
  return () => {
    throw new Error(`${name} is not available in the embedded extensions host; model calls are owned by the Rust provider registry`);
  };
}

export const stream = unavailable("stream");
export const streamSimple = unavailable("streamSimple");
export const completeSimple = unavailable("completeSimple");
export const lazyStream = unavailable("lazyStream");

// ---------------------------------------------------------------------------
// TypeBox re-exports from `pi-ai-extension-view.ts:8-28`.
// `Static` / `TSchema` are type-only upstream and have no runtime binding.
// ---------------------------------------------------------------------------

export { Type };

export function StringEnum(values, options) {
  return Type.Unsafe({
    type: "string",
    enum: values,
    ...(options?.description ? { description: options.description } : {}),
    ...(options?.default ? { default: options.default } : {}),
  });
}

export default {
  VERSION: "1.5.2",
  getModel,
  getApiProvider,
  getApiProviders,
  getSupportedThinkingLevels,
  clampThinkingLevel,
  modelsAreEqual,
  modelsEqual,
  contentText,
  getSystemMessageText,
  normalizeContext,
  getCurrentTools,
  getCurrentSystemMessage,
  withoutInitialSystemMessage,
  DEFAULT_MAX_AGENT_RETRY_DELAY_MS,
  retryDelayMs,
  isRetryableAssistantError,
  isContextOverflow,
  ModelsError,
  imageErrorResult,
  classifierErrorResult,
  oauthSuccessHtml,
  oauthErrorHtml,
  PI_ATTRIBUTION,
  DEFAULT_RADIUS_GATEWAY,
  normalizeRadiusGatewayUrl,
  formatUserAgent,
  uuidv7,
  providers,
  models,
  createModels,
  personalizeOpenRouterCatalog,
  resetApiProviders,
  stream,
  streamSimple,
  completeSimple,
  lazyStream,
  Type,
  StringEnum,
};
