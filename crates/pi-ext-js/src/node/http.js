// `node:http` - resolution-only shim.
//
// `pi-zed-models/src/browser-auth.ts:2` imports `http.createServer` at module
// top level for its OAuth redirect listener. The embedded host has no socket
// seam (no `net.listen` op), so this module exists to make that import
// resolve: it exposes the Node shape without pretending to listen. Every
// call that would need a real socket throws the same diagnostic the resolver
// used before this shim existed, so a caller never silently gets a dead
// server. A future host socket op can replace `Server.listen` without
// changing the importing extension.

const UNSUPPORTED =
  'Module "node:http" is not polyfilled by the extensions host; see crates/pi-ext-js/src/node for the supported set';

function unsupported() {
  throw new Error(UNSUPPORTED);
}

/** The subset of `http.Server` an importing extension can touch without a
 * socket: event registration and the (throwing) `listen` entry point. */
export class Server {
  constructor(listener) {
    this.listener = listener;
    this.events = new Map();
    this.listening = false;
  }

  on(event, handler) {
    const handlers = this.events.get(event) ?? [];
    handlers.push(handler);
    this.events.set(event, handlers);
    return this;
  }

  once(event, handler) {
    const wrapped = (...args) => {
      handler(...args);
      this.off(event, wrapped);
    };
    return this.on(event, wrapped);
  }

  off(event, handler) {
    const handlers = this.events.get(event) ?? [];
    const index = handlers.indexOf(handler);
    if (index >= 0) handlers.splice(index, 1);
    return this;
  }

  emit(event, ...args) {
    for (const handler of this.events.get(event) ?? []) handler(...args);
    return this.events.has(event);
  }

  // No host op binds a port, so starting the listener is unsupported.
  listen() {
    unsupported();
  }

  address() {
    return null;
  }

  close(callback) {
    this.listening = false;
    callback?.();
    return this;
  }

  setTimeout() {
    return this;
  }
}

export function createServer(listener) {
  return new Server(listener);
}

export function request() {
  unsupported();
}

export function get() {
  unsupported();
}

export const STATUS_CODES = {
  100: "Continue",
  200: "OK",
  201: "Created",
  204: "No Content",
  301: "Moved Permanently",
  302: "Found",
  400: "Bad Request",
  401: "Unauthorized",
  403: "Forbidden",
  404: "Not Found",
  500: "Internal Server Error",
  502: "Bad Gateway",
  503: "Service Unavailable",
};

export const METHODS = [
  "GET",
  "HEAD",
  "POST",
  "PUT",
  "PATCH",
  "DELETE",
  "CONNECT",
  "OPTIONS",
  "TRACE",
];

export class Agent {}

export default {
  Server,
  createServer,
  request,
  get,
  STATUS_CODES,
  METHODS,
  Agent,
};
