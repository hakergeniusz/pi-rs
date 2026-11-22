// `node:url` — WHATWG `URL` / `URLSearchParams` plus `fileURLToPath` and
// `pathToFileURL`. QuickJS-NG ships no WHATWG implementation, so the parsing,
// authority handling, path encoding and relative resolution live here. The
// two host-specific file conversions still go through `__piOps`, which keeps
// path semantics owned by the Rust host.
//
// Fidelity target: the WHATWG URL Standard and Node's `node:url` docs,
// notably:
//   * `protocol`, `host` (hostname + port, *without* userinfo), `hostname`,
//     `port` (default ports elided), `origin` ("null" for file/custom schemes),
//   * path percent-encoding (existing `%XX` escapes are preserved),
//   * `URLSearchParams` mutations synchronise `search` and `href`,
//   * relative resolution against a base (network-path, absolute-path,
//     query-only, fragment-only and dot-segment forms).

import { Buffer } from "node:buffer";

const ops = globalThis.__piOps;

const HEX = "0123456789ABCDEF";
const DEFAULT_PORTS = {
  "http:": "80",
  "https:": "443",
  "ws:": "80",
  "wss:": "443",
  "ftp:": "21",
};
const SPECIAL = new Set(["http:", "https:", "ws:", "wss:", "ftp:", "file:"]);
const ORIGIN_SCHEMES = new Set(["http:", "https:", "ws:", "wss:", "ftp:"]);

// WHATWG path/query/userinfo encode sets keep everything except C0 controls,
// U+0020, `"`, `#`, `<`, `>`, `?`, `` ` ``, `{`, `}`. Special URLs additionally
// encode `'`. `%` is preserved only as part of a well-formed escape.
const PATH_ALLOWED = "!$&'()*+,-.;=:@/~";
const PATH_ALLOWED_SPECIAL = "!$&()*+,-.;=:@/~";
const QUERY_ALLOWED = "!$&'()*+,-.;=:@/?~";
const FRAGMENT_ALLOWED = QUERY_ALLOWED;
const USERINFO_ALLOWED = "!$&'()*+,-.;=~";

function isHexDigit(character) {
  return (
    (character >= "0" && character <= "9") ||
    (character >= "a" && character <= "f") ||
    (character >= "A" && character <= "F")
  );
}

function isHexByte(byte) {
  return (
    (byte >= 0x30 && byte <= 0x39) ||
    (byte >= 0x61 && byte <= 0x66) ||
    (byte >= 0x41 && byte <= 0x46)
  );
}

function isAlphanumeric(byte) {
  return (
    (byte >= 0x30 && byte <= 0x39) ||
    (byte >= 0x41 && byte <= 0x5a) ||
    (byte >= 0x61 && byte <= 0x7a)
  );
}

function percentByte(byte) {
  return "%" + HEX[byte >> 4] + HEX[byte & 0x0f];
}

// Percent-encode a UTF-8 byte sequence while preserving well-formed `%XX`
// escapes and the characters in `allowed`.
function encodeWith(bytes, allowed) {
  let output = "";
  for (let index = 0; index < bytes.length; index++) {
    const byte = bytes[index];
    if (
      byte === 0x25 &&
      index + 2 < bytes.length &&
      isHexByte(bytes[index + 1]) &&
      isHexByte(bytes[index + 2])
    ) {
      output +=
        "%" + String.fromCharCode(bytes[index + 1]) + String.fromCharCode(bytes[index + 2]);
      index += 2;
      continue;
    }
    const character = String.fromCharCode(byte);
    if (isAlphanumeric(byte) || allowed.indexOf(character) >= 0) output += character;
    else output += percentByte(byte);
  }
  return output;
}

function encodeComponent(text, allowed) {
  return encodeWith(Buffer.from(text, "utf8"), allowed);
}

