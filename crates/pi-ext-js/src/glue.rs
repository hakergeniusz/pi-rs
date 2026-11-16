//! The JavaScript side of the engine boundary.
//!
//! Three sources live here:
//!
//! 1. [`GLUE`] — the bootstrap evaluated once per host. It ports the JS
//!    halves of upstream's loading and dispatch machinery: the per-extension
//!    record with its `pi` API object (`createExtensionAPI`,
//!    `core/extensions/loader.ts:253-561` — including the exact registration
//!    error messages), the headless context hierarchy (`createContext`,
//!    `createToolContext`, `createCommandContext` and the `noOpUIContext`
//!    matrix, `runner.ts:331-362, 891-1041`), the dispatch-session
//!    protocol that lets the Rust runner invoke handlers and observe their
//!    in-place event mutations (`snapshotEventHandlers` +
//!    the `emit*` loops, `runner.ts` §2), the extension event bus
//!    (`core/event-bus.ts`), and the JS-side stashes for callables that
//!    cannot cross as JSON — the virtual-model `route` functions, the
//!    provider `streamSimple` handlers (`__piSessionCallRoute` /
//!    `__piSessionCallProviderStream`; `core/provider-composer.ts:588-613`,
//!    `core/extensions/types.ts:1989-2007`), and the provider oauth
//!    configs (`__piSessionCallOAuthLogin` + the one-shot refresh/api-key
//!    calls: `adaptOAuth`'s JS arm — `login` receives the adapter
//!    callbacks whose one-way notifications cross the `oauthNotify` op
//!    and whose prompt promises the host resolves token-verified through
//!    `oauthResolvePrompt`; `core/provider-composer.ts:358-377, 488-511`,
//!    `packages/ai/src/compat/extension-oauth-types.ts:39-47`), and the
//!    stream options' fn-valued instrumentation (`onPayload` suspends on
//!    a promise the host resolves with the `before_provider_request`
//!    chain's value, the one-way hooks queue through `providerHookEvent`,
//!    `signal` is a plain object the host flips — the handler contract is
//!    `custom-provider.md:195-198`), and the MCP tool bridge
//!    (`__piMcpRegisterTools` + the `mcpCallTool` op — the `pi.registerTool`
//!    calls the bundled `mcp` extension makes when a server connects,
//!    `index.ts:416-443`; the execute wrappers forward to the native
//!    manager, `mcp_manager.rs`).
//! 2. [`TYPEBOX_SHIM`] — the `typebox` virtual module. Upstream serves the
//!    real bundled `TypeBox` (`virtual-modules.ts`); the E1 shim reproduces
//!    the exact construction semantics of the builders extensions use
//!    (`TypeBox` 1.3.27 `Memory.Create`: enumerable keys are
//!    `{...schema, ...options}`, and `~kind`/`~optional` are attached as
//!    non-enumerable, so `JSON.stringify` output is byte-identical).
//! 3. [`DECLARATIONS_SHIM`] — the `@earendil-works/pi-codemode/declarations`
//!    virtual module: a verbatim port of `resolveSchemaRef`
//!    (`packages/codemode/src/declarations.ts:160-180`), the only binding
//!    the bundled `tool-search` extension imports from it.

/// The per-host bootstrap: state, contexts, sessions, bus, `pi` API.
pub const GLUE: &str = r#"
// A minimal console: QuickJS has none, and upstream's bus and loaders use
// console.error. `level` is "log" | "warn" | "error"; the host writes to
// stderr.
globalThis.console = {
  log: (...args) => globalThis.__piOps.print("log", args.map(String).join(" ")),
  warn: (...args) => globalThis.__piOps.print("warn", args.map(String).join(" ")),
  error: (...args) => globalThis.__piOps.print("error", args.map(String).join(" ")),
};

// `Intl.Segmenter` shim: QuickJS-NG ships no `Intl`, but the bundled
// tool-search constructs one at module top level. Upstream reaches the
// segmenter only for text outside its own ASCII fast path, so the shim
// approximates ICU word segmentation with Unicode property-escaped runs;
// the deviation is recorded in HANDOFF.md (E4 replaces this with a real
// Intl subset).
globalThis.Intl = {
  Segmenter: function () {
    this.segment = (text) =>
      (String(text).match(/[\p{L}\p{M}\p{N}]+/gu) || []).map((segment) => ({
        segment,
        isWordLike: true,
      }));
  },
};

// Runtime state. `staleMessage` implements the assertActive protocol
// (loader.ts:200-207, runner.ts:729-742); per-extension state is
// "loading" | "active" | "failed" (loader.ts:262-267); `bound` mirrors
// bindCore: action methods throw the not-initialized error until the
// runner binds (loader.ts:160-244).
globalThis.__piRuntime = {
  staleMessage: undefined,
  bound: false,
  pendingProviderRegistrations: [],
  pendingNativeProviderRegistrations: [],
  pendingVirtualModelRegistrations: [],
};

// The `route` functions of registered virtual models, keyed
// `provider\0id` (model-runtime.ts:97-182 `virtualModels`). Definitions
// cross to the host as JSON — which drops functions — so the callable stays
// here and the host invokes it per request through the routing protocol.
globalThis.__piVirtualModels = new Map();
// The `streamSimple` handlers of registered providers, keyed by provider
// name (model-runtime.ts:964-986 stores the function in `extensionProviders`
// — `core/extensions/types.ts:1989-2007` types it as the StreamFunction
// `(model, context, options?) => AssistantMessageEventStream`). Config
// crosses to the host as JSON — which drops functions — so the callable
// stays here and the host invokes it per request through the
// provider-stream protocol; the config JSON carries a `streamSimple: true`
// marker so the host knows the handler exists (the marker merges
// over-previous like the rest of the config, mirroring the stored
// function).
globalThis.__piProviderStreams = new Map();
globalThis.__piProviderOAuth = new Map();
globalThis.__piExtensions = [];
globalThis.__piExtState = [];

globalThis.__piAssertRuntimeActive = function () {
  if (globalThis.__piRuntime.staleMessage !== undefined) {
    throw new Error(globalThis.__piRuntime.staleMessage);
  }
};

globalThis.__piAssertActive = function (extIdx) {
  const state = globalThis.__piExtState[extIdx];
  if (state === "failed") {
    throw new Error(
      `Extension "${globalThis.__piExtensions[extIdx].path}" failed to load and its API is no longer active.`,
    );
  }
  globalThis.__piAssertRuntimeActive();
};

// The runner binds once loading completes: action methods stop throwing
// (the bindCore moment, runner.ts:417-551).
globalThis.__piBindRuntime = function () {
  globalThis.__piRuntime.bound = true;
};

globalThis.__piInvalidate = function (message) {
  if (globalThis.__piRuntime.staleMessage === undefined) {
    globalThis.__piRuntime.staleMessage =
      message ??
      "This extension ctx is stale after session replacement or reload. Do not use a captured pi or command ctx after ctx.newSession(), ctx.fork(), ctx.switchSession(), or ctx.reload(). For newSession, fork, and switchSession, move post-replacement work into withSession and use the ctx passed to withSession. For reload, do not use the old ctx after await ctx.reload().";
    globalThis.__piBus.clear();
  }
};

// Extension lifecycle (loader.ts:607-644): begin allocates the record,
// commit activates it, discard fails it and unsubscribes its bus handlers.
globalThis.__piBeginExtension = function (path) {
  globalThis.__piExtensions.push({
    path,
    events: {},
    tools: {},
    commands: {},
    flags: {},
    busSubscriptions: [],
    pendingChanges: [],
    // The virtual-model definitions this extension registered, for cleanup
    // when the record is discarded (the route functions live in
    // __piVirtualModels keyed by the definitions' provider/id).
    virtualModels: [],
    // The provider streamSimple handlers this extension registered, for
    // the same discard-time cleanup (they live in __piProviderStreams
    // keyed by provider name).
    providerStreams: [],
    // The provider oauth configs this extension registered, for the same
    // discard-time cleanup (they live in __piProviderOAuth keyed by
    // provider name).
    providerOauth: [],
  });
  globalThis.__piExtState.push("loading");
  return globalThis.__piExtensions.length - 1;
};

globalThis.__piCommitExtension = function (extIdx) {
  globalThis.__piExtState[extIdx] = "active";
  // Run the runtime changes deferred during loading (loader.ts:269-277):
  // a failed extension's changes die with its record.
  const pending = globalThis.__piExtensions[extIdx].pendingChanges ?? [];
  globalThis.__piExtensions[extIdx].pendingChanges = [];
  for (const change of pending) globalThis.__piRunProviderChange(extIdx, change);
};

