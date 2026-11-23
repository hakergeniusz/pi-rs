// `node:fs` — the synchronous surface the bundled MCP extension uses
// (`extensions/mcp/{config,log,cli}.ts`) plus the calls third-party
// extensions commonly make. Every call crosses through host ops; no Node
// runtime is embedded. Errors carry Node's `code` and `path` properties.

import { Buffer } from "node:buffer";

const ops = globalThis.__piOps;

function unwrap(raw, operation, path) {
  const value = JSON.parse(raw);
  if (value.error) {
    const error = new Error(`${value.code}: ${value.error}, ${operation} '${path}'`);
    error.code = value.code;
    error.path = path;
    throw error;
  }
  return value.ok;
}

function encodingOf(options) {
  if (typeof options === "string") return options;
  if (options && typeof options === "object") return options.encoding;
  return undefined;
}

export class Stats {
  constructor(value) {
    this.isFileValue = !!value.isFile;
    this.isDirectoryValue = !!value.isDirectory;
    this.isSymbolicLinkValue = !!value.isSymbolicLink;
    this.isBlockDeviceValue = !!value.isBlockDevice;
    this.isCharacterDeviceValue = !!value.isCharacterDevice;
    this.isFIFOValue = !!value.isFIFO;
    this.isSocketValue = !!value.isSocket;
    this.size = value.size;
    this.mtimeMs = value.mtimeMs;
    this.mode = value.mode;
    this.mtime = new Date(value.mtimeMs);
  }
  isFile() {
    return this.isFileValue;
  }
  isDirectory() {
    return this.isDirectoryValue;
  }
  isSymbolicLink() {
    return this.isSymbolicLinkValue;
  }
  isBlockDevice() {
    return this.isBlockDeviceValue;
  }
  isCharacterDevice() {
    return this.isCharacterDeviceValue;
  }
  isFIFO() {
    return this.isFIFOValue;
  }
  isSocket() {
    return this.isSocketValue;
  }
}

export class Dirent {
  constructor(name, isDirectory, isFile, isSymbolicLink) {
    this.name = name;
    this.isDirectoryValue = isDirectory;
    this.isFileValue = isFile;
    this.isSymbolicLinkValue = isSymbolicLink;
  }
  isDirectory() {
    return this.isDirectoryValue;
  }
  isFile() {
    return this.isFileValue;
  }
  isSymbolicLink() {
    return this.isSymbolicLinkValue;
  }
}

export function existsSync(path) {
  return ops.nodeFsExistsSync(String(path)) === "true";
}

export function readFileSync(path, options) {
  const encoding = encodingOf(options) ?? "buffer";
  const value = unwrap(ops.nodeFsReadFileSync(String(path), encoding), "open", String(path));
  return encoding === "buffer" ? Buffer.from(value) : value;
}

export function writeFileSync(path, data, options) {
  const encoding = encodingOf(options) ?? "utf8";
  const text = typeof data === "string" ? data : Buffer.from(data).toString(encoding);
  unwrap(ops.nodeFsWriteFileSync(String(path), text, encoding), "write", String(path));
}

export function appendFileSync(path, data, options) {
  const encoding = encodingOf(options) ?? "utf8";
  const text = typeof data === "string" ? data : Buffer.from(data).toString(encoding);
  unwrap(ops.nodeFsAppendFileSync(String(path), text, encoding), "append", String(path));
}

export function readdirSync(path, options) {
  const names = unwrap(ops.nodeFsReadDirSync(String(path)), "scandir", String(path));
  if (options && options.withFileTypes) {
    return names.map((name) => {
      let isDirectory = false;
      let isFile = false;
      let isSymbolicLink = false;
      try {
        // Dirents describe the entry itself, so a symlink must not be
        // followed (the same rule Node uses for `withFileTypes`).
        const stat = unwrap(ops.nodeFsLstatSync(`${String(path)}/${name}`), "lstat", String(path));
        isDirectory = !!stat.isDirectory;
        isFile = !!stat.isFile;
        isSymbolicLink = !!stat.isSymbolicLink;
      } catch {
        // A broken symlink still gets a Dirent with all type flags false.
      }
      return new Dirent(name, isDirectory, isFile, isSymbolicLink);
    });
  }
  return names;
}

