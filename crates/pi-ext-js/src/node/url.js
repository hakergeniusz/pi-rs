// `node:url` — `fileURLToPath` is used by the extension loader upstream
// (`core/extensions/loader.ts:9`) and by the bundled MCP runtime
// (`extensions/mcp/runtime.ts:10`), `pathToFileURL` is its inverse. A small
// URL/URLSearchParams pair is provided because QuickJS-NG ships no WHATWG
// implementation.

const ops = globalThis.__piOps;

export class URLSearchParams {
  constructor(init) {
    this._pairs = [];
    if (typeof init === "string") {
      const query = init.startsWith("?") ? init.slice(1) : init;
      for (const part of query.split("&")) {
        if (!part) continue;
        const [key, value = ""] = part.split("=");
        this._pairs.push([decodeURIComponent(key), decodeURIComponent(value)]);
      }
    } else if (init && typeof init === "object") {
      for (const [key, value] of Object.entries(init)) this._pairs.push([key, String(value)]);
    }
  }

  get(name) {
    const found = this._pairs.find(([key]) => key === name);
    return found ? found[1] : null;
  }

  has(name) {
    return this._pairs.some(([key]) => key === name);
  }

  set(name, value) {
    const index = this._pairs.findIndex(([key]) => key === name);
    if (index < 0) this._pairs.push([name, String(value)]);
    else this._pairs[index] = [name, String(value)];
  }

  append(name, value) {
    this._pairs.push([name, String(value)]);
  }

  delete(name) {
    this._pairs = this._pairs.filter(([key]) => key !== name);
  }

  entries() {
    return this._pairs[Symbol.iterator]();
  }

  forEach(callback) {
    for (const [key, value] of this._pairs) callback(value, key, this);
  }

  toString() {
    return this._pairs
      .map(([key, value]) => `${encodeURIComponent(key)}=${encodeURIComponent(value)}`)
      .join("&");
  }
}

export class URL {
  constructor(input, base) {
    let text = String(input);
    if (base !== undefined && !/^[A-Za-z][A-Za-z0-9+.-]*:/.test(text)) {
      const baseText = String(base);
      const scheme = baseText.match(/^[A-Za-z][A-Za-z0-9+.-]*:/)?.[0] ?? "";
      const afterScheme = baseText.slice(scheme.length);
      const prefix = afterScheme.startsWith("//")
        ? `${scheme}//${afterScheme.slice(2).split("/")[0]}`
        : scheme;
      text = `${prefix}/${text}`.replace(/\/{2,}/g, (match) =>
        match.startsWith("//") ? "//" : "/",
      );
    }
    const match = text.match(
      /^([A-Za-z][A-Za-z0-9+.-]*):(?:\/\/([^/?#]*))?([^?#]*)(?:\?([^#]*))?(?:#(.*))?$/,
    );
    if (!match) throw new TypeError("Invalid URL");
    this.protocol = `${match[1]}:`;
    this.host = match[2] ?? "";
    this.hostname = this.host.split(":")[0];
    this.pathname = match[3] || "/";
    this.search = match[4] === undefined ? "" : `?${match[4]}`;
    this.hash = match[5] === undefined ? "" : `#${match[5]}`;
    this.searchParams = new URLSearchParams(match[4] ?? "");
    // WHATWG keeps the empty authority for `file:` URLs (file:///path).
    const authority = this.host
      ? `//${this.host}`
      : this.protocol === "file:"
        ? "//"
        : "";
    this.href = `${this.protocol}${authority}${this.pathname}${this.search}${this.hash}`;
  }

  toString() {
    return this.href;
  }

  toJSON() {
    return this.href;
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
