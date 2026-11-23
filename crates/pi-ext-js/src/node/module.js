// `node:module` — the `createRequire` entry point upstream extensions use
// (`pi-gemini-router/index.ts:55,339`) when they need a synchronous require
// of a specifier that is already available to the ESM loader.
//
// The require is backed by the same resolver/loader pair that serves ESM
// imports (`resolver.rs`): virtual `node:*` modules, host packages, relative
// files and ancestor `node_modules` all resolve identically. The Rust op
// evaluates the resolved module synchronously and returns its namespace, so
// no second module registry is introduced. Unsupported specifiers throw the
// same E1 error as an `import`.

const ops = globalThis.__piOps;

function baseOf(from) {
  // `import.meta.url` is undefined in the embedded engine; the Rust op
  // reports the calling module's name so relative requires still work.
  let base = String(from ?? ops.nodeCurrentModule());
  if (base.startsWith("file://")) base = base.slice(7);
  if (base === "undefined" || base === "null") base = ops.nodeCurrentModule();
  base = base.split("?")[0].split("#")[0];
  if (base.endsWith("/")) base += "index.js";
  return base;
}

export function createRequire(from) {
  const base = baseOf(from);
  function nodeRequire(specifier) {
    return ops.nodeRequire(base, String(specifier));
  }
  nodeRequire.resolve = (specifier) => ops.nodeRequireResolve(base, String(specifier));
  nodeRequire.cache = {};
  nodeRequire.main = undefined;
  nodeRequire.extensions = {};
  return nodeRequire;
}

export class Module {
  static createRequire = createRequire;
  constructor(id, parent) {
    this.id = id;
    this.parent = parent;
    this.exports = {};
    this.filename = id;
    this.loaded = false;
  }
  static _load(id, parent) {
    return createRequire(parent?.filename ?? "index.js")(id);
  }
}

export const builtinModules = [
  "buffer",
  "child_process",
  "crypto",
  "fs",
  "fs/promises",
  "module",
  "os",
  "path",
  "process",
  "url",
  "util",
];

export default { createRequire, Module, builtinModules };
