// Global `process` bootstrap for the embedded extensions host.
//
// This is a plain script (not a module) evaluated once when `JsHost` is
// created, so an extension can reference `process` at module top level
// without importing `node:process` first. Upstream extensions run inside a
// real Node process; here `HostCore` is the only owner of cwd and the host
// op table is the only owner of the environment, so the object below is a
// faithful *shape* with deliberately inert lifecycle methods:
//
// - `cwd()` reads HostCore (the session cwd), not the OS cwd.
// - `env` is a snapshot from `nodeProcessEnv`; credential-shaped keys are
//   filtered there. JS-side writes mutate only this snapshot.
// - `exit()` records `exitCode` but never terminates the host.
// - `on`/`once`/`kill` record/listeners only; no signals are delivered.
//
// `node:process` re-exports this same object, so `import process from
// "node:process" === globalThis.process` holds.

const ops = globalThis.__piOps;

function value(operation) {
  return JSON.parse(ops.nodeProcess(operation));
}

const listeners = new Map();

const processObject = {
  env: JSON.parse(ops.nodeProcessEnv()),
  platform: value("platform"),
  arch: value("arch"),
  version: value("version"),
  versions: { node: value("version").replace(/^v/, "") },
  title: value("title"),
  argv: value("argv"),
  execArgv: [],
  pid: 1,
  ppid: 0,
  execPath: "node",
  exitCode: 0,
  release: { name: "node" },
  config: {},

  cwd() {
    return value("cwd");
  },

  chdir(path) {
    throw new Error(`process.chdir('${String(path)}') is not available in the extensions host`);
  },

  nextTick(callback, ...args) {
    Promise.resolve().then(() => callback(...args));
  },

  uptime() {
    return 0;
  },

  memoryUsage() {
    return { rss: 0, heapTotal: 0, heapUsed: 0, external: 0, arrayBuffers: 0 };
  },

  stdout: {
    write(text) {
      ops.print("log", String(text));
      return true;
    },
    isTTY: false,
    columns: 80,
    rows: 24,
  },

  stderr: {
    write(text) {
      ops.print("error", String(text));
      return true;
    },
    isTTY: false,
    columns: 80,
    rows: 24,
  },

  stdin: {
    on() {
      return this;
    },
    once() {
      return this;
    },
    removeListener() {
      return this;
    },
    read() {
      return null;
    },
    resume() {
      return this;
    },
    pause() {
      return this;
    },
    isTTY: false,
  },

  // Embedded-host deviation: there is no process to terminate. Recording
  // the code keeps `process.exitCode` observable without tearing down the
  // session (the one-shot host owns teardown).
  exit(code) {
    if (code !== undefined) processObject.exitCode = code;
  },

  on(event, callback) {
    const list = listeners.get(String(event)) ?? [];
    list.push(callback);
    listeners.set(String(event), list);
    return this;
  },

  once(event, callback) {
    const wrapped = (...args) => {
      this.removeListener(event, wrapped);
      callback(...args);
    };
    return this.on(event, wrapped);
  },

  removeListener(event, callback) {
    listeners.set(
      String(event),
      (listeners.get(String(event)) ?? []).filter((item) => item !== callback),
    );
    return this;
  },

  removeAllListeners(event) {
    if (event === undefined) listeners.clear();
    else listeners.delete(String(event));
    return this;
  },

  listeners(event) {
    return [...(listeners.get(String(event)) ?? [])];
  },

  listenerCount(event) {
    return (listeners.get(String(event)) ?? []).length;
  },

  emit(event, ...args) {
    for (const callback of [...(listeners.get(String(event)) ?? [])]) callback(...args);
    return this.listenerCount(event) > 0;
  },

  // Signal delivery would escape the host's lifecycle, so `kill` is a
  // no-op that reports success (used by stale-lock probes in extensions).
  kill(_pid, _signal) {
    return true;
  },

  send() {
    return false;
  },

  disconnect() {},

  hrtime(previous) {
    const nanos = typeof BigInt === "function"
      ? BigInt(Date.now()) * 1000000n
      : Date.now() * 1000000;
    if (previous === undefined) return nanos;
    return nanos - previous;
  },
};

globalThis.process = processObject;