// `application/x-www-form-urlencoded`: space becomes `+`, and only ASCII
// alphanumerics plus `*`, `-`, `.`, `_` are left unescaped.
function formEncode(text) {
  const bytes = Buffer.from(text, "utf8");
  let output = "";
  for (let index = 0; index < bytes.length; index++) {
    const byte = bytes[index];
    if (byte === 0x20) output += "+";
    else if (
      isAlphanumeric(byte) ||
      byte === 0x2a ||
      byte === 0x2d ||
      byte === 0x2e ||
      byte === 0x5f
    )
      output += String.fromCharCode(byte);
    else output += percentByte(byte);
  }
  return output;
}

function formDecode(text) {
  const bytes = [];
  for (let index = 0; index < text.length; index++) {
    const character = text[index];
    if (character === "+") {
      bytes.push(0x20);
      continue;
    }
    if (
      character === "%" &&
      index + 2 < text.length &&
      isHexDigit(text[index + 1]) &&
      isHexDigit(text[index + 2])
    ) {
      bytes.push(parseInt(text.slice(index + 1, index + 3), 16));
      index += 2;
      continue;
    }
    const encoded = Buffer.from(character, "utf8");
    for (let byteIndex = 0; byteIndex < encoded.length; byteIndex++) {
      bytes.push(encoded[byteIndex]);
    }
  }
  return Buffer.from(bytes).toString("utf8");
}

function splitQueryFragment(text) {
  let rest = text;
  let fragment = "";
  const hash = rest.indexOf("#");
  if (hash >= 0) {
    fragment = encodeComponent(rest.slice(hash + 1), FRAGMENT_ALLOWED);
    rest = rest.slice(0, hash);
  }
  let query = "";
  const question = rest.indexOf("?");
  if (question >= 0) {
    query = encodeComponent(rest.slice(question + 1), QUERY_ALLOWED);
    rest = rest.slice(0, question);
  }
  return { path: rest, query, fragment };
}

// Normalise dot segments, encode each segment, and remember a trailing slash.
function normalizePath(path, special) {
  if (path === "") return "";
  const absolute = path.startsWith("/");
  const segments = path.split("/");
  const encoded = [];
  for (let index = 0; index < segments.length; index++) {
    const segment = segments[index];
    if (segment === "") {
      if (index > 0 && index < segments.length - 1) encoded.push("");
      continue;
    }
    if (segment === ".") continue;
    if (segment === "..") {
      const last = encoded[encoded.length - 1];
      if (encoded.length > 0 && last !== "" && last !== "..") encoded.pop();
      else if (!absolute) encoded.push("..");
      continue;
    }
    encoded.push(encodeComponent(segment, special ? PATH_ALLOWED_SPECIAL : PATH_ALLOWED));
  }
  let result = (absolute ? "/" : "") + encoded.join("/");
  if (path.endsWith("/") || path.endsWith("/.") || path.endsWith("/..")) {
    if (!result.endsWith("/")) result += "/";
  }
  return result;
}

function parseAuthority(authority, protocol, special) {
  let username = "";
  let password = "";
  let hostPort = authority;
  const at = authority.lastIndexOf("@");
  if (at >= 0) {
    const userinfo = authority.slice(0, at);
    hostPort = authority.slice(at + 1);
    const colon = userinfo.indexOf(":");
    username = encodeComponent(colon < 0 ? userinfo : userinfo.slice(0, colon), USERINFO_ALLOWED);
    if (colon >= 0) {
      password = encodeComponent(userinfo.slice(colon + 1), USERINFO_ALLOWED);
    }
  }

  let host = "";
  let portText = "";
  if (hostPort.startsWith("[")) {
    const close = hostPort.indexOf("]");
    if (close < 0) throw new TypeError("Invalid URL");
    host = hostPort.slice(0, close + 1).toLowerCase();
    const tail = hostPort.slice(close + 1);
    if (tail.startsWith(":")) portText = tail.slice(1);
    else if (tail !== "") throw new TypeError("Invalid URL");
  } else {
    const colon = hostPort.lastIndexOf(":");
    if (colon >= 0) {
      host = hostPort.slice(0, colon);
      portText = hostPort.slice(colon + 1);
    } else {
      host = hostPort;
    }
  }
  if (special) host = host.toLowerCase();

  let port = "";
  if (portText !== "") {
    if (protocol === "file:") throw new TypeError("Invalid URL");
    if (!/^[0-9]+$/.test(portText)) throw new TypeError("Invalid URL");
    const number = Number(portText);
    if (!Number.isFinite(number) || number > 65535) throw new TypeError("Invalid URL");
    port = String(number);
    if (port === DEFAULT_PORTS[protocol]) port = "";
  }
  return { username, password, host, port };
}

