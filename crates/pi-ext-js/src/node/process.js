// `node:process` — the ambient object Node extensions expect. cwd and mode
// come from the host's HostCore, not the OS process, so a session's
// configured cwd is what `process.cwd()` reports. Credential-shaped
// environment keys are filtered by the host op (see `WIRING.md`).

const ops = globalThis.__piOps;

function value(operation) {
  return JSON.parse(ops.nodeProcess(operation));
}

export const env = JSON.parse(ops.nodeProcessEnv());
export const platform = value("platform");
export const arch = value("arch");
export const version = value("version");
export const versions = { node: version.replace(/^v/, "") };
export const title = value("title");
export const argv = value("argv");
export const execArgv = [];
export const pid = 1;
export const ppid = 0;

export function cwd() {
  return value("cwd");
}

export function chdir(path) {
  // The embedded host owns cwd; changing it would desynchronise the `pi`
  // API and every extension context. Fail loudly instead of pretending.
  throw new Error(`process.chdir('${path}') is not available in the extensions host`);
}

export function nextTick(callback, ...args) {
  Promise.resolve().then(() => callback(...args));
}

export function uptime() {
  return 0;
}

export function memoryUsage() {
  return { rss: 0, heapTotal: 0, heapUsed: 0, external: 0 };
}

export const stdout = {
  write: (text) => ops.print("log", String(text)),
  isTTY: false,
};
export const stderr = {
  write: (text) => ops.print("error", String(text)),
  isTTY: false,
};

// Node has no `exit` in an embedded host: terminating the pi process from
// an extension would bypass every session teardown path. Keep the call
// shape but make it a no-op, recorded as a deviation in `WIRING.md`.
export function exit(code) {
  void code;
}

// Node exposes `process` globally; extensions written against that assume
// it exists without an import.
if (globalThis.process === undefined) {
  globalThis.process = {
    env,
    platform,
    arch,
    version,
    cwd,
    nextTick,
    stdout,
    stderr,
    exit,
  };
}

export default {
  env,
  platform,
  arch,
  version,
  versions,
  title,
  argv,
  execArgv,
  pid,
  ppid,
  cwd,
  chdir,
  nextTick,
  uptime,
  memoryUsage,
  stdout,
  stderr,
  exit,
};