// Provider/virtual-model registration deferral (loader.ts:221-239 plus
// runner.ts:470-521): while an extension is loading its changes queue on
// the extension record; after commit they queue on the runtime until
// bindCore; post-bind they apply immediately and throw into the caller.
globalThis.__piRunProviderChange = function (extIdx, change) {
  const extensionPath = globalThis.__piExtensions[extIdx].path;
  // MCP registrations apply immediately once the extension is active —
  // pre-bind without an event (the change listener is only set at
  // bindCore, `runner.ts:465`), post-bind through the change queue the
  // runner drains (`mcp_servers_change`, `runner.ts:465-466`).
  if (change.kind === "mcpRegister") {
    const error = globalThis.__piOps.mcpRegister(change.name, extensionPath, change.configJson);
    if (error !== "") throw new Error(error);
    return;
  }
  if (change.kind === "mcpUnregister") {
    globalThis.__piOps.mcpUnregister(change.name, extensionPath);
    return;
  }
  if (!globalThis.__piRuntime.bound) {
    if (change.kind === "provider") {
      globalThis.__piRuntime.pendingProviderRegistrations.push({
        name: change.name,
        configJson: change.configJson,
        extensionPath,
      });
    } else if (change.kind === "native") {
      globalThis.__piRuntime.pendingNativeProviderRegistrations.push({
        providerJson: change.providerJson,
        extensionPath,
      });
    } else if (change.kind === "virtual") {
      globalThis.__piRuntime.pendingVirtualModelRegistrations.push({
        definitionJson: change.definitionJson,
        extensionPath,
      });
    } else if (change.kind === "unregisterProvider") {
      globalThis.__piRuntime.pendingProviderRegistrations =
        globalThis.__piRuntime.pendingProviderRegistrations.filter(
          (entry) => entry.name !== change.name,
        );
      globalThis.__piRuntime.pendingNativeProviderRegistrations =
        globalThis.__piRuntime.pendingNativeProviderRegistrations.filter(
          (entry) => JSON.parse(entry.providerJson).id !== change.name,
        );
    } else {
      globalThis.__piRuntime.pendingVirtualModelRegistrations =
        globalThis.__piRuntime.pendingVirtualModelRegistrations.filter((entry) => {
          const definition = JSON.parse(entry.definitionJson);
          return definition.provider !== change.provider || definition.id !== change.id;
        });
    }
    return;
  }
  let error = "";
  if (change.kind === "provider") {
    error = globalThis.__piOps.providerRegister(change.name, extensionPath, change.configJson);
  } else if (change.kind === "native") {
    error = globalThis.__piOps.providerRegisterNative(change.providerJson, extensionPath);
  } else if (change.kind === "virtual") {
    error = globalThis.__piOps.virtualModelRegister(change.definitionJson, extensionPath);
  } else if (change.kind === "unregisterProvider") {
    globalThis.__piOps.providerUnregister(change.name);
  } else {
    globalThis.__piOps.virtualModelUnregister(change.provider, change.id);
  }
  if (error !== "") throw new Error(error);
};

globalThis.__piDrainPendingProviderQueues = function () {
  const drained = JSON.stringify({
    providers: globalThis.__piRuntime.pendingProviderRegistrations,
    nativeProviders: globalThis.__piRuntime.pendingNativeProviderRegistrations,
    virtualModels: globalThis.__piRuntime.pendingVirtualModelRegistrations,
  });
  globalThis.__piRuntime.pendingProviderRegistrations = [];
  globalThis.__piRuntime.pendingNativeProviderRegistrations = [];
  globalThis.__piRuntime.pendingVirtualModelRegistrations = [];
  return drained;
};

globalThis.__piDiscardExtension = function (extIdx) {
  globalThis.__piExtState[extIdx] = "failed";
  const ext = globalThis.__piExtensions[extIdx];
  for (const unsubscribe of ext.busSubscriptions) unsubscribe();
  // A failed extension's virtual models die with its record (the deferred
  // registration never applies — loader.ts:269-277). Only the entries this
  // extension still owns are removed, so a re-registration by a live
  // extension survives.
  for (const entry of ext.virtualModels ?? []) {
    if (globalThis.__piVirtualModels.get(entry.key) === entry.definition) {
      globalThis.__piVirtualModels.delete(entry.key);
    }
  }
  // The same ownership rule for provider streamSimple handlers: only the
  // entries this extension still owns are removed, so a re-registration by
  // a live extension survives.
  for (const entry of ext.providerStreams ?? []) {
    if (globalThis.__piProviderStreams.get(entry.name) === entry.streamSimple) {
      globalThis.__piProviderStreams.delete(entry.name);
    }
  }
  // The same ownership rule for provider oauth configs.
  for (const entry of ext.providerOauth ?? []) {
    if (globalThis.__piProviderOAuth.get(entry.name) === entry.oauth) {
      globalThis.__piProviderOAuth.delete(entry.name);
    }
  }
  globalThis.__piExtensions[extIdx] = { path: ext.path, events: {}, tools: {}, commands: {}, flags: {}, busSubscriptions: [] };
};

// The extension event bus (core/event-bus.ts:12-33): handlers are wrapped
// so a throw reports through console.error instead of breaking emit, and
// emit order is subscription order.
function createBus() {
  const channels = new Map();
  return {
    emit(channel, data) {
      const handlers = channels.get(channel);
      if (handlers === undefined) return;
      for (const handler of [...handlers]) {
        Promise.resolve()
          .then(() => handler(data))
          .catch((error) =>
            globalThis.console.error(`Event handler error (${channel}):`, error),
          );
      }
    },
    on(channel, handler) {
      const list = channels.get(channel) ?? [];
      channels.set(channel, [...list, handler]);
      return () => {
        const current = channels.get(channel) ?? [];
        const index = current.indexOf(handler);
        if (index !== -1) {
          const next = current.slice();
          next.splice(index, 1);
          channels.set(channel, next);
        }
      };
    },
    clear() {
      channels.clear();
    },
  };
}

globalThis.__piBus = createBus();

// `exec` (core/exec.ts): the Rust op runs the process synchronously and
// returns the ExecResult JSON; a validation failure throws like upstream's
// RangeError.
globalThis.__piExec = function (command, args, options) {
  const raw = globalThis.__piOps.exec(
    command,
    JSON.stringify(args ?? []),
    JSON.stringify(options ?? null),
  );
  const result = JSON.parse(raw);
  if (result.execError !== undefined) throw new Error(result.execError);
  return Promise.resolve(result);
};

