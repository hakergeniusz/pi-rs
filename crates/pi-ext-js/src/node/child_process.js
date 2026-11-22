// `node:child_process` — a sync-first subset built on the host's `pi.exec`
// primitive (`core/exec.ts:49-162`). Upstream extensions only use the sync
// calls; `spawn` returns a shape-compatible object whose events fire once
// the engine drains its job queue (see `WIRING.md` for the gap).

import { Buffer } from "node:buffer";

const ops = globalThis.__piOps;

function hostCwd(options) {
  if (options && options.cwd) return String(options.cwd);
  return JSON.parse(ops.nodeProcess("cwd"));
}

function run(command, args, options) {
  const config = {
    cwd: hostCwd(options),
    maxBuffer: options && options.maxBuffer,
    timeout: options && options.timeout,
  };
  const raw = ops.exec(command, JSON.stringify(args.map(String)), JSON.stringify(config));
  const result = JSON.parse(raw);
  if (result.execError) throw new Error(result.execError);
  return result;
}

function encoded(text, options) {
  if (options && options.encoding === "buffer") return Buffer.from(text, "utf8");
  return text;
}

function commandError(command, result) {
  const error = new Error(`Command failed: ${command}\n${result.stderr}`);
  error.status = result.code;
  error.code = result.code;
  error.signal = result.killed ? "SIGTERM" : null;
  error.stdout = result.stdout;
  error.stderr = result.stderr;
  return error;
}

export function execFileSync(file, args = [], options = {}) {
  const result = run(String(file), args, options);
  if (result.code !== 0) throw commandError(String(file), result);
  return encoded(result.stdout, options);
}

export function execSync(command, options = {}) {
  const result = run("/bin/sh", ["-c", String(command)], options);
  if (result.code !== 0) throw commandError(String(command), result);
  return encoded(options.stdio === "inherit" ? "" : result.stdout, options);
}

export function spawnSync(file, args = [], options = {}) {
  const result = run(String(file), args, options);
  return {
    pid: 0,
    status: result.code,
    signal: result.killed ? "SIGTERM" : null,
    error: result.execError ? new Error(result.execError) : undefined,
    stdout: encoded(result.stdout, options),
    stderr: encoded(result.stderr, options),
    output: [null, encoded(result.stdout, options), encoded(result.stderr, options)],
  };
}

function emitter() {
  const handlers = new Map();
  return {
    on(event, callback) {
      const list = handlers.get(event) ?? [];
      list.push(callback);
      handlers.set(event, list);
      return this;
    },
    once(event, callback) {
      return this.on(event, (...args) => {
        this.removeListener(event, callback);
        callback(...args);
      });
    },
    removeListener(event, callback) {
      handlers.set(
        event,
        (handlers.get(event) ?? []).filter((item) => item !== callback),
      );
      return this;
    },
    emit(event, ...args) {
      for (const callback of handlers.get(event) ?? []) callback(...args);
    },
  };
}

export function spawn(file, args = [], options = {}) {
  const out = emitter();
  const err = emitter();
  const child = emitter();
  child.pid = 0;
  child.spawnfile = String(file);
  child.spawnargs = args.map(String);
  child.stdout = out;
  child.stderr = err;
  child.stdin = { write: () => true, end: () => {}, on: () => child.stdin };
  child.kill = () => true;
  child.connected = false;

  // The host primitive is synchronous; deliver its output on the next
  // microtask so handlers registered after `spawn()` still observe it.
  Promise.resolve().then(() => {
    try {
      const result = run(String(file), args, options);
      out.emit("data", encoded(result.stdout, options));
      err.emit("data", encoded(result.stderr, options));
      child.emit("close", result.code, result.killed ? "SIGTERM" : null);
      child.emit("exit", result.code, result.killed ? "SIGTERM" : null);
    } catch (error) {
      child.emit("error", error);
    }
  });
  return child;
}

export function fork() {
  throw new Error("child_process.fork is not available in the extensions host");
}

export default { execSync, execFileSync, spawnSync, spawn, fork };
