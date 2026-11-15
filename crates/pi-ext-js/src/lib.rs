//! E0 spike for the extensions host: a minimal embedded-JS engine round trip.
//!
//! Proves the mechanics `docs/EXTENSIONS.md` §6.4 sketches for `pi-ext-js`,
//! on the engine picked at the E0 gate: `rquickjs` 0.14 (QuickJS-NG). The
//! engine choice flipped from `deno_core` to `rquickjs` before the spike
//! ran: Cloudflare acquired Deno (2026-10-09) and deprecated the Deno
//! runtime (EOL ~2027-10), while Bun — the operator's suggested
//! alternative — exposes no embeddable library API at all (the Rust rewrite
//! of 2026-05 ships none; embedding Bun inside another process is still
//! only possible as the subprocess host rejected in §6.2). The recon's
//! recorded fallback (`rquickjs` + a TS transpiler) therefore became the
//! primary; `deno_core` is demoted to fallback and the full decision record
//! lives in `docs/EXTENSIONS.md` §6 and `HANDOFF.md`.
//!
//! Upstream ground truth for the shape proven here:
//! - factory contract — an extension module default-exports
//!   `(pi: ExtensionAPI) => void | Promise<void>`
//!   (packages/coding-agent/docs/extensions.md, "Extension structure");
//! - `pi.on(event, handler)` registration, fanned out in registration order
//!   by the runner (`packages/coding-agent/src/core/extensions/runner.ts`);
//! - `pi.registerCommand(name, handler)` command registration (runner.ts).
//!
//! Spike scope — intentionally absent, all E1 work per `docs/EXTENSIONS.md`
//! §8: no TS transpile (`QuickJS` parses JavaScript only; `swc`/`oxc` lands
//! in E1 as the jiti replacement), no import resolution or module loader (the
//! spike module has no imports), no async factory or async handlers
//! (`QuickJS` is single-threaded here; the host drains pending engine jobs
//! synchronously after each call), no full `ExtensionAPI` — just `on`,
//! `registerCommand`, and a Rust-backed `getConfig` to prove the JS→Rust→JS
//! op round trip — and no `ExtensionRunner` port (dispatch semantics stay
//! Rust-side per §6.4).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rquickjs::{Array, Context, Function, Module, Object, Runtime, Value};

/// Bootstrap glue evaluated once per host: it creates the `pi` API object
/// with JS-side registries for event and command handlers, and forwards
/// `getConfig` to the Rust closure installed as
/// `globalThis.__piGetConfig`.
const BOOTSTRAP: &str = r#"
globalThis.__piEvents = [];
globalThis.__piCommandNames = [];
globalThis.__piCommands = {};
globalThis.__toText = (value) =>
  typeof value === "string" ? value : JSON.stringify(value);
globalThis.__pi = {
  on(event, handler) {
    globalThis.__piEvents.push([event, handler]);
  },
  registerCommand(name, handler) {
    globalThis.__piCommands[name] = handler;
    globalThis.__piCommandNames.push(name);
  },
  getConfig(key) {
    return globalThis.__piGetConfig(key);
  },
};
"#;

/// Synthetic E0 fixture module in the upstream factory shape (tests and the
/// `spike` example only; real user code is never loaded by this crate).
pub const SPIKE_EXTENSION_SOURCE: &str = r#"
export default function (pi) {
  pi.on("session_start", () => "started");
  pi.on("session_start", () => "also-started");
  pi.on("before_tool_call", (payload) =>
    globalThis.__toText({
      tool: JSON.parse(payload).tool,
      patched: true,
    }),
  );
  pi.registerCommand("ping", () => "pong");
  globalThis.__seenModel = pi.getConfig("model");
}
"#;

/// Errors raised by the JS host.
#[derive(Debug, thiserror::Error)]
pub enum JsHostError {
    /// The engine failed to compile, evaluate, or call into user code.
    #[error("javascript engine error: {0}")]
    Js(#[from] rquickjs::Error),
    /// The module has no default export to invoke as the factory.
    #[error("extension `{0}` has no default export")]
    MissingDefaultExport(String),
    /// A pending engine job threw while the host drained it.
    #[error("javascript job error: {0}")]
    Job(String),
}

/// A single-threaded embedded JavaScript host for one set of extensions.
///
/// Owns one `QuickJS` runtime plus its context; every method enters the
/// context, does its work, and drains pending engine jobs before returning.
/// The engine boundary carries only what §6.4 allows: registration calls
/// (JS→Rust state), event dispatch (Rust→JS calls, JSON-shaped payloads and
/// stringified results), and settings reads (Rust state into JS).
pub struct JsHost {
    // Dropped before `rt` so the context never outlives its runtime.
    ctx: Context,
    rt: Runtime,
    config: Arc<Mutex<HashMap<String, String>>>,
}

impl JsHost {
    /// Creates a runtime with a 256 MiB engine memory limit and installs
    /// the bootstrap glue.
    pub fn new() -> Result<Self, JsHostError> {
        let rt = Runtime::new()?;
        rt.set_memory_limit(256 * 1024 * 1024);
        let ctx = Context::full(&rt)?;
        let config = Arc::new(Mutex::new(HashMap::new()));
        let getter_config = config.clone();
        ctx.with(|ctx| -> Result<(), JsHostError> {
            let getter = Function::new(ctx.clone(), move |key: String| -> String {
                getter_config
                    .lock()
                    .expect("config mutex poisoned")
                    .get(&key)
                    .cloned()
                    .unwrap_or_default()
            })?;
            ctx.globals().set("__piGetConfig", getter)?;
            ctx.eval::<(), _>(BOOTSTRAP)?;
            Ok(())
        })?;
        Ok(Self { ctx, rt, config })
    }