// Registration error messages are verbatim from createExtensionAPI
// (loader.ts:298-358).
globalThis.__piCreateApi = function (extIdx) {
  const ext = globalThis.__piExtensions[extIdx];
  const assertActive = () => globalThis.__piAssertActive(extIdx);
  // Action methods delegate to the shared runtime, which throws until the
  // runner binds (createExtensionRuntime, loader.ts:160-244).
  const assertBound = () => {
    if (!globalThis.__piRuntime.bound) {
      throw new Error(
        "Extension runtime not initialized. Action methods cannot be called during extension loading.",
      );
    }
  };
  const assertAction = () => {
    assertActive();
    assertBound();
  };
  // Registration changes defer while this extension loads
  // (loader.ts:269-272) and run immediately afterwards.
  const applyProviderChange = (kind, payload) => {
    if (globalThis.__piExtState[extIdx] === "loading") {
      ext.pendingChanges.push({ kind, ...payload });
    } else {
      globalThis.__piRunProviderChange(extIdx, { kind, ...payload });
    }
  };
  return {
    on(event, handler) {
      assertActive();
      const list = ext.events[event] ?? [];
      list.push(handler);
      ext.events[event] = list;
      return () => {
        const handlers = ext.events[event];
        if (handlers === undefined) return;
        const index = handlers.indexOf(handler);
        if (index === -1) return;
        handlers.splice(index, 1);
        if (handlers.length === 0) delete ext.events[event];
      };
    },

    registerTool(tool) {
      assertActive();
      if (
        typeof tool.parameters !== "object" ||
        tool.parameters === null ||
        Array.isArray(tool.parameters)
      ) {
        throw new Error(
          `Tool "${tool.name}" registered by extension "${ext.path}" must define an object parameter schema.`,
        );
      }
      ext.tools[tool.name] = tool;
      globalThis.__piOps.refreshTools();
    },

    registerCommand(name, options) {
      assertActive();
      if (typeof name !== "string" || name.length === 0) {
        throw new Error(
          `Command registered by extension "${ext.path}" must have a non-empty string name. Use pi.registerCommand("name", { description, handler }).`,
        );
      }
      if (options === undefined || typeof options.handler !== "function") {
        throw new Error(
          `Command "/${name}" registered by extension "${ext.path}" must define handler().`,
        );
      }
      ext.commands[name] = { name, description: options.description, handler: options.handler };
    },

    registerFlag(name, options) {
      assertActive();
      if (options.default !== undefined && typeof options.default !== options.type) {
        throw new Error(
          `Invalid default for flag "${name}": expected ${options.type}, got ${typeof options.default}`,
        );
      }
      ext.flags[name] = {
        name,
        description: options.description,
        type: options.type,
        default: options.default,
      };
      if (options.default !== undefined) {
        globalThis.__piOps.flagDefault(name, JSON.stringify(options.default));
      }
    },

    getFlag(name) {
      assertActive();
      if (!(name in ext.flags)) return undefined;
      const raw = globalThis.__piOps.getFlagValue(name);
      return raw === "" ? undefined : JSON.parse(raw);
    },

    registerShortcut() {
      throw new Error("pi.registerShortcut is not available in the E1 headless extensions core (E3)");
    },
    registerMessageRenderer() {
      throw new Error("pi.registerMessageRenderer is not available in the E1 headless extensions core (E3)");
    },
    registerMarkdownTransformer() {
      throw new Error("pi.registerMarkdownTransformer is not available in the E1 headless extensions core (E3)");
    },
    registerEntryRenderer() {
      throw new Error("pi.registerEntryRenderer is not available in the E1 headless extensions core (E3)");
    },
    registerToolRenderer() {
      throw new Error("pi.registerToolRenderer is not available in the E1 headless extensions core (E3)");
    },
    registerProvider() {
      throw new Error("pi.registerProvider is not available in the E1 headless extensions core (E2)");
    },
    unregisterProvider() {
      throw new Error("pi.unregisterProvider is not available in the E1 headless extensions core (E2)");
    },
    registerVirtualModel() {
      throw new Error("pi.registerVirtualModel is not available in the E1 headless extensions core (E2)");
    },
    unregisterVirtualModel() {
      throw new Error("pi.unregisterVirtualModel is not available in the E1 headless extensions core (E2)");
    },
    setModel() {
      throw new Error("pi.setModel is not available in the E1 headless extensions core (E2)");
    },

    sendMessage(message, options) {
      assertAction();
      globalThis.__piOps.sendMessage(JSON.stringify(message), JSON.stringify(options ?? null));
    },
    sendUserMessage(content, options) {
      assertAction();
      globalThis.__piOps.sendUserMessage(JSON.stringify(content), JSON.stringify(options ?? null));
    },
    appendEntry(customType, data) {
      assertAction();
      globalThis.__piOps.appendEntry(customType, JSON.stringify(data ?? null));
    },
    setSessionName(name) {
      assertAction();
      globalThis.__piOps.setSessionName(name);
    },
    getSessionName() {
      assertAction();
      const name = globalThis.__piOps.getSessionName();
      return name === "" ? undefined : name;
    },
    setLabel(entryId, label) {
      assertAction();
      globalThis.__piOps.setLabel(entryId, label === undefined ? "" : label);
    },

    exec: (command, args, options) => {
      assertActive();
      return globalThis.__piExec(command, args, options);
    },

    getAllTools() {
      assertAction();
      return JSON.parse(globalThis.__piOps.getAllTools());
    },
    getActiveTools() {
      assertAction();
      return JSON.parse(globalThis.__piOps.getActiveTools());
    },
    setActiveTools(toolNames) {
      assertAction();
      globalThis.__piOps.setActiveTools(JSON.stringify(toolNames ?? []));
    },
    getSettings() {
      assertAction();
      return JSON.parse(globalThis.__piOps.getSettings());
    },
    getCommands() {
      assertAction();
      return JSON.parse(globalThis.__piOps.getCommands());
    },
    getThinkingLevel() {
      assertAction();
      const level = globalThis.__piOps.getThinkingLevel();
      return level === "" ? undefined : level;
    },
    setThinkingLevel(level) {
      assertAction();
      globalThis.__piOps.setThinkingLevel(level === undefined ? "" : level);
    },

    registerMcpServer(name, config) {
      assertActive();
      const validationError = globalThis.__piOps.validateMcpServer(name, JSON.stringify(config ?? null));
      if (validationError !== "") {
        throw new Error(`Invalid MCP server registered by extension "${ext.path}": ${validationError}`);
      }
      // Ownership and namespace clashes throw into the caller immediately
      // (loader.ts:489-504); the registration itself defers while this
      // extension loads like the other runtime changes
      // (applyRuntimeChange, loader.ts:269-272, 499), so a failed load
      // drops it.
      const ownershipError = globalThis.__piOps.mcpOwnership(name, ext.path);
      if (ownershipError !== "") throw new Error(ownershipError);
      applyProviderChange("mcpRegister", { name, configJson: JSON.stringify(config ?? null) });
    },
    unregisterMcpServer(name) {
      assertActive();
      applyProviderChange("mcpUnregister", { name });
    },
    getMcpServers() {
      assertActive();
      return JSON.parse(globalThis.__piOps.getMcpServers());
    },

    registerProvider(providerOrName, config) {
      assertActive();
      if (typeof providerOrName === "string") {
        if (!config) {
          throw new Error("Provider config is required when registering by name");
        }
        // validateExtensionProvider (provider-composer.ts:544-548): the
        // check throws into the extension before anything is stored, so a
        // broken re-registration leaves the stored config untouched
        // (model-runtime.ts:964-968).
        if (typeof config.streamSimple === "function" && !config.api) {
          throw new Error(
            `Provider ${providerOrName}: "api" is required when registering streamSimple.`,
          );
        }
        // `streamSimple` is a function and cannot cross as JSON: stash it
        // (the map replaces on re-registration like model-runtime.ts's
        // per-key merge; a re-registration without the key keeps the
        // previous handler), record ownership for discard-time cleanup,
        // then send the serializable shape with a `true` marker so the
        // host knows the handler exists.
        if (typeof config.streamSimple === "function") {
          globalThis.__piProviderStreams.set(providerOrName, config.streamSimple);
          const owned = ext.providerStreams.find((entry) => entry.name === providerOrName);
          if (owned === undefined) {
            ext.providerStreams.push({ name: providerOrName, streamSimple: config.streamSimple });
          } else {
            owned.streamSimple = config.streamSimple;
          }
        }
        // The oauth config's functions (login/refreshToken/getApiKey)
        // cannot cross as JSON: stash the whole config object keyed by
        // provider (the map replaces on re-registration like the
        // streamSimple merge) and send a `true` marker; the metadata
        // subset (name/isSubscription) rides the config as before.
        const hasOauthFns =
          typeof config.oauth === "object" &&
          config.oauth !== null &&
          typeof config.oauth.login === "function";
        if (hasOauthFns) {
          globalThis.__piProviderOAuth.set(providerOrName, config.oauth);
          const owned = ext.providerOauth.find((entry) => entry.name === providerOrName);
          if (owned === undefined) {
            ext.providerOauth.push({ name: providerOrName, oauth: config.oauth });
          } else {
            owned.oauth = config.oauth;
          }
        }
        applyProviderChange("provider", {
          name: providerOrName,
          // The top-level `streamSimple` function crosses as a `true`
          // presence marker (a shallow copy keeps nested values verbatim —
          // nested functions drop like JSON.stringify always did). The
          // oauth object crosses verbatim: JSON.stringify drops its
          // function keys naturally, so the metadata subset (name,
          // isSubscription) still rides to the host, and login-ability is
          // a runner query over the stash.
          configJson: JSON.stringify({
            ...config,
            streamSimple:
              typeof config.streamSimple === "function" ? true : config.streamSimple,
          }),
        });
        return;
      }
      applyProviderChange("native", { providerJson: JSON.stringify(providerOrName ?? null) });
    },

    unregisterProvider(name) {
      assertActive();
      const provider = String(name);
      globalThis.__piProviderStreams.delete(provider);
      ext.providerStreams = ext.providerStreams.filter((entry) => entry.name !== provider);
      globalThis.__piProviderOAuth.delete(provider);
      ext.providerOauth = ext.providerOauth.filter((entry) => entry.name !== provider);
      applyProviderChange("unregisterProvider", { name: provider });
    },

    registerVirtualModel(model) {
      assertActive();
      // `route` is a function and cannot cross as JSON: stash the definition
      // (the map replaces on re-registration, like model-runtime.ts's
      // `virtualModels`), record ownership for discard-time cleanup, then
      // send the serializable shape.
      const definition = model ?? null;
      if (definition !== null && typeof definition === "object") {
        const key = `${definition.provider}\u0000${definition.id}`;
        globalThis.__piVirtualModels.set(key, definition);
        ext.virtualModels.push({ key, definition });
      }
      applyProviderChange("virtual", { definitionJson: JSON.stringify(definition) });
    },

    unregisterVirtualModel(provider, id) {
      assertActive();
      const key = `${String(provider)}\u0000${String(id)}`;
      globalThis.__piVirtualModels.delete(key);
      ext.virtualModels = ext.virtualModels.filter((entry) => entry.key !== key);
      applyProviderChange("unregisterVirtual", { provider: String(provider), id: String(id) });
    },

    events: {
      emit(channel, data) {
        assertActive();
        globalThis.__piBus.emit(channel, data);
      },
      on(channel, handler) {
        assertActive();
        const unsubscribe = globalThis.__piBus.on(channel, handler);
        ext.busSubscriptions.push(unsubscribe);
        return unsubscribe;
      },
    },
  };
};

