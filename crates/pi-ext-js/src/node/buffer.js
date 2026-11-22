// `node:buffer` — the subset of Node's Buffer API extensions use to move
// bytes between `node:fs`, `node:crypto` and `node:child_process`.
// Encoding conversions are host ops so base64/hex agree with Rust's base64
// crate; the byte container itself is a Uint8Array subclass.

const ops = globalThis.__piOps;

function decode(raw, encoding) {
  const value = JSON.parse(raw);
  if (value.error) throw new Error(value.error);
  return value.ok;
}

function toBytes(value, encoding) {
  if (typeof value === "string") {
    const raw = JSON.parse(ops.nodeBufferEncode(value, encoding || "utf8"));
    if (raw.error) throw new Error(raw.error);
    return Uint8Array.from(raw.ok);
  }
  if (typeof value === "number") return new Uint8Array(value);
  if (value instanceof Uint8Array) return new Uint8Array(value);
  if (value instanceof ArrayBuffer) return new Uint8Array(value);
  if (Array.isArray(value)) return Uint8Array.from(value);
  if (value && typeof value.length === "number") {
    const bytes = new Uint8Array(value.length);
    for (let i = 0; i < value.length; i++) bytes[i] = value[i] & 0xff;
    return bytes;
  }
  throw new TypeError(
    "The first argument must be of type string or an instance of Buffer, ArrayBuffer, or Array",
  );
}

export class Buffer extends Uint8Array {
  constructor(value, encodingOrOffset) {
    if (typeof value === "number") super(value);
    else super(toBytes(value, encodingOrOffset));
  }

  static from(value, encodingOrOffset, length) {
    if (typeof value === "number") return new Buffer(value);
    return new Buffer(value, encodingOrOffset, length);
  }

  static alloc(size, fill, encoding) {
    const bytes = new Buffer(size);
    if (fill !== undefined && fill !== null) {
      const pattern = toBytes(fill, encoding || "utf8");
      for (let i = 0; i < bytes.length; i++) bytes[i] = pattern[i % pattern.length];
    }
    return bytes;
  }

  static allocUnsafe(size) {
    return new Buffer(size);
  }

  static isBuffer(value) {
    return value instanceof Buffer;
  }

  static byteLength(value, encoding) {
    if (typeof value === "string") return toBytes(value, encoding || "utf8").length;
    if (value instanceof ArrayBuffer) return value.byteLength;
    return value && typeof value.length === "number" ? value.length : 0;
  }

  static concat(list, totalLength) {
    const arrays = Array.from(list);
    const size = totalLength ?? arrays.reduce((sum, item) => sum + item.length, 0);
    const out = new Buffer(size);
    let offset = 0;
    for (const item of arrays) {
      for (let i = 0; i < item.length && offset < size; i++) out[offset++] = item[i];
    }
    return out;
  }

  static compare(left, right) {
    const size = Math.min(left.length, right.length);
    for (let i = 0; i < size; i++) {
      if (left[i] !== right[i]) return left[i] < right[i] ? -1 : 1;
    }
    return left.length === right.length ? 0 : left.length < right.length ? -1 : 1;
  }

  toString(encoding = "utf8", start, end) {
    const slice = start === undefined ? this : this.subarray(start, end);
    return decode(
      ops.nodeBufferDecode(JSON.stringify(Array.from(slice)), encoding || "utf8"),
      encoding || "utf8",
    );
  }

  toJSON() {
    return { type: "Buffer", data: Array.from(this) };
  }

  equals(other) {
    return Buffer.compare(this, other) === 0;
  }
}

export const SlowBuffer = Buffer;
export const kMaxLength = 2 ** 31 - 1;
export const constants = { MAX_LENGTH: kMaxLength };

// Node exposes Buffer globally; extensions rely on that without importing.
if (globalThis.Buffer === undefined) globalThis.Buffer = Buffer;

export default {
  Buffer,
  SlowBuffer,
  kMaxLength,
  constants,
  isUtf8: () => true,
};
