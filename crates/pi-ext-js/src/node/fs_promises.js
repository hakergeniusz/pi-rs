// `node:fs/promises` — the async arm imported by the bundled llama provider
// (`extensions/llama/huggingface.ts:1`). It re-exports the promise-returning
// half of the `node:fs` shim rather than duplicating the host ops.

import {
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

export default {
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
  rm,
};