// The UI context. Headless (no bound UI — runner.ts:331-362
// `noOpUIContext`, §4.6 row 1 of docs/EXTENSIONS.md): notify reaches the
// host hook, dialogs resolve to their "no UI" answers, setters are
// no-ops. With a bound UI (E2, mode "rpc" — the surface of
// `modes/rpc/rpc-mode.ts` `createExtensionUIContext`): dialog methods
// queue a request through the `uiRequest` op and return a promise that
// the host settles via `__piUiResolve`; fire-and-forget methods queue a
// request with no pending promise; terminal-only methods keep their RPC
// limitations (docs/rpc-extension-ui.md). The mode is read at call time,
// so binding a UI affects contexts created earlier.
globalThis.__piUiPending = new Map();
globalThis.__piUiResolve = function (id, payloadJson) {
  const entry = globalThis.__piUiPending.get(id);
  if (entry === undefined) return;
  globalThis.__piUiPending.delete(id);
  let payload;
  try {
    payload = payloadJson === "" ? undefined : JSON.parse(payloadJson);
  } catch {
    payload = undefined;
  }
  entry(payload);
};

globalThis.__piCreateUiCtx = function () {
  const assertActive = () => globalThis.__piAssertRuntimeActive();
  const hasUi = () => globalThis.__piOps.hasUi() === "true";
  // The response parse mirrors upstream's per-method `parseResponse`
  // (rpc-mode.ts:118-129): `{cancelled:true}` falls to the method
  // default, `{value}`/`{confirmed}` carry the answer, `{}` (the
  // sink-side timeout default) falls through too.
  const parseValue = (payload) => {
    if (payload && payload.cancelled) return undefined;
    if (payload && "value" in payload) return payload.value;
    return undefined;
  };
  const parseConfirm = (payload) => {
    if (payload && payload.cancelled) return false;
    if (payload && "confirmed" in payload) return payload.confirmed;
    return false;
  };
  // One queued request: `{method, ...fields}` — dialog methods wait on
  // the returned id, fire-and-forget calls never do.
  const queue = (method, fields, wait) => {
    const id = globalThis.__piOps.uiRequest(JSON.stringify(Object.assign({ method }, fields)));
    if (!wait) return undefined;
    return new Promise((resolve) => {
      globalThis.__piUiPending.set(id, resolve);
    });
  };

  const headless = {
    select: () => {
      assertActive();
      return Promise.resolve(undefined);
    },
    confirm: () => {
      assertActive();
      return Promise.resolve(false);
    },
    input: () => {
      assertActive();
      return Promise.resolve(undefined);
    },
    editor: () => {
      assertActive();
      return Promise.resolve(undefined);
    },
    custom: () => {
      assertActive();
      return Promise.resolve(undefined);
    },
    notify: (message) => {
      assertActive();
      globalThis.__piOps.notify(typeof message === "string" ? message : JSON.stringify(message));
    },
    onTerminalInput: () => {
      assertActive();
      return () => {};
    },
    setStatus: () => {
      assertActive();
    },
    setWorkingMessage: () => {
      assertActive();
    },
    setWorkingVisible: () => {
      assertActive();
    },
    setWorkingIndicator: () => {
      assertActive();
    },
    setHiddenThinkingLabel: () => {
      assertActive();
    },
    setWidget: () => {
      assertActive();
    },
    setFooter: () => {
      assertActive();
    },
    setHeader: () => {
      assertActive();
    },
    setTitle: () => {
      assertActive();
    },
    pasteToEditor: () => {
      assertActive();
    },
    setEditorText: () => {
      assertActive();
    },
    getEditorText: () => {
      assertActive();
      return "";
    },
    addAutocompleteProvider: () => {
      assertActive();
    },
    setTheme: () => {
      assertActive();
      return { success: false, error: "UI not available" };
    },
    getAllThemes: () => {
      assertActive();
      return [];
    },
    getTheme: () => {
      assertActive();
      return undefined;
    },
    getToolsExpanded: () => {
      assertActive();
      return false;
    },
    setToolsExpanded: () => {
      assertActive();
    },
  };

  const live = {
    select: (title, options, opts) => {
      assertActive();
      const fields = { title, options };
      if (opts && opts.timeout !== undefined) fields.timeout = opts.timeout;
      return queue("select", fields, true).then(parseValue);
    },
    confirm: (title, message, opts) => {
      assertActive();
      const fields = { title, message };
      if (opts && opts.timeout !== undefined) fields.timeout = opts.timeout;
      return queue("confirm", fields, true).then(parseConfirm);
    },
    input: (title, placeholder, opts) => {
      assertActive();
      const fields = { title };
      if (placeholder !== undefined) fields.placeholder = placeholder;
      if (opts && opts.timeout !== undefined) fields.timeout = opts.timeout;
      return queue("input", fields, true).then(parseValue);
    },
    editor: (title, prefill) => {
      assertActive();
      const fields = { title };
      if (prefill !== undefined) fields.prefill = prefill;
      return queue("editor", fields, true).then(parseValue);
    },
    custom: () => {
      // RPC mode answers custom immediately; the request still crosses
      // so the runner emits its ui_prompt pair in upstream's order.
      assertActive();
      return queue("custom", {}, true);
    },
    notify: (message, notifyType) => {
      assertActive();
      const fields = { message };
      if (notifyType !== undefined) fields.notifyType = notifyType;
      queue("notify", fields, false);
    },
    onTerminalInput: () => {
      assertActive();
      return () => {};
    },
    setStatus: (statusKey, statusText) => {
      assertActive();
      const fields = { statusKey };
      if (statusText !== undefined) fields.statusText = statusText;
      queue("setStatus", fields, false);
    },
    setWorkingMessage: () => {
      assertActive();
    },
    setWorkingVisible: () => {
      assertActive();
    },
    setWorkingIndicator: () => {
      assertActive();
    },
    setHiddenThinkingLabel: () => {
      assertActive();
    },
    setWidget: (widgetKey, widgetLines, options) => {
      assertActive();
      // Component factories are not supported in RPC mode.
      if (widgetLines === undefined || Array.isArray(widgetLines)) {
        const fields = { widgetKey, widgetLines };
        if (options && options.placement !== undefined) fields.widgetPlacement = options.placement;
        queue("setWidget", fields, false);
      }
    },
    setFooter: () => {
      assertActive();
    },
    setHeader: () => {
      assertActive();
    },
    setTitle: (title) => {
      assertActive();
      queue("setTitle", { title }, false);
    },
    pasteToEditor: (text) => {
      assertActive();
      live.setEditorText(text);
    },
    setEditorText: (text) => {
      assertActive();
      queue("set_editor_text", { text }, false);
    },
    getEditorText: () => {
      assertActive();
      return "";
    },
    addAutocompleteProvider: () => {
      assertActive();
    },
    setEditorComponent: () => {
      assertActive();
    },
    getEditorComponent: () => {
      assertActive();
      return undefined;
    },
    setTheme: () => {
      assertActive();
      return { success: false, error: "Theme switching not supported in RPC mode" };
    },
    getAllThemes: () => {
      assertActive();
      return [];
    },
    getTheme: () => {
      assertActive();
      return undefined;
    },
    getToolsExpanded: () => {
      assertActive();
      return false;
    },
    setToolsExpanded: () => {
      assertActive();
    },
  };

  // One stable surface; the branch happens per call so a later
  // `set_ui_context` affects contexts created before it.
  const pick = () => (hasUi() ? live : headless);
  return {
    select: (title, options, opts) => pick().select(title, options, opts),
    confirm: (title, message, opts) => pick().confirm(title, message, opts),
    input: (title, placeholder, opts) => pick().input(title, placeholder, opts),
    editor: (title, prefill) => pick().editor(title, prefill),
    custom: (factory, options) => pick().custom(factory, options),
    notify: (message, notifyType) => pick().notify(message, notifyType),
    onTerminalInput: (handler) => pick().onTerminalInput(handler),
    setStatus: (key, text) => pick().setStatus(key, text),
    setWorkingMessage: (message) => pick().setWorkingMessage(message),
    setWorkingVisible: (visible) => pick().setWorkingVisible(visible),
    setWorkingIndicator: (options) => pick().setWorkingIndicator(options),
    setHiddenThinkingLabel: (label) => pick().setHiddenThinkingLabel(label),
    setWidget: (key, content, options) => pick().setWidget(key, content, options),
    setFooter: (factory) => pick().setFooter(factory),
    setHeader: (factory) => pick().setHeader(factory),
    setTitle: (title) => pick().setTitle(title),
    pasteToEditor: (text) => pick().pasteToEditor(text),
    setEditorText: (text) => pick().setEditorText(text),
    getEditorText: () => pick().getEditorText(),
    addAutocompleteProvider: (...args) => pick().addAutocompleteProvider(...args),
    setEditorComponent: (...args) => pick().setEditorComponent(...args),
    getEditorComponent: () => pick().getEditorComponent(),
    setTheme: (theme) => pick().setTheme(theme),
    getAllThemes: () => pick().getAllThemes(),
    getTheme: (name) => pick().getTheme(name),
    getToolsExpanded: () => pick().getToolsExpanded(),
    setToolsExpanded: (expanded) => pick().setToolsExpanded(expanded),
  };
};

