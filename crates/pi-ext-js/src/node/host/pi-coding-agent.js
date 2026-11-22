// `@earendil-works/pi-coding-agent` virtual module — the in-process surface
// the port can genuinely provide from HostCore: settings, session metadata
// and tools. Upstream bundles the whole package
// (`core/extensions/virtual-modules.ts:11,33`); the port's session, tools and
// settings managers are Rust-side, so only their state crosses this boundary.
// (`@mariozechner/pi-coding-agent` is an alias of the same surface.)

const ops = globalThis.__piOps;

function read() {
  return JSON.parse(ops.getSessionMetadata());
}

export const VERSION = "1.5.2";

export function getSettings() {
  return JSON.parse(ops.getSettings());
}

export function getSessionMetadata() {
  return read();
}

export function getSessionName() {
  return read().name ?? ops.getSessionName();
}

export function getCwd() {
  return ops.cwd();
}

export function getMode() {
  return ops.mode();
}

export function getThinkingLevel() {
  return ops.getThinkingLevel() || undefined;
}

export function getAllTools() {
  return JSON.parse(ops.getAllTools());
}

export function getActiveTools() {
  return JSON.parse(ops.getActiveTools());
}

export function setActiveTools(names) {
  ops.setActiveTools(JSON.stringify(names));
}

export function getCallableTools() {
  return JSON.parse(ops.getCallableTools());
}

// The classes below are intentionally absent: instantiating a Rust-owned
// SessionManager/SettingsManager from JS would create a second, divergent
// source of truth. Extensions that need them receive the live instances via
// the `pi` API object instead.

export default {
  VERSION,
  getSettings,
  getSessionMetadata,
  getSessionName,
  getCwd,
  getMode,
  getThinkingLevel,
  getAllTools,
  getActiveTools,
  setActiveTools,
  getCallableTools,
};
