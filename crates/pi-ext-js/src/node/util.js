// `node:util` — formatting/inspection helpers and the type predicates
// extensions commonly use. `TextEncoder`/`TextDecoder` are wired through
// the `node:buffer` ops so utf8/hex/base64 agree with the rest of the host.

import { Buffer } from "node:buffer";

export function inspect(value, options = {}) {
  const depth = options.depth ?? 2;
  const seen = new Set();
  function render(item, level) {
    if (item === null) return "null";
    if (item === undefined) return "undefined";
    if (typeof item === "string") {
      return options.quoteStrings === false ? item : JSON.stringify(item);
    }
    if (typeof item === "number" || typeof item === "boolean" || typeof item === "bigint") {
      return String(item);
    }
    if (typeof item === "function") return `[Function: ${item.name || "anonymous"}]`;
    if (item instanceof Error) return `${item.name}: ${item.message}`;
    if (item instanceof Date) return item.toISOString();
    if (Buffer.isBuffer(item)) return `<Buffer ${item.toString("hex")}>`;
    if (seen.has(item)) return "[Circular *1]";
    seen.add(item);
    if (Array.isArray(item)) {
      if (level >= depth) return `[Array: ${item.length}]`;
      const parts = item.map((entry) => render(entry, level + 1));
      seen.delete(item);
      return `[ ${parts.join(", ")} ]`;
    }
    if (level >= depth) return "[Object]";
    const parts = Object.keys(item).map((key) => `${key}: ${render(item[key], level + 1)}`);
    seen.delete(item);
    return `{ ${parts.join(", ")} }`;
  }
  return render(value, 0);
}

export function format(first, ...rest) {
  if (typeof first !== "string") return [first, ...rest].map((item) => inspect(item)).join(" ");
  let index = 0;
  let out = "";
  for (let cursor = 0; cursor < first.length; cursor++) {
    if (first[cursor] !== "%" || cursor + 1 >= first.length) {
      out += first[cursor];
      continue;
    }
    const specifier = first[++cursor];
    if (specifier === "%") {
      out += "%";
    } else if (index >= rest.length) {
      out += `%${specifier}`;
    } else if (specifier === "s") {
      out += String(rest[index++]);
    } else if (specifier === "d" || specifier === "i" || specifier === "f") {
      out += String(Number(rest[index++]));
    } else if (specifier === "j") {
      out += JSON.stringify(rest[index++]);
    } else if (specifier === "o" || specifier === "O") {
      out += inspect(rest[index++]);
    } else {
      out += `%${specifier}`;
    }
  }
  for (; index < rest.length; index++) out += ` ${inspect(rest[index])}`;
  return out;
}

export const isDeepStrictEqual = (left, right) => JSON.stringify(left) === JSON.stringify(right);

export function promisify(fn) {
  return (...args) =>
    new Promise((resolve, reject) => {
      fn(...args, (error, value) => (error ? reject(error) : resolve(value)));
    });
}

export const deprecate = (fn) => fn;

export const types = {
  isString: (value) => typeof value === "string",
  isNumber: (value) => typeof value === "number",
  isBoolean: (value) => typeof value === "boolean",
  isFunction: (value) => typeof value === "function",
  isArray: (value) => Array.isArray(value),
  isObject: (value) => value !== null && typeof value === "object",
  isUndefined: (value) => value === undefined,
  isNull: (value) => value === null,
  isBigInt: (value) => typeof value === "bigint",
  isSymbol: (value) => typeof value === "symbol",
  isPromise: (value) => !!value && typeof value.then === "function",
  isDate: (value) => value instanceof Date,
  isRegExp: (value) => value instanceof RegExp,
  isBuffer: (value) => Buffer.isBuffer(value),
  isMap: (value) => value instanceof Map,
  isSet: (value) => value instanceof Set,
};

export class TextEncoder {
  get encoding() {
    return "utf-8";
  }
  encode(text = "") {
    return Buffer.from(String(text), "utf8");
  }
  encodeInto(text, destination) {
    const bytes = this.encode(text);
    let copied = 0;
    for (; copied < bytes.length && copied < destination.length; copied++) {
      destination[copied] = bytes[copied];
    }
    return { read: String(text).length, written: copied };
  }
}

export class TextDecoder {
  constructor(encoding = "utf-8", options = {}) {
    this.encoding = encoding;
    this.fatal = !!options.fatal;
  }
  decode(bytes) {
    const source = bytes === undefined ? new Uint8Array(0) : bytes;
    return Buffer.from(source).toString(this.encoding === "utf-8" ? "utf8" : this.encoding);
  }
}

if (globalThis.TextEncoder === undefined) globalThis.TextEncoder = TextEncoder;
if (globalThis.TextDecoder === undefined) globalThis.TextDecoder = TextDecoder;

export default {
  format,
  inspect,
  isDeepStrictEqual,
  promisify,
  deprecate,
  types,
  TextEncoder,
  TextDecoder,
};