export function statSync(path) {
  return new Stats(unwrap(ops.nodeFsStatSync(String(path)), "stat", String(path)));
}

// Unlike `statSync`, lstat reports the link itself (including
// `isSymbolicLink()`), matching Node's `fs.lstatSync`.
export function lstatSync(path) {
  return new Stats(unwrap(ops.nodeFsLstatSync(String(path)), "lstat", String(path)));
}

export function mkdirSync(path, options) {
  const recursive = !!(options && options.recursive);
  unwrap(
    ops.nodeFsMkdirSync(String(path), recursive ? "true" : "false"),
    "mkdir",
    String(path),
  );
  return recursive ? String(path) : undefined;
}

// Node validates `mode` as a non-negative integer; strings are octal
// (`chmodSync(path, "755") === chmodSync(path, 0o755)`). The host op receives
// a decimal string so the Rust side stays a pure integer check.
function modeOf(mode) {
  if (typeof mode === "string") {
    if (!/^[0-7]+$/.test(mode)) {
      throw new TypeError("The \"mode\" argument must be an integer or an octal string");
    }
    return String(parseInt(mode, 8));
  }
  if (typeof mode !== "number" || !Number.isInteger(mode) || mode < 0) {
    throw new TypeError("The \"mode\" argument must be a non-negative integer");
  }
  return String(mode);
}

// `chmodSync` maps to real Unix permissions in the host op; on non-Unix
// platforms it is a documented no-op (the host has no portable mode seam).
export function chmodSync(path, mode) {
  unwrap(
    ops.nodeFsChmodSync(String(path), modeOf(mode)),
    "chmod",
    String(path),
  );
}

// `mkdtempSync(prefix)` appends Node's six random alphanumerics under the
// given prefix and returns the created directory. The optional `encoding`
// ("buffer") is honoured the same way `readFileSync` honours it.
export function mkdtempSync(prefix, options) {
  const created = unwrap(ops.nodeFsMkdtempSync(String(prefix)), "mkdtemp", String(prefix));
  return encodingOf(options) === "buffer" ? Buffer.from(created) : created;
}

export function renameSync(oldPath, newPath) {
  unwrap(
    ops.nodeFsRenameSync(String(oldPath), String(newPath)),
    "rename",
    String(oldPath),
  );
}

export function unlinkSync(path) {
  unwrap(ops.nodeFsUnlinkSync(String(path)), "unlink", String(path));
}

export function rmSync(path, options) {
  unwrap(
    ops.nodeFsRmSync(
      String(path),
      options && options.recursive ? "true" : "false",
      options && options.force ? "true" : "false",
    ),
    "rm",
    String(path),
  );
}

export const rmdirSync = rmSync;

export function realpathSync(path) {
  return unwrap(ops.nodeFsRealpathSync(String(path)), "realpath", String(path));
}

export function readlinkSync(path) {
  return unwrap(ops.nodeFsReadlinkSync(String(path)), "readlink", String(path));
}

export function symlinkSync(target, path) {
  unwrap(ops.nodeFsSymlinkSync(String(target), String(path)), "symlink", String(path));
}

// Minimal fd handles for the sweep's `fs.openSync` callers (`pi-cord/src/chat.ts:792`
// and `src/lock.ts:52`). The fd is an opaque host id; only the four calls
// below accept it. Flags r/r+/w/wx/a(ax) map to OpenOptions; `mode` is
// accepted for signature fidelity but ignored (the host never chmods).
// No file streams, async fd APIs, fchmod/fsync/ftruncate or real OS fd
// numbers are provided - documented in `node/mod.rs`.
export function openSync(path, flags = "r", mode) {
  return unwrap(
    ops.nodeFsOpenSync(String(path), String(flags), mode === undefined ? "" : String(mode)),
    "open",
    String(path),
  );
}

