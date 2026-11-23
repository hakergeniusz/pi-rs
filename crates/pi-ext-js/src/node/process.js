// `node:process` — re-exports the host-wide `process` object installed by
// `process_global.js` at engine creation. Keeping one object (rather than a
// second module-local copy) means an extension that imports the module sees
// the same `env` mutations and the same recorded `exitCode` as one that just
// uses the global. The global bootstrap owns all values; see that file for
// the host-state boundary and the documented no-exit/no-signal deviations.

const p = globalThis.process;

export const env = p.env;
export const platform = p.platform;
export const arch = p.arch;
export const version = p.version;
export const versions = p.versions;
export const title = p.title;
export const argv = p.argv;
export const execArgv = p.execArgv;
export const pid = p.pid;
export const ppid = p.ppid;
export const execPath = p.execPath;
export const stdout = p.stdout;
export const stderr = p.stderr;
export const stdin = p.stdin;

export const cwd = p.cwd;
export const chdir = p.chdir;
export const nextTick = p.nextTick;
export const uptime = p.uptime;
export const memoryUsage = p.memoryUsage;
export const exit = p.exit;
export const on = p.on;
export const once = p.once;
export const removeListener = p.removeListener;
export const kill = p.kill;
export const hrtime = p.hrtime;

export default p;