// The base ExtensionContext (runner.ts:891-969). Values resolve at call
// time through the ops, so host bindings apply whenever the handler reads
// them; staleness is the runtime-level check (runner.assertActive,
// runner.ts:738-742), and `signal` is undefined in E1 (no host abort
// signal crosses the engine boundary yet — deviation in HANDOFF.md).
globalThis.__piCreateCtx = function () {
  const assertActive = () => globalThis.__piAssertRuntimeActive();
  return {
    ui: globalThis.__piCreateUiCtx(assertActive),
    get mode() {
      assertActive();
      return globalThis.__piOps.mode();
    },
    get hasUI() {
      assertActive();
      return globalThis.__piOps.hasUi() === "true";
    },
    get cwd() {
      assertActive();
      return globalThis.__piOps.cwd();
    },
    get model() {
      assertActive();
      const raw = globalThis.__piOps.getModel();
      return raw === "" ? undefined : JSON.parse(raw);
    },
    get scopedModels() {
      assertActive();
      return [];
    },
    get thinkingLevel() {
      assertActive();
      const level = globalThis.__piOps.getThinkingLevel();
      return level === "" ? undefined : level;
    },
    isIdle: () => {
      assertActive();
      return globalThis.__piOps.isIdle();
    },
    isProjectTrusted: () => {
      assertActive();
      return globalThis.__piOps.isProjectTrusted();
    },
    get signal() {
      assertActive();
      return undefined;
    },
    abort: () => {
      assertActive();
      globalThis.__piOps.abort();
    },
    hasPendingMessages: () => {
      assertActive();
      return globalThis.__piOps.hasPendingMessages();
    },
    shutdown: () => {
      assertActive();
      globalThis.__piOps.shutdown();
    },
    getContextUsage: () => {
      assertActive();
      const raw = globalThis.__piOps.getContextUsage();
      return raw === "" ? undefined : JSON.parse(raw);
    },
    compact: (options) => {
      assertActive();
      globalThis.__piOps.compact(JSON.stringify(options ?? null));
    },
    getSystemPrompt: () => {
      assertActive();
      return globalThis.__piOps.getSystemPrompt();
    },
  };
};

// The command context additions (runner.ts:1004-1041) with the unbound
// defaults from bindCommandContext (runner.ts:553-570): waiting is
// immediate and the session-mutation handlers report "not cancelled".
globalThis.__piCreateCommandCtx = function () {
  const base = globalThis.__piCreateCtx();
  base.getSystemPromptOptions = () => {
    globalThis.__piAssertRuntimeActive();
    return JSON.parse(globalThis.__piOps.getSystemPromptOptions());
  };
  base.waitForIdle = () => {
    globalThis.__piAssertRuntimeActive();
    return Promise.resolve();
  };
  base.newSession = () => {
    globalThis.__piAssertRuntimeActive();
    return Promise.resolve({ cancelled: false });
  };
  base.fork = () => {
    globalThis.__piAssertRuntimeActive();
    return Promise.resolve({ cancelled: false });
  };
  base.navigateTree = () => {
    globalThis.__piAssertRuntimeActive();
    return Promise.resolve({ cancelled: false });
  };
  base.switchSession = () => {
    globalThis.__piAssertRuntimeActive();
    return Promise.resolve({ cancelled: false });
  };
  base.reload = () => {
    globalThis.__piAssertRuntimeActive();
    return Promise.resolve();
  };
  return base;
};

// The reduced ProjectTrustContext (`types.ts:448-458` via
// emitProjectTrustEvent, runner.ts:302-329): cwd, mode, hasUI, and a
// notify-capable ui — nothing else.
globalThis.__piCreateTrustCtx = function () {
  const assertActive = () => globalThis.__piAssertRuntimeActive();
  // `ProjectTrustContext.ui` is `Pick<ExtensionUIContext, "select" |
  // "confirm" | "input" | "notify">` (types.ts:733-738): the same
  // dispatch surface as the base context, restricted to those four, so
  // trust handlers can prompt in UI mode.
  const full = globalThis.__piCreateUiCtx();
  return {
    get cwd() {
      assertActive();
      return globalThis.__piOps.cwd();
    },
    get mode() {
      assertActive();
      return globalThis.__piOps.mode();
    },
    get hasUI() {
      assertActive();
      return globalThis.__piOps.hasUi() === "true";
    },
    ui: {
      select: (title, options, opts) => full.select(title, options, opts),
      confirm: (title, message, opts) => full.confirm(title, message, opts),
      input: (title, placeholder, opts) => full.input(title, placeholder, opts),
      notify: (message, notifyType) => full.notify(message, notifyType),
    },
  };
};

// Error reporting matches emitError's message/stack pair
// (runner.ts:1119-1127).
globalThis.__piErrorText = function (error) {
  if (error instanceof Error) {
    return JSON.stringify({ message: error.message, stack: error.stack ?? null });
  }
  return JSON.stringify({ message: String(error), stack: null });
};

// Dispatch sessions. One session = one runner dispatch: the event object
// stays live across handlers so in-place mutations chain (§6.3 of
// docs/EXTENSIONS.md), and results land in a slot the Rust runner reads
// after draining the job queue. `kind` selects the context:
// "event" | "command" | "tool" | "trust" | "route" | "providerStream".
globalThis.__piSessions = new Map();
globalThis.__piSessionSeq = 0;

globalThis.__piSessionBegin = function (eventJson, kind, toolCallId) {
  const id = ++globalThis.__piSessionSeq;
  let ctx;
  if (kind === "command") ctx = globalThis.__piCreateCommandCtx();
  else if (kind === "tool") ctx = globalThis.__piCreateToolCtx(toolCallId);
  else if (kind === "trust") ctx = globalThis.__piCreateTrustCtx();
  else if (kind === "route") ctx = globalThis.__piCreateCtx();
  else ctx = globalThis.__piCreateCtx();
  globalThis.__piSessions.set(id, {
    event: JSON.parse(eventJson),
    ctx,
    done: false,
    hasValue: false,
    value: undefined,
    error: undefined,
  });
  return id;
};

globalThis.__piSessionSetEvent = function (id, eventJson) {
  const session = globalThis.__piSessions.get(id);
  session.event = JSON.parse(eventJson);
};

globalThis.__piSessionEvent = function (id) {
  const session = globalThis.__piSessions.get(id);
  try {
    return JSON.stringify(session.event);
  } catch (error) {
    return JSON.stringify({ __sessionError: `event is not JSON-serializable: ${error.message}` });
  }
};