// Node's other ubiquitous global that extensions use at load time without
// an import (`pi-guardrails/index.ts:166`). QuickJS ships no structured
// clone, so mirror the cloneable subset: primitives, plain objects/arrays,
// Date, RegExp, Map, Set, ArrayBuffer and typed arrays. Cycles are kept by
// identity via `seen`. Functions/symbols throw a DataCloneError-shaped error
// like Node. This is a structural clone of host-local values only; it never
// crosses a process boundary.
function structuredCloneValue(value, seen = new Map()) {
  if (value === null || typeof value !== "object") {
    if (typeof value === "symbol") {
      const error = new Error("Could not be cloned.");
      error.name = "DataCloneError";
      throw error;
    }
    return value;
  }
  if (seen.has(value)) return seen.get(value);
  if (value instanceof Date) return new Date(value.getTime());
  if (value instanceof RegExp) return new RegExp(value.source, value.flags);
  if (value instanceof Map) {
    const out = new Map();
    seen.set(value, out);
    for (const [key, item] of value) out.set(structuredCloneValue(key, seen), structuredCloneValue(item, seen));
    return out;
  }
  if (value instanceof Set) {
    const out = new Set();
    seen.set(value, out);
    for (const item of value) out.add(structuredCloneValue(item, seen));
    return out;
  }
  if (value instanceof ArrayBuffer) {
    const out = new ArrayBuffer(value.byteLength);
    new Uint8Array(out).set(new Uint8Array(value));
    seen.set(value, out);
    return out;
  }
  if (ArrayBuffer.isView(value)) {
    const Ctor = value.constructor;
    const out = new Ctor(structuredCloneValue(value.buffer, seen), value.byteOffset, value.length);
    seen.set(value, out);
    return out;
  }
  const out = Array.isArray(value) ? [] : Object.create(Object.getPrototypeOf(value));
  seen.set(value, out);
  for (const key of Object.keys(value)) out[key] = structuredCloneValue(value[key], seen);
  return out;
}

if (globalThis.structuredClone === undefined) globalThis.structuredClone = structuredCloneValue;

// The GLUE bootstrap installs only `Intl.Segmenter`; extensions also use
// `Intl.NumberFormat` at module top level (pi-sol-pi/src/sol-pi/tui.ts:14).
// QuickJS-NG ships no ICU, so provide the decimal/percent/currency subset the
// real-world sweep needs: grouping, digit limits and `resolvedOptions`.
// A future ICU-backed host can replace this without touching extensions.
if (globalThis.Intl === undefined) globalThis.Intl = {};
if (globalThis.Intl.NumberFormat === undefined) {
  globalThis.Intl.NumberFormat = class NumberFormat {
    constructor(locales, options = {}) {
      const locale = typeof locales === "string"
        ? locales
        : Array.isArray(locales) ? locales[0] : undefined;
      this.locale = locale || "en-US";
      this.options = options;
      this.style = options.style ?? "decimal";
      this.currency = options.currency;
      this.maximumFractionDigits = options.maximumFractionDigits
        ?? (this.style === "currency" ? 2 : 3);
      this.minimumFractionDigits = options.minimumFractionDigits ?? 0;
    }

    format(value) {
      let number = Number(value);
      if (!Number.isFinite(number)) number = 0;
      if (this.style === "percent") number *= 100;
      const digits = Math.max(0, Math.min(20,
        Math.max(this.minimumFractionDigits, this.maximumFractionDigits)));
      const [integer, fraction] = number.toFixed(digits).split(".");
      const grouped = integer.replace(/\B(?=(\d{3})+(?!\d))/g, ",");
      let formatted = fraction ? `${grouped}.${fraction}` : grouped;
      if (this.style === "percent") formatted += "%";
      if (this.style === "currency" && this.currency) formatted = `${this.currency}${formatted}`;
      return formatted;
    }

    formatToParts(value) {
      return [{ type: "integer", value: this.format(value) }];
    }

    resolvedOptions() {
      return {
        locale: this.locale,
        style: this.style,
        maximumFractionDigits: this.maximumFractionDigits,
        minimumFractionDigits: this.minimumFractionDigits,
        ...(this.currency ? { currency: this.currency } : {}),
      };
    }
  };
}
