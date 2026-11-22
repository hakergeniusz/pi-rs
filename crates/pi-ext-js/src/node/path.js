// `node:path` — lexical POSIX path manipulation backed by the `nodePath`
// host op. Upstream imports it in the bundled MCP extension
// (`extensions/mcp/runtime.ts:8`, `extensions/mcp/index.ts:29`), tool-search
// (`extensions/codemode/tool.ts:24`) and the llama provider
// (`extensions/llama/huggingface.ts:3`). The port runs on POSIX; `win32`
// exposes the same lexical operations (recorded in `WIRING.md`).

const ops = globalThis.__piOps;

function call(operation, args) {
  return JSON.parse(ops.nodePath(operation, JSON.stringify(args.map(String))));
}

export function join(...parts) {
  return call("join", parts);
}

export function resolve(...parts) {
  return call("resolve", parts);
}

export function normalize(path) {
  return call("normalize", [path]);
}

export function isAbsolute(path) {
  return call("isAbsolute", [path]) === true;
}

export function basename(path, suffix) {
  return suffix === undefined ? call("basename", [path]) : call("basename", [path, suffix]);
}

export function dirname(path) {
  return call("dirname", [path]);
}

export function extname(path) {
  return call("extname", [path]);
}

export function relative(from, to) {
  return call("relative", [from, to]);
}

export function parse(path) {
  return call("parse", [path]);
}

export function format(object) {
  const base = object.base ?? `${object.name ?? ""}${object.ext ?? ""}`;
  const dir = object.dir ?? "";
  if (!dir) return base;
  if (dir === "/") return `/${base}`;
  return `${dir}/${base}`;
}

export function toNamespacedPath(path) {
  return path;
}

export const sep = "/";
export const delimiter = ":";

// Upstream extensions may reach for `path.posix` explicitly. `win32` keeps
// the same shape but the port's lexical semantics are POSIX-only.
const surface = {
  join,
  resolve,
  normalize,
  isAbsolute,
  basename,
  dirname,
  extname,
  relative,
  parse,
  format,
  toNamespacedPath,
  sep,
  delimiter,
};

export const posix = surface;
export const win32 = {
  ...surface,
  sep: "\\",
  delimiter: ";",
};

export default { ...surface, posix, win32 };
