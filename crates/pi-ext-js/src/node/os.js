// `node:os` — the host-backed calls used by the bundled extensions
// (`homedir` in `extensions/mcp/runtime.ts:7` and
// `extensions/llama/huggingface.ts:2`). Values come from the `nodeOs` op.

const ops = globalThis.__piOps;

function value(operation) {
  return JSON.parse(ops.nodeOs(operation));
}

export function homedir() {
  return value("homedir");
}

export function tmpdir() {
  return value("tmpdir");
}

export function platform() {
  return value("platform");
}

export function arch() {
  return value("arch");
}

export function eol() {
  return value("eol");
}

export function type() {
  return value("type");
}

export function release() {
  return value("release");
}

export function hostname() {
  return value("hostname");
}

export function cpus() {
  return value("cpus");
}

export function availableParallelism() {
  return Number(value("availableParallelism"));
}

export const constants = {
  UV_UDP_REUSEADDR: 1,
  signals: {},
};

export const devNull = "/dev/null";

export default {
  homedir,
  tmpdir,
  platform,
  arch,
  eol,
  type,
  release,
  hostname,
  cpus,
  availableParallelism,
  constants,
  devNull,
};