function parseUrlParts(text, baseText) {
  // WHATWG strips leading/trailing C0-or-space and any tab/newline inside.
  let input = String(text).replace(/^[\u0000-\u0020]+|[\u0000-\u0020]+$/g, "");
  input = input.replace(/[\t\n\r]/g, "");
  const scheme = input.match(/^([A-Za-z][A-Za-z0-9+.-]*):/);
  if (!scheme) {
    if (baseText === undefined) throw new TypeError("Invalid URL");
    return resolveWithBase(baseText, input);
  }

  const protocol = scheme[1].toLowerCase() + ":";
  const special = SPECIAL.has(protocol);
  let rest = input.slice(scheme[0].length);
  if (special) rest = rest.replace(/\\/g, "/");
  // Strip the query and fragment *before* locating the authority, otherwise
  // a `?` or `#` in the path would be encoded into the pathname.
  const split = splitQueryFragment(rest);
  const pathSource = split.path;

  let hasAuthority = false;
  let username = "";
  let password = "";
  let host = "";
  let port = "";
  let path = pathSource;

  if (pathSource.startsWith("//") || special) {
    hasAuthority = true;
    // Special URLs tolerate a single leading slash (`http:/host` →
    // `http://host/`); three slashes leave an empty host and reject.
    const after = pathSource.startsWith("//")
      ? pathSource.slice(2)
      : special && pathSource.startsWith("/")
        ? pathSource.slice(1)
        : pathSource;
    const slash = after.indexOf("/");
    const authority = slash < 0 ? after : after.slice(0, slash);
    path = slash < 0 ? "" : after.slice(slash);
    const parsed = parseAuthority(authority, protocol, special);
    username = parsed.username;
    password = parsed.password;
    host = parsed.host;
    port = parsed.port;
  }

  if (special && protocol !== "file:" && host === "") throw new TypeError("Invalid URL");
  if (hasAuthority && path === "") path = "/";
  return {
    protocol,
    hasAuthority,
    username,
    password,
    host,
    port,
    path: normalizePath(path, special),
    query: split.query,
    fragment: split.fragment,
  };
}

function resolveWithBase(baseText, relative) {
  const base = parseUrlParts(baseText);
  const parts = { ...base };
  if (relative === "") return parts;

  if (relative[0] === "#") {
    parts.fragment = encodeComponent(relative.slice(1), FRAGMENT_ALLOWED);
    return parts;
  }
  if (relative[0] === "?") {
    parts.query = encodeComponent(relative.slice(1), QUERY_ALLOWED);
    return parts;
  }

  if (relative.startsWith("//")) {
    const after = relative.slice(2);
    const slash = after.indexOf("/");
    const authority = slash < 0 ? after : after.slice(0, slash);
    const split = splitQueryFragment(slash < 0 ? "" : after.slice(slash));
    const special = SPECIAL.has(parts.protocol);
    const parsed = parseAuthority(authority, parts.protocol, special);
    parts.hasAuthority = true;
    parts.username = parsed.username;
    parts.password = parsed.password;
    parts.host = parsed.host;
    parts.port = parsed.port;
    parts.path = normalizePath(split.path, special);
    parts.query = split.query;
    parts.fragment = split.fragment;
    return parts;
  }

  const special = SPECIAL.has(parts.protocol);
  const split = splitQueryFragment(relative);
  if (split.path.startsWith("/")) {
    parts.path = normalizePath(split.path, special);
  } else {
    const directory = parts.path.slice(0, parts.path.lastIndexOf("/") + 1);
    parts.path = normalizePath(directory + split.path, special);
  }
  parts.query = split.query;
  parts.fragment = split.fragment;
  return parts;
}