export function closeSync(fd) {
  unwrap(ops.nodeFsCloseSync(fd), "close", String(fd));
}

function bytesOf(data, offset, length, encoding) {
  const start = offset == null ? 0 : offset;
  const end = length == null ? undefined : start + length;
  if (typeof data === "string") {
    const raw = JSON.parse(ops.nodeBufferEncode(data, encoding || "utf8"));
    if (raw.error) throw new Error(raw.error);
    return raw.ok.slice(start, end);
  }
  const source = data instanceof Uint8Array ? data : Uint8Array.from(data);
  return Array.from(source.subarray(start, end));
}

export function readSync(fd, buffer, offset = 0, length, position = null) {
  const size = length === undefined ? buffer.length - offset : length;
  const bytes = unwrap(
    ops.nodeFsReadSync(fd, size, position === null ? -1 : position),
    "read",
    String(fd),
  );
  for (let index = 0; index < bytes.length; index++) buffer[offset + index] = bytes[index];
  return bytes.length;
}

export function writeSync(fd, data, offset = 0, length, position = null, encoding) {
  let encodingValue = encoding;
  if (typeof offset === "string" || typeof length === "string") {
    // Node's `fs.writeSync(fd, string, position?, encoding?)` overload.
    const positionOrEncoding = offset;
    encodingValue = typeof length === "string" ? length : typeof offset === "string" ? offset : encoding;
    if (typeof positionOrEncoding === "number") position = positionOrEncoding;
    offset = undefined;
    length = undefined;
  }
  const bytes = bytesOf(data, offset ?? 0, length, encodingValue);
  return unwrap(
    ops.nodeFsWriteSync(fd, JSON.stringify(bytes), position === null ? -1 : position),
    "write",
    String(fd),
  );
}

// Promise forms settle from the same host ops. Upstream is async here; the
// embedded host has a synchronous filesystem, so the observable result is
// the same once the engine's job queue drains.
export function readFile(path, options) {
  return Promise.resolve().then(() => readFileSync(path, options));
}

export function writeFile(path, data, options) {
  return Promise.resolve().then(() => writeFileSync(path, data, options));
}

export function appendFile(path, data, options) {
  return Promise.resolve().then(() => appendFileSync(path, data, options));
}

export function mkdir(path, options) {
  return Promise.resolve().then(() => mkdirSync(path, options));
}

export function stat(path) {
  return Promise.resolve().then(() => statSync(path));
}

export function lstat(path) {
  return Promise.resolve().then(() => lstatSync(path));
}

export function readdir(path, options) {
  return Promise.resolve().then(() => readdirSync(path, options));
}

export function unlink(path) {
  return Promise.resolve().then(() => unlinkSync(path));
}

export function rename(oldPath, newPath) {
  return Promise.resolve().then(() => renameSync(oldPath, newPath));
}

export function realpath(path) {
  return Promise.resolve().then(() => realpathSync(path));
}

export const constants = {
  F_OK: 0,
  R_OK: 4,
  W_OK: 2,
  X_OK: 1,
  // POSIX open flags (Linux values); `openSync` accepts these numerically.
  O_RDONLY: 0,
  O_WRONLY: 1,
  O_RDWR: 2,
  O_CREAT: 64,
  O_EXCL: 128,
  O_NOCTTY: 256,
  O_TRUNC: 512,
  O_APPEND: 1024,
  O_NOFOLLOW: 131072,
};

export default {
  Stats,
  Dirent,
  existsSync,
  readFileSync,
  writeFileSync,
  appendFileSync,
  readdirSync,
  statSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  chmodSync,
  renameSync,
  unlinkSync,
  rmSync,
  rmdirSync,
  realpathSync,
  readlinkSync,
  symlinkSync,
  openSync,
  closeSync,
  readSync,
  writeSync,
  readFile,
  writeFile,
  appendFile,
  mkdir,
  stat,
  lstat,
  readdir,
  unlink,
  rename,
  realpath,
  constants,
};
