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
//!    the `emit*` loops, `runner.ts` §2), and the extension event bus
//!    (`core/event-bus.ts`).
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
globalThis.__piRuntime = { staleMessage: undefined, bound: false };
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
  });
  globalThis.__piExtState.push("loading");
  return globalThis.__piExtensions.length - 1;
};

globalThis.__piCommitExtension = function (extIdx) {
  globalThis.__piExtState[extIdx] = "active";
};

globalThis.__piDiscardExtension = function (extIdx) {
  globalThis.__piExtState[extIdx] = "failed";
  const ext = globalThis.__piExtensions[extIdx];
  for (const unsubscribe of ext.busSubscriptions) unsubscribe();
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
      const registrationError = globalThis.__piOps.mcpRegister(name, ext.path, JSON.stringify(config ?? null));
      if (registrationError !== "") throw new Error(registrationError);
    },
    unregisterMcpServer(name) {
      assertActive();
      const error = globalThis.__piOps.mcpUnregister(name, ext.path);
      if (error !== "") throw new Error(error);
    },
    getMcpServers() {
      assertActive();
      return JSON.parse(globalThis.__piOps.getMcpServers());
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

// The headless UI matrix (noOpUIContext, runner.ts:331-362; §4.6 row 1 of
// docs/EXTENSIONS.md): notify reaches the host hook, dialogs resolve to
// their "no UI" answers, and every setter is a no-op.
globalThis.__piCreateUiCtx = function () {
  const assertActive = () => globalThis.__piAssertRuntimeActive();
  return {
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
      return false;
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
      return false;
    },
    ui: {
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
      notify: (message) => {
        assertActive();
        globalThis.__piOps.notify(typeof message === "string" ? message : JSON.stringify(message));
      },
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
// "event" | "command" | "tool".
globalThis.__piSessions = new Map();
globalThis.__piSessionSeq = 0;

globalThis.__piSessionBegin = function (eventJson, kind, toolCallId) {
  const id = ++globalThis.__piSessionSeq;
  let ctx;
  if (kind === "command") ctx = globalThis.__piCreateCommandCtx();
  else if (kind === "tool") ctx = globalThis.__piCreateToolCtx(toolCallId);
  else if (kind === "trust") ctx = globalThis.__piCreateTrustCtx();
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

globalThis.__piSessionSettle = function (session, outcome) {
  session.done = true;
  if (outcome.error !== undefined) {
    session.error = outcome.error;
    session.hasValue = false;
    session.value = undefined;
    return;
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