export class URLSearchParams {
  constructor(init, onChange) {
    this._pairs = [];
    this._onChange = typeof onChange === "function" ? onChange : null;
    if (typeof init === "string") {
      this._parse(init.startsWith("?") ? init.slice(1) : init);
    } else if (init instanceof URLSearchParams) {
      this._pairs = init._pairs.map((pair) => [pair[0], pair[1]]);
    } else if (Array.isArray(init)) {
      for (const entry of init) this._pairs.push([String(entry[0]), String(entry[1])]);
    } else if (init && typeof init === "object") {
      for (const [key, value] of Object.entries(init)) {
        this._pairs.push([String(key), String(value)]);
      }
    }
  }

  _parse(query) {
    this._pairs = [];
    for (const part of query.split("&")) {
      if (part === "") continue;
      const equals = part.indexOf("=");
      const key = equals < 0 ? part : part.slice(0, equals);
      const value = equals < 0 ? "" : part.slice(equals + 1);
      this._pairs.push([formDecode(key), formDecode(value)]);
    }
  }

  // Used by the URL setters so replacing `search` does not re-enter the
  // change callback and rebuild `href` twice.
  _reset(query) {
    this._parse(query);
  }

  _changed() {
    if (this._onChange !== null) this._onChange(this.toString());
  }

  get(name) {
    const found = this._pairs.find(([key]) => key === String(name));
    return found ? found[1] : null;
  }

  getAll(name) {
    return this._pairs.filter(([key]) => key === String(name)).map(([, value]) => value);
  }

  has(name) {
    return this._pairs.some(([key]) => key === String(name));
  }

  set(name, value) {
    const key = String(name);
    const text = String(value);
    const index = this._pairs.findIndex(([pairKey]) => pairKey === key);
    if (index < 0) this._pairs.push([key, text]);
    else this._pairs[index] = [key, text];
    this._changed();
  }

  append(name, value) {
    this._pairs.push([String(name), String(value)]);
    this._changed();
  }

  delete(name) {
    const key = String(name);
    this._pairs = this._pairs.filter(([pairKey]) => pairKey !== key);
    this._changed();
  }

  sort() {
    this._pairs.sort((left, right) => {
      if (left[0] < right[0]) return -1;
      if (left[0] > right[0]) return 1;
      return 0;
    });
    this._changed();
  }

  entries() {
    return this._pairs[Symbol.iterator]();
  }

  keys() {
    return this._pairs.map(([key]) => key)[Symbol.iterator]();
  }

  values() {
    return this._pairs.map(([, value]) => value)[Symbol.iterator]();
  }

  forEach(callback) {
    for (const [key, value] of this._pairs) callback(value, key, this);
  }

  [Symbol.iterator]() {
    return this._pairs[Symbol.iterator]();
  }

  toString() {
    return this._pairs.map(([key, value]) => `${formEncode(key)}=${formEncode(value)}`).join("&");
  }
}

export class URL {
  constructor(input, base) {
    this._parts = parseUrlParts(String(input), base === undefined ? undefined : String(base));
    this._href = "";
    this.searchParams = new URLSearchParams(this._parts.query, (text) => {
      this._parts.query = text;
      this._recompute();
    });
    this._recompute();
  }

  _recompute() {
    const parts = this._parts;
    let href = parts.protocol;
    if (parts.hasAuthority) {
      href += "//";
      if (parts.username !== "" || parts.password !== "") {
        href += parts.username;
        if (parts.password !== "") href += ":" + parts.password;
        href += "@";
      }
      href += parts.host;
      if (parts.port !== "") href += ":" + parts.port;
    }
    href += parts.path;
    if (parts.query !== "") href += "?" + parts.query;
    if (parts.fragment !== "") href += "#" + parts.fragment;
    this._href = href;
  }

  get href() {
    return this._href;
  }

