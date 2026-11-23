// `node:fs/promises` — the async arm imported by the bundled llama provider
// (`extensions/llama/huggingface.ts:1`) and by pi-sol-pi's observation pack.
// It re-exports the promise-returning half of the `node:fs` shim rather than
// duplicating the host ops, and layers a minimal `FileHandle`/`open` pair on
// the same opaque fd table used by `fs.openSync`.

import { Buffer } from "node:buffer";
import {
  chmodSync,
  closeSync,
  openSync,
  readSync,
  statSync,
  writeSync,
  appendFile,
  lstat,
  mkdir,
  readFile,
  readdir,
  realpath,
  rename,
  rmSync,
  stat,
  unlink,
  writeFile,
} from "node:fs";

export {
  appendFile,
  lstat,
  mkdir,
  readFile,
  readdir,
  realpath,
  rename,
  stat,
  unlink,
  writeFile,
};

export const rm = (path, options) => Promise.resolve().then(() => rmSync(path, options));

// The flag values are the Linux POSIX values also exposed on `node:fs`'s
// `constants`; `openSync` parses them numerically through the host op.
export const constants = {
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

/** The subset of Node's `FileHandle` the observation pack uses. */
export class FileHandle {
  constructor(fd, path) {
    this.fd = fd;
    this.path = path;
    this.closed = false;
  }

  async stat() {
    return statSync(this.path);
  }

  async read(buffer, offset = 0, length, position = null) {
    const bytesRead = readSync(this.fd, buffer, offset, length, position);
    return { bytesRead, buffer };
  }

  async readFile(options) {
    const size = statSync(this.path).size;
    const buffer = Buffer.alloc(size);
    const { bytesRead } = await this.read(buffer, 0, size, 0);
    const slice = buffer.subarray(0, bytesRead);
    return options && options.encoding ? slice.toString(options.encoding) : slice;
  }

  async writeFile(data, options) {
    const bytes = typeof data === "string"
      ? Buffer.from(data, (options && options.encoding) || "utf8")
      : Buffer.from(data);
    writeSync(this.fd, bytes);
  }

  async write(data, offset, length, position) {
    return writeSync(this.fd, data, offset, length, position);
  }

  async close() {
    if (this.closed) return;
    this.closed = true;
    closeSync(this.fd);
  }
}

export function open(path, flags = "r", mode) {
  return Promise.resolve().then(() => {
    const fd = openSync(path, flags, mode);
    // `openSync` accepts `mode` for signature fidelity but does not chmod;
    // apply it here so a created file honours the caller's requested mode.
    if (typeof mode === "number") chmodSync(path, mode);
    return new FileHandle(fd, path);
  });
}

export default {
  appendFile,
  lstat,
  mkdir,
  open,
  FileHandle,
  constants,
  readFile,
  readdir,
  realpath,
  rename,
  stat,
  unlink,
  writeFile,
  rm,
};