globalThis.__piSessionPatch = function (id, key, valueJson) {
  const session = globalThis.__piSessions.get(id);
  session.event[key] = JSON.parse(valueJson);
};

// The user_bash outcome validation (isUserBashEventResult +
// emitUserBash's throw, runner.ts:1281-1290): invalid results become the
// session error (the runner propagates them); `{operations}` results
// stash the live operations object for `call_bash_exec` and cross as
// `{operations:true}`.
globalThis.__piUserBashInvalidMessage =
  "Invalid user_bash handler result: return undefined for local execution or exactly one valid { operations } or { result } object";
globalThis.__piValidateUserBash = function (value) {
  if (value === undefined) return { value: undefined };
  if (typeof value !== "object" || value === null) {
    return { error: { message: globalThis.__piUserBashInvalidMessage } };
  }
  const hasOperations = value.operations !== undefined;
  const hasResult = value.result !== undefined;
  if (hasOperations === hasResult) {
    return { error: { message: globalThis.__piUserBashInvalidMessage } };
  }
  if (hasOperations) {
    const operations = value.operations;
    if (
      typeof operations !== "object" ||
      operations === null ||
      typeof operations.exec !== "function"
    ) {
      return { error: { message: globalThis.__piUserBashInvalidMessage } };
    }
    globalThis.__piBashOperations = operations;
    return { value: { operations: true } };
  }
  const result = value.result;
  const ok =
    typeof result === "object" &&
    result !== null &&
    typeof result.output === "string" &&
    "exitCode" in result &&
    (result.exitCode === undefined || typeof result.exitCode === "number") &&
    typeof result.cancelled === "boolean" &&
    typeof result.truncated === "boolean" &&
    (result.fullOutputPath === undefined || typeof result.fullOutputPath === "string");
  if (!ok) {
    return { error: { message: globalThis.__piUserBashInvalidMessage } };
  }
  return { value: value };
};

globalThis.__piSessionSettle = function (session, outcome) {
  session.done = true;
  if (outcome.error !== undefined) {
    session.error = outcome.error;
    session.hasValue = false;
    session.value = undefined;
    return;
  }
  if (session.eventName === "user_bash") {
    outcome = globalThis.__piValidateUserBash(outcome.value);
    if (outcome.error !== undefined) {
      // Slot errors are JSON strings (the shape __piErrorText produces);
      // __piSessionLast parses them back into objects.
      session.error = JSON.stringify({ message: outcome.error.message, stack: null });
      session.hasValue = false;
      session.value = undefined;
      return;
    }
  }
  session.error = undefined;
  if (outcome.value === undefined) {
    session.hasValue = false;
    session.value = undefined;
    return;
  }
  session.hasValue = true;
  session.value = outcome.value;
};

globalThis.__piSessionCall = function (id, extIdx, event, handlerIdx) {
  const session = globalThis.__piSessions.get(id);
  session.eventName = event;
  const handlers = globalThis.__piExtensions[extIdx].events[event];
  const handler = handlers === undefined ? undefined : handlers[handlerIdx];
  Promise.resolve()
    .then(() => handler(session.event, session.ctx))
    .then(
      (result) => globalThis.__piSessionSettle(session, { value: result }),
      (error) => globalThis.__piSessionSettle(session, { error: globalThis.__piErrorText(error) }),
    );
};

globalThis.__piSessionCallCommand = function (id, extIdx, name, args) {
  const session = globalThis.__piSessions.get(id);
  session.eventName = `command:${name}`;
  const command = globalThis.__piExtensions[extIdx].commands[name];
  Promise.resolve()
    .then(() => command.handler(args, session.ctx))
    .then(
      () => globalThis.__piSessionSettle(session, { value: undefined }),
      (error) => globalThis.__piSessionSettle(session, { error: globalThis.__piErrorText(error) }),
    );
};

globalThis.__piSessionCallTool = function (id, extIdx, name, toolCallId) {
  const session = globalThis.__piSessions.get(id);
  session.eventName = `tool:${name}`;
  const argsJson = JSON.stringify(session.event);
  const tool = globalThis.__piExtensions[extIdx].tools[name];
  const onUpdate = (update) => {
    globalThis.__piOps.toolUpdate(toolCallId, JSON.stringify(update ?? null));
  };
  Promise.resolve()
    .then(() => tool.execute(toolCallId, session.event, undefined, onUpdate, session.ctx))
    .then(
      (result) => globalThis.__piSessionSettle(session, { value: result }),
      (error) => globalThis.__piSessionSettle(session, { error: globalThis.__piErrorText(error) }),
    );
};

// The virtual-model routing call (`ModelRuntime.resolveModel` →
// `virtual.route(request, ctx)`, model-runtime.ts:1059-1065): the host
// built the request (model, reason, previous, failed, state, messages) and
// the stashed route function answers with `{model, thinkingLevel, state?}`.
globalThis.__piSessionCallRoute = function (id, provider, modelId) {
  const session = globalThis.__piSessions.get(id);
  const definition = globalThis.__piVirtualModels.get(`${provider}\u0000${modelId}`);
  if (definition === undefined || typeof definition.route !== "function") {
    // Slot errors are JSON strings (the shape __piErrorText produces);
    // __piSessionLast parses them back into objects.
    globalThis.__piSessionSettle(session, {
      error: JSON.stringify({
        message: `Virtual model ${provider}/${modelId} is not registered.`,
        stack: null,
      }),
    });
    return;
  }
  session.eventName = "route";
  Promise.resolve()
    .then(() => definition.route(session.event, session.ctx))
    .then(
      (result) => globalThis.__piSessionSettle(session, { value: result }),
      (error) => globalThis.__piSessionSettle(session, { error: globalThis.__piErrorText(error) }),
    );
};

// The provider streamSimple call (`streamWith`'s extension arm,
// provider-composer.ts:588-613 → `extension.streamSimple(model, context,
// options)`): the host built the marshalled request — `{model, context,
// options}` rides the session event — and the stashed handler answers with
// an AssistantMessageEventStream (`core/extensions/types.ts:1989-2007`,
// `packages/ai` `StreamFunction`). The glue drives the iterable inside the
// engine: every event crosses through the `providerStreamEvent` op in
// emission order, and the session settles once iteration finishes. A
// handler throw — before or mid-iteration — settles as a session error
// (upstream's lazyStream encodes setup failures as the stream's terminal
// error event; the Rust side appends it after the events already crossed).
globalThis.__piSessionCallProviderStream = function (id, provider) {
  const session = globalThis.__piSessions.get(id);
  const streamFn = globalThis.__piProviderStreams.get(provider);
  if (streamFn === undefined || typeof streamFn !== "function") {
    // Slot errors are JSON strings (the shape __piErrorText produces);
    // __piSessionLast parses them back into objects.
    globalThis.__piSessionSettle(session, {
      error: JSON.stringify({
        message: `Provider ${provider} is not registered with a streamSimple handler.`,
        stack: null,
      }),
    });
    return;
  }
  session.eventName = "providerStream";
  // The `SimpleStreamOptions` instrumentation crosses as functions
  // (custom-provider.md:195-198 — handlers call the hooks; pi sets them
  // on every run, `sdk.ts:424-426`): `onPayload` suspends on a promise
  // the host resolves with the `before_provider_request` chain's final
  // value; the one-way hooks queue FIFO through the `providerHookEvent`
  // op; `signal` is a plain object the host flips when the run aborts.
  const options =
    session.event.options && typeof session.event.options === "object"
      ? session.event.options
      : (session.event.options = {});
  session.providerHookToken = 0;
  session.providerHookPrompts = new Map();
  session.streamSignal = { aborted: false };
  options.signal = session.streamSignal;
  options.onPayload = (payload) =>
    new Promise((resolve) => {
      const token = ++session.providerHookToken;
      session.providerHookPrompts.set(token, resolve);
      globalThis.__piOps.providerHookPayload(
        id,
        token,
        JSON.stringify(payload ?? null),
      );
    });
  options.onResponse = (response) => {
    globalThis.__piOps.providerHookEvent(
      id,
      "after_provider_response",
      JSON.stringify(response ?? null),
    );
  };
  options.onProviderStreamEvent = (data, model) => {
    globalThis.__piOps.providerHookEvent(
      id,
      "provider_stream_event",
      JSON.stringify({ data: data ?? null, model: model ?? null }),
    );
  };
  Promise.resolve()
    .then(() => streamFn(session.event.model, session.event.context, options))
    .then(async (stream) => {
      for await (const event of stream) {
        globalThis.__piOps.providerStreamEvent(id, JSON.stringify(event));
      }
    })
    .then(
      () => globalThis.__piSessionSettle(session, { value: null }),
      (error) => globalThis.__piSessionSettle(session, { error: globalThis.__piErrorText(error) }),
    );
};