  set href(value) {
    this._parts = parseUrlParts(String(value));
    this.searchParams._reset(this._parts.query);
    this._recompute();
  }

  get protocol() {
    return this._parts.protocol;
  }

  set protocol(value) {
    const text = String(value).toLowerCase();
    this._parts.protocol = text.endsWith(":") ? text : text + ":";
    this._recompute();
  }

  // Node's `host` is hostname + port; userinfo is a separate component.
  get host() {
    return this._parts.host + (this._parts.port === "" ? "" : ":" + this._parts.port);
  }

  set host(value) {
    const parsed = parseAuthority(
      String(value),
      this._parts.protocol,
      SPECIAL.has(this._parts.protocol),
    );
    this._parts.host = parsed.host;
    this._parts.port = parsed.port;
    this._recompute();
  }

  get hostname() {
    return this._parts.host;
  }

  set hostname(value) {
    const text = String(value);
    this._parts.host = SPECIAL.has(this._parts.protocol) ? text.toLowerCase() : text;
    this._recompute();
  }

  get port() {
    return this._parts.port;
  }

  set port(value) {
    const text = String(value);
    if (text === "") {
      this._parts.port = "";
      this._recompute();
      return;
    }
    if (!/^[0-9]+$/.test(text)) throw new TypeError("Invalid URL");
    const number = Number(text);
    if (!Number.isFinite(number) || number > 65535) throw new TypeError("Invalid URL");
    const canonical = String(number);
    this._parts.port = canonical === DEFAULT_PORTS[this._parts.protocol] ? "" : canonical;
    this._recompute();
  }

  get origin() {
    const protocol = this._parts.protocol;
    return ORIGIN_SCHEMES.has(protocol) ? protocol + "//" + this.host : "null";
  }

  get pathname() {
    return this._parts.path;
  }

  set pathname(value) {
    this._parts.path = normalizePath(String(value), SPECIAL.has(this._parts.protocol));
    this._recompute();
  }

  get search() {
    return this._parts.query === "" ? "" : "?" + this._parts.query;
  }

  set search(value) {
    const text = String(value);
    const query = text.startsWith("?") ? text.slice(1) : text;
    this._parts.query = encodeComponent(query, QUERY_ALLOWED);
    this.searchParams._reset(this._parts.query);
    this._recompute();
  }

  get hash() {
    return this._parts.fragment === "" ? "" : "#" + this._parts.fragment;
  }

  set hash(value) {
    const text = String(value);
    const fragment = text.startsWith("#") ? text.slice(1) : text;
    this._parts.fragment =
      fragment === "" ? "" : encodeComponent(fragment, FRAGMENT_ALLOWED);
    this._recompute();
  }

  get username() {
    return this._parts.username;
  }

  get password() {
    return this._parts.password;
  }

  toString() {
    return this._href;
  }

  toJSON() {
    return this._href;
  }
}

export function fileURLToPath(url, options) {
  const text =
    typeof url === "string"
      ? url
      : url && url.href !== undefined
        ? url.href
        : String(url);
  const raw = JSON.parse(ops.nodeUrlFileToPath(text));
  if (raw.error) {
    const error = new Error(raw.error);
    error.code = raw.code;
    throw error;
  }
  if (options && options.windows) return raw.ok.replace(/^\//, "C:\\").replace(/\//g, "\\");
  return raw.ok;
}

export function pathToFileURL(path) {
  const raw = JSON.parse(ops.nodeUrlPathToFileUrl(String(path)));
  return new URL(raw.ok);
}

export function domainToASCII(domain) {
  return String(domain).toLowerCase();
}

export function domainToUnicode(domain) {
  return String(domain).toLowerCase();
}

if (globalThis.URL === undefined) globalThis.URL = URL;
if (globalThis.URLSearchParams === undefined) globalThis.URLSearchParams = URLSearchParams;

export default {
  URL,
  URLSearchParams,
  fileURLToPath,
  pathToFileURL,
  domainToASCII,
  domainToUnicode,
};