    /// Replaces the settings map the `getConfig` op serves from.
    pub fn set_config(&self, config: HashMap<String, String>) {
        *self.config.lock().expect("config mutex poisoned") = config;
    }

    /// Loads one extension module and invokes its default-export factory
    /// with the `pi` API object, mirroring the upstream load step.
    pub fn load_extension(&self, name: &str, source: &str) -> Result<(), JsHostError> {
        self.ctx.with(|ctx| -> Result<(), JsHostError> {
            let (module, _promise) = Module::declare(ctx.clone(), name, source)?.eval()?;
            let namespace = module.namespace()?;
            let factory: Option<Function> = namespace.get("default")?;
            let Some(factory) = factory else {
                return Err(JsHostError::MissingDefaultExport(name.to_string()));
            };
            let pi: Object = ctx.globals().get("__pi")?;
            let _: Value = factory.call((pi,))?;
            Ok(())
        })?;
        self.drive_jobs()?;
        Ok(())
    }

    /// Event names in registration order (duplicates included); each
    /// registry entry is an `[event, handler]` pair.
    pub fn registered_events(&self) -> Result<Vec<String>, JsHostError> {
        self.ctx.with(|ctx| -> Result<Vec<String>, JsHostError> {
            let events: Array = ctx.globals().get("__piEvents")?;
            let count = events.len();
            let mut names = Vec::with_capacity(count);
            for i in 0..count {
                let pair: Array = events.get(i)?;
                names.push(pair.get(0)?);
            }
            Ok(names)
        })
    }

    /// Command names in registration order.
    pub fn registered_commands(&self) -> Result<Vec<String>, JsHostError> {
        self.string_array("__piCommandNames")
    }

    /// Dispatches an event to every handler registered for it, in
    /// registration order, returning each handler's stringified result;
    /// `undefined`/`null` results are dropped.
    pub fn emit(&self, event: &str, payload: &str) -> Result<Vec<String>, JsHostError> {
        let out = self.ctx.with(|ctx| -> Result<Vec<String>, JsHostError> {
            let to_text: Function = ctx.globals().get("__toText")?;
            let events: Array = ctx.globals().get("__piEvents")?;
            let count = events.len();
            let mut out = Vec::new();
            for i in 0..count {
                let pair: Array = events.get(i)?;
                let name: String = pair.get(0)?;
                if name != event {
                    continue;
                }
                let handler: Function = pair.get(1)?;
                let result: Value = handler.call((payload,))?;
                if result.is_undefined() || result.is_null() {
                    continue;
                }
                out.push(to_text.call((result,))?);
            }
            Ok(out)
        })?;
        self.drive_jobs()?;
        Ok(out)
    }

    /// Reads a global string binding back out of the engine (used by the
    /// tests and the `spike` example to prove the JS→Rust→JS→Rust loop).
    pub fn global_string(&self, name: &str) -> Result<Option<String>, JsHostError> {
        self.ctx.with(|ctx| -> Result<Option<String>, JsHostError> {
            let value: Value = ctx.globals().get(name)?;
            if value.is_undefined() || value.is_null() {
                return Ok(None);
            }
            let to_text: Function = ctx.globals().get("__toText")?;
            Ok(Some(to_text.call((value,))?))
        })
    }

    /// Reads a JS-side array of strings (the bootstrap registries).
    fn string_array(&self, global: &str) -> Result<Vec<String>, JsHostError> {
        self.ctx.with(|ctx| -> Result<Vec<String>, JsHostError> {
            let items: Array = ctx.globals().get(global)?;
            let count = items.len();
            let mut names = Vec::with_capacity(count);
            for i in 0..count {
                names.push(items.get::<String>(i)?);
            }
            Ok(names)
        })
    }

    /// Drains pending engine jobs so module evaluation and promise
    /// settlement finish before control returns to Rust.
    fn drive_jobs(&self) -> Result<(), JsHostError> {
        // The job-error type is not publicly nameable in `rquickjs`, so the
        // error is flattened to its message here.
        while self
            .rt
            .execute_pending_job()
            .map_err(|err| JsHostError::Job(err.to_string()))?
        {}
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> JsHost {
        let host = JsHost::new().expect("host");
        let mut config = HashMap::new();
        config.insert("model".to_string(), "glm-5.3-flash".to_string());
        host.set_config(config);
        host
    }

    #[test]
    fn loads_module_and_registers() {
        let host = host();
        host.load_extension("spike", SPIKE_EXTENSION_SOURCE)
            .expect("load");
        assert_eq!(
            host.registered_events().expect("events"),
            ["session_start", "session_start", "before_tool_call"]
        );
        assert_eq!(host.registered_commands().expect("commands"), ["ping"]);
        assert_eq!(
            host.global_string("__seenModel").expect("global"),
            Some("glm-5.3-flash".to_string())
        );
    }

    #[test]
    fn emit_fans_out_in_registration_order() {
        let host = host();
        host.load_extension("spike", SPIKE_EXTENSION_SOURCE)
            .expect("load");
        assert_eq!(
            host.emit("session_start", "").expect("emit"),
            ["started", "also-started"]
        );
        assert_eq!(
            host.emit("before_tool_call", r#"{"tool":"read"}"#)
                .expect("emit"),
            [r#"{"tool":"read","patched":true}"#]
        );
        assert_eq!(
            host.emit("unknown_event", "").expect("emit"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn module_without_default_export_is_rejected() {
        let host = host();
        let err = host
            .load_extension("bad", "export const answer = 42;")
            .expect_err("missing default export must fail");
        assert!(matches!(err, JsHostError::MissingDefaultExport(ref name) if name == "bad"));
    }
}