// The oauth login bridge (`adaptOAuth`, `provider-composer.ts:488-511`
// runs on the host side; this glue is its JS arm): the stashed config's
// `login` receives the adapter callbacks — one-way notifications cross
// the `oauthNotify` op, and the prompt callbacks return promises the
// host resolves through `oauthResolvePrompt` (the UI answers, the
// capability fires, the login function continues). `signal` is a plain
// object the host flips through `oauthAbortSignal`.
globalThis.__piOAuthPrompts = new Map();
globalThis.__piOAuthPromptToken = 0;
globalThis.__piSessionCallOAuthLogin = function (id, provider) {
  const session = globalThis.__piSessions.get(id);
  const oauth = globalThis.__piProviderOAuth.get(provider);
  if (oauth === undefined || typeof oauth.login !== "function") {
    globalThis.__piSessionSettle(session, {
      error: JSON.stringify({
        message: `Provider ${provider} is not registered with an oauth login handler.`,
        stack: null,
      }),
    });
    return;
  }
  session.eventName = "oauthNotify";
  session.oauthSignal = { aborted: false };
  const notify = (data) => {
    globalThis.__piOps.oauthNotify(id, JSON.stringify(data));
  };
  const ask = (payload) =>
    new Promise((resolve) => {
      const token = ++globalThis.__piOAuthPromptToken;
      globalThis.__piOAuthPrompts.set(id, { token, resolve, payload });
      globalThis.__piOps.oauthPrompt(id, token, JSON.stringify(payload));
    });
  const callbacks = {
    onAuth: (info) => notify({ type: "auth_url", ...info }),
    onDeviceCode: (info) => notify({ type: "device_code", ...info }),
    onPrompt: (prompt) => ask({ type: "text", ...prompt }),
    onProgress: (message) => notify({ type: "progress", message }),
    onManualCodeInput: () =>
      ask({ type: "manual_code", message: "Paste the authorization code" }),
    onSelect: (prompt) => ask({ type: "select", ...prompt }),
    signal: session.oauthSignal,
  };
  Promise.resolve()
    .then(() => oauth.login(callbacks))
    .then(
      (credential) =>
        globalThis.__piSessionSettle(session, {
          value: JSON.stringify({ ...credential, type: "oauth" }),
        }),
      (error) => globalThis.__piSessionSettle(session, { error: globalThis.__piErrorText(error) }),
    );
};

globalThis.__piResolveOAuthPrompt = function (id, token, answerJson) {
  const pending = globalThis.__piOAuthPrompts.get(id);
  if (pending === undefined) return "no-prompt";
  if (pending.token !== token) return "token-mismatch";
  globalThis.__piOAuthPrompts.delete(id);
  // A JSON `null` answer (the UI's cancel/escape) resolves as JS
  // `undefined` — upstream's prompt promises resolve `string | undefined`
  // and `JSON.stringify` drops `undefined` keys (extension-oauth-types.ts:39-47).
  let answer = undefined;
  try {
    answer = answerJson === null || answerJson === undefined ? undefined : JSON.parse(answerJson);
    if (answer === null) answer = undefined;
  } catch {
    answer = undefined;
  }
  pending.resolve(answer);
  return "ok";
};

// Resolves a stream session's pending `onPayload` promise with the
// `before_provider_request` chain's final value (the handler uses any
// non-undefined replacement, like the built-in adapters do). Token
// verified like the oauth prompts; prompts live on the session object,
// so `session_end` releases them.
globalThis.__piResolveProviderHook = function (id, token, resultJson) {
  const session = globalThis.__piSessions.get(id);
  if (!session || !session.providerHookPrompts) return "no-prompt";
  const pending = session.providerHookPrompts.get(token);
  if (pending === undefined) return "token-mismatch";
  session.providerHookPrompts.delete(token);
  let value;
  try {
    value = JSON.parse(resultJson);
  } catch {
    value = undefined;
  }
  pending(value);
  return "ok";
};

// Flags a stream session's crossed `signal.aborted` — the run's abort
// reaching a handler that polls it between yields.
globalThis.__piFlipProviderSignal = function (id) {
  const session = globalThis.__piSessions.get(id);
  if (session && session.streamSignal) session.streamSignal.aborted = true;
};

globalThis.__piOAuthHasLogin = function (provider) {
  const oauth = globalThis.__piProviderOAuth.get(provider);
  return oauth !== undefined && typeof oauth.login === "function";
};

globalThis.__piAbortOAuthSignal = function (id) {
  const session = globalThis.__piSessions.get(id);
  if (session && session.oauthSignal) session.oauthSignal.aborted = true;
};

// The one-shot oauth calls (`adaptOAuth`'s `refresh`/`toAuth` arms): the
// stashed `refreshToken` is async and settles through the job queue into
// a polled slot; `getApiKey` is synchronous and answers inline. Both
// return `{value}` or `{error}`.
globalThis.__piOAuthCallText = null;
globalThis.__piOAuthCallReady = function () {
  return globalThis.__piOAuthCallText !== null;
};
globalThis.__piCallOAuthRefresh = function (id, provider, credentialJson) {
  globalThis.__piOAuthCallText = null;
  const run = async () => {
    const oauth = globalThis.__piProviderOAuth.get(provider);
    if (oauth === undefined || typeof oauth.refreshToken !== "function") {
      return JSON.stringify({
        error: `Provider ${provider} is not registered with an oauth refresh handler.`,
      });
    }
    try {
      const credential = await oauth.refreshToken(
        JSON.parse(credentialJson || "null"),
        { aborted: false },
      );
      return JSON.stringify({ value: { ...credential, type: "oauth" } });
    } catch (error) {
      return JSON.stringify({ error: globalThis.__piErrorText(error) });
    }
  };
  Promise.resolve()
    .then(run)
    .then((text) => {
      globalThis.__piOAuthCallText = text;
    });
};
globalThis.__piCallOAuthApiKey = function (id, provider, credentialJson) {
  const oauth = globalThis.__piProviderOAuth.get(provider);
  if (oauth === undefined || typeof oauth.getApiKey !== "function") {
    return JSON.stringify({
      error: `Provider ${provider} is not registered with an oauth getApiKey handler.`,
    });
  }
  try {
    const credential = JSON.parse(credentialJson || "null");
    return JSON.stringify({ value: oauth.getApiKey(credential) });
  } catch (error) {
    return JSON.stringify({ error: globalThis.__piErrorText(error) });
  }
};

// The `user_bash` operations bridge (`BashOperations.exec`,
// core/tools/bash.ts:74-93): the host calls the stashed operations with
// a normalized options object; `onData` streams through the `bashData`
// op, `signal` stays undefined (no abort across the engine boundary —
// HANDOFF deviation), and the result lands in a one-slot poll the Rust
// side reads after draining jobs.
globalThis.__piBashText = null;
globalThis.__piCallBashOperations = function (command, cwd, optionsJson) {
  globalThis.__piBashText = null;
  const run = async () => {
    const operations = globalThis.__piBashOperations;
    if (!operations || typeof operations.exec !== "function") {
      return JSON.stringify({ error: "No bash operations are registered" });
    }
    let options = {};
    try {
      options = JSON.parse(optionsJson || "{}") ?? {};
    } catch {
      options = {};
    }
    const onData = (data) => {
      globalThis.__piOps.bashData(typeof data === "string" ? data : String(data));
    };
    try {
      const result = await operations.exec(command, cwd, {
        onData,
        timeout: options.timeout,
        env: options.env,
      });
      return JSON.stringify({ exitCode: result === undefined || result === null ? null : result.exitCode });
    } catch (error) {
      return JSON.stringify({ error: error instanceof Error ? error.message : String(error) });
    }
  };
  // The result text lands in the poll slot; a bridge rejection (host
  // pathologies) degrades to the {error} shape so the bounded poll
  // still settles.
  run().then(
    (text) => {
      globalThis.__piBashText = text;
    },
    (error) => {
      globalThis.__piBashText = JSON.stringify({
        error: error instanceof Error ? error.message : String(error),
      });
    },
  );
};
globalThis.__piBashReady = () => globalThis.__piBashText !== null;

