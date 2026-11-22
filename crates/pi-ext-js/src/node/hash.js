// `node:crypto` — the hash and UUID surface used by the bundled MCP OAuth
// flow (`extensions/mcp/oauth.ts:13`, `packages/mcp/src/naming.ts:6`). The
// digest itself is computed by the host's sha2 dependency; the JS half
// only implements Node's chainable Hash object.

import { Buffer } from "node:buffer";

const ops = globalThis.__piOps;

function unwrap(raw) {
  const value = JSON.parse(raw);
  if (value.error) {
    const error = new Error(value.error);
    error.code = value.code;
    throw error;
  }
  return value.ok;
}

export function createHash(algorithm) {
  const chunks = [];
  return {
    update(data, encoding = "utf8") {
      const bytes = typeof data === "string" ? Buffer.from(data, encoding) : Buffer.from(data);
      chunks.push(bytes);
      return this;
    },
    digest(encoding = "buffer") {
      const bytes = Buffer.concat(chunks);
      const raw = JSON.parse(
        ops.nodeCryptoHash(String(algorithm), JSON.stringify(Array.from(bytes)), String(encoding)),
      );
      if (raw.error) {
        const error = new Error(raw.error);
        error.code = raw.code;
        throw error;
      }
      if (encoding === "buffer") return Buffer.from(raw.ok);
      return raw.ok;
    },
    copy() {
      const clone = createHash(algorithm);
      for (const chunk of chunks) clone.update(chunk);
      return clone;
    },
  };
}

export function createHmac(algorithm, key) {
  void algorithm;
  void key;
  throw new Error("crypto.createHmac is not available in the extensions host");
}

export function randomUUID() {
  return unwrap(ops.nodeCryptoRandomUuid());
}

export function randomBytes(size) {
  const bytes = new Uint8Array(size);
  for (let i = 0; i < size; i++) bytes[i] = Math.floor(Math.random() * 256);
  return Buffer.from(bytes);
}

export const getHashes = () => ["sha256", "sha512"];

export const constants = {
  defaultEncoding: "buffer",
};

export default {
  createHash,
  createHmac,
  randomUUID,
  randomBytes,
  getHashes,
  constants,
  webcrypto: globalThis.crypto,
};