// The result slot, JSON-encoded: {done:false} while pending,
// {done:true,error:{message,stack}} on throw, {done:true} without a
// result, {done:true,value:...} with one. Serialization failures degrade
// to the error shape like upstream's structuredClone-averse results would.
globalThis.__piSessionLast = function (id) {
  const session = globalThis.__piSessions.get(id);
  if (session === undefined) return JSON.stringify({ done: true, error: { message: "no such session" } });
  if (!session.done) return JSON.stringify({ done: false });
  if (session.error !== undefined) return JSON.stringify({ done: true, error: JSON.parse(session.error) });
  if (!session.hasValue) return JSON.stringify({ done: true });
  try {
    return JSON.stringify({ done: true, value: session.value });
  } catch (error) {
    return JSON.stringify({
      done: true,
      error: { message: `result is not JSON-serializable: ${error.message}`, stack: null },
    });
  }
};

globalThis.__piSessionEnd = function (id) {
  globalThis.__piSessions.delete(id);
};

// The tool context additions (runner.ts:975-1002): the callable tool list
// and nested executeTool, which never rejects — in E1 it returns the
// unbound-runner result (runner.ts:988-999) and E2 binds a real executor.
globalThis.__piCreateToolCtx = function (toolCallId) {
  const base = globalThis.__piCreateCtx();
  Object.defineProperty(base, "tools", {
    get() {
      globalThis.__piAssertRuntimeActive();
      return JSON.parse(globalThis.__piOps.getCallableTools());
    },
  });
  base.executeTool = (name, args, options) => {
    globalThis.__piAssertRuntimeActive();
    const raw = globalThis.__piOps.nestedExecuteTool(
      toolCallId,
      name,
      JSON.stringify(args ?? null),
      JSON.stringify(options ?? null),
    );
    return Promise.resolve(JSON.parse(raw));
  };
  return base;
};

// The Rust runner's view of every loaded extension: everything the
// dispatch loops and aggregations need, without the functions.
globalThis.__piInspect = function () {
  return JSON.stringify(
    globalThis.__piExtensions.map((ext) => ({
      path: ext.path,
      events: Object.fromEntries(Object.entries(ext.events).map(([name, list]) => [name, list.map((_, index) => index)])),
      tools: Object.entries(ext.tools).map(([name, tool]) => ({
        name,
        label: tool.label ?? name,
        description: tool.description ?? "",
        parameters: tool.parameters ?? null,
        exposure: tool.exposure ?? "direct",
        namespace: tool.namespace ?? null,
        defaultActive: tool.defaultActive !== false,
        hasExecute: typeof tool.execute === "function",
      })),
      commands: Object.entries(ext.commands).map(([name, command]) => ({
        name,
        description: command.description ?? "",
      })),
      flags: Object.entries(ext.flags).map(([name, flag]) => ({
        name,
        description: flag.description ?? "",
        type: flag.type,
        default: flag.default === undefined ? null : flag.default,
      })),
    })),
  );
};

// The index of a loaded extension by path, or -1 (the runner looks the
// synthetic `builtin:mcp` record up before registering bridge tools).
globalThis.__piFindExtension = function (path) {
  const index = globalThis.__piExtensions.findIndex((ext) => ext.path === path);
  return index;
};

// The MCP tool bridge (the `pi.registerTool(definition)` calls the bundled
// MCP extension makes when a server connects, `index.ts:416-443`): each
// definition lands in the synthetic extension's tool registry with an
// execute wrapper that forwards to the `mcpCallTool` op — the host maps the
// wire result (`convertMcpResult`, `tools.ts:215-235`) and the wrapper
// throws its `error` payload, so a failed call is a tool error.
// Registration rules match `pi.registerTool` (loader.ts:298-358): the
// parameters must be an object schema.
globalThis.__piMcpRegisterTools = function (path, toolsJson) {
  const extIdx = globalThis.__piFindExtension(path);
  if (extIdx === -1) {
    throw new Error(`MCP tools registered for unknown extension "${path}"`);
  }
  const ext = globalThis.__piExtensions[extIdx];
  const tools = JSON.parse(toolsJson);
  for (const tool of tools) {
    if (
      typeof tool.parameters !== "object" ||
      tool.parameters === null ||
      Array.isArray(tool.parameters)
    ) {
      throw new Error(
        `Tool "${tool.name}" registered by extension "${path}" must define an object parameter schema.`,
      );
    }
    const server = tool.server;
    const serverTool = tool.serverTool;
    ext.tools[tool.name] = {
      name: tool.name,
      label: tool.label,
      description: tool.description,
      parameters: tool.parameters,
      exposure: tool.exposure,
      namespace: tool.namespace,
      execute: (toolCallId, params) => {
        const raw = globalThis.__piOps.mcpCallTool(
          server,
          serverTool,
          JSON.stringify(params ?? {}),
        );
        const parsed = JSON.parse(raw);
        if (parsed.error !== undefined) throw new Error(parsed.error);
        return parsed.result;
      },
    };
  }
  globalThis.__piOps.refreshTools();
};
"#;

/// The `typebox` virtual module: the builders the E1 extension surface
/// needs, reproducing `TypeBox` 1.3.27's `Memory.Create` semantics —
/// enumerable keys are `{...schema, ...options}` in that order and the
/// `~kind`/`~optional` markers are non-enumerable, so `JSON.stringify`
/// output matches the real package byte for byte. Unsupported builders
/// throw rather than silently diverge.
pub const TYPEBOX_SHIM: &str = r#"
const setHidden = (object, key, value) => {
  Object.defineProperty(object, key, { configurable: true, writable: true, enumerable: false, value });
  return object;
};
const isSchema = (value) => typeof value === "object" && value !== null && !Array.isArray(value);
const isOptional = (value) => isSchema(value) && "~optional" in value;

const unsupported = (name) => {
  throw new Error(`typebox.${name} is not part of the E1 typebox shim (supported: String, Number, Boolean, Object, Array, Optional, Union, Unsafe)`);
};

export const Type = {
  String(options) {
    return setHidden({ type: "string", ...(options ?? {}) }, "~kind", "String");
  },
  Number(options) {
    return setHidden({ type: "number", ...(options ?? {}) }, "~kind", "Number");
  },
  Boolean(options) {
    return setHidden({ type: "boolean", ...(options ?? {}) }, "~kind", "Boolean");
  },
  Object(properties, options) {
    const props = properties ?? {};
    const required = Object.keys(props).filter((key) => !isOptional(props[key]));
    const schema = { type: "object" };
    if (required.length > 0) schema.required = required;
    schema.properties = props;
    return setHidden({ ...schema, ...(options ?? {}) }, "~kind", "Object");
  },
  Array(items, options) {
    return setHidden({ type: "array", items, ...(options ?? {}) }, "~kind", "Array");
  },
  Union(variants, options) {
    return setHidden({ anyOf: variants, ...(options ?? {}) }, "~kind", "Union");
  },
  Unsafe(schema) {
    return setHidden({ ...schema }, "~kind", "Unsafe");
  },
  Optional(schema) {
    return setHidden({ ...schema }, "~optional", true);
  },
};
Type.Literal = () => unsupported("Literal");
Type.Record = () => unsupported("Record");
Type.Enum = () => unsupported("Enum");
Type.KeyOf = () => unsupported("KeyOf");
Type.Recursive = () => unsupported("Recursive");
export default Type;
"#;

/// The `@earendil-works/pi-codemode/declarations` virtual module: a
/// verbatim port of `resolveSchemaRef`
/// (`packages/codemode/src/declarations.ts:160-180`), the only binding the
/// bundled tool-search extension imports.
pub const DECLARATIONS_SHIM: &str = r##"
export function resolveSchemaRef(ref, root) {
	if (ref !== "#" && !ref.startsWith("#/")) return undefined;
	let current = root;
	for (const segment of ref.slice(2).split("/").filter(Boolean)) {
		let decoded;
		try {
			decoded = decodeURIComponent(segment);
		} catch {
			// Schemas come from tools and MCP servers: a malformed escape names nothing instead of failing the caller.
			return undefined;
		}
		const key = decoded.replaceAll("~1", "/").replaceAll("~0", "~");
		if (typeof current !== "object" || current === null || Array.isArray(current) || !(key in current)) {
			return undefined;
		}
		current = current[key];
	}
	if (typeof current === "boolean") return current;
	return typeof current === "object" && current !== null && !Array.isArray(current) ? current : undefined;
}
"##;
