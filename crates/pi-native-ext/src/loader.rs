//! The one module in the native-extension stack that contains `unsafe`.
//!
//! Upstream pi has no equivalent (its extension runtime is `JavaScript`); this
//! is an additive pi-rs capability. All FFI with a loaded `cdylib` lives here
//! so the rest of the stack, including the `pi-coding-agent` host, stays safe.
//! Every unsafe operation has a `SAFETY` comment spelling out the invariant
//! from the ABI contract in [`crate::abi`].

use std::ffi::{CStr, CString, c_char};
use std::path::{Path, PathBuf};

use libloading::Library;
use serde_json::Value;

use crate::abi::{
    ABI_VERSION, CallRequest, CallResult, DeclaredTool, InitConfig, InitResult, SYMBOL_ABI_VERSION,
    SYMBOL_CALL, SYMBOL_FREE_STRING, SYMBOL_INIT, SYMBOL_SHUTDOWN,
};

type AbiVersionFn = unsafe extern "C" fn() -> u32;
type InitFn = unsafe extern "C" fn(*const c_char) -> *mut c_char;
type CallFn = unsafe extern "C" fn(*const c_char) -> *mut c_char;
type FreeStringFn = unsafe extern "C" fn(*mut c_char);
type ShutdownFn = unsafe extern "C" fn();

/// Failures while loading or calling a native extension. The extension name
/// is always present so startup diagnostics can point at the offending file.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// The dynamic library could not be opened.
    #[error("native extension `{name}`: cannot open {path}: {source}")]
    Open {
        /// Extension name from the file stem.
        name: String,
        /// Path that was attempted.
        path: PathBuf,
        /// Underlying loader error.
        #[source]
        source: libloading::Error,
    },
    /// A required ABI symbol was not exported.
    #[error("native extension `{name}`: missing required symbol `{symbol}`")]
    MissingSymbol {
        /// Extension name.
        name: String,
        /// Symbol that was looked up.
        symbol: &'static str,
    },
    /// The extension reports an incompatible ABI version.
    #[error("native extension `{name}`: ABI mismatch, expected {expected} but got {actual}")]
    AbiMismatch {
        /// Extension name.
        name: String,
        /// Version the host speaks.
        expected: u32,
        /// Version the extension reported.
        actual: u32,
    },
    /// The extension rejected its init config.
    #[error("native extension `{name}`: init failed: {message}")]
    InitFailed {
        /// Extension name.
        name: String,
        /// Reason from the extension.
        message: String,
    },
    /// A string crossing the boundary was not valid UTF-8 or not valid `JSON`.
    #[error("native extension `{name}`: invalid {what}: {message}")]
    InvalidPayload {
        /// Extension name.
        name: String,
        /// What was being decoded (`config`, `init result`, `call result`).
        what: &'static str,
        /// Decode error.
        message: String,
    },
    /// An `init` / `call` response reported `ok: false`.
    #[error("native extension `{name}`: {message}")]
    Extension {
        /// Extension name.
        name: String,
        /// Error string from the extension.
        message: String,
    },
}

/// A loaded `cdylib` with its required symbols resolved.
///
/// The [`Library`] is stored alongside the raw function pointers. Those
/// pointers are only valid while the library is loaded, so the fields are
/// private and there is no way to extract them. [`Drop`] calls
/// `pi_ext_shutdown` (if the extension exports it) before unloading.
pub struct LoadedLibrary {
    name: String,
    path: PathBuf,
    // Lifetime anchor: every symbol pointer below points into this library,
    // so it must stay mapped for as long as `self` exists. It is never read
    // directly, but dropping it early would invalidate all the fn pointers.
    #[allow(dead_code)]
    library: Library,
    init: InitFn,
    call: CallFn,
    free_string: FreeStringFn,
    shutdown: Option<ShutdownFn>,
    initialized: bool,
    tools: Vec<DeclaredTool>,
    hooks: Vec<String>,
}

impl LoadedLibrary {
    /// Open `path`, verify the ABI, and run `pi_ext_init` with `config`.
    pub fn open(path: &Path, name: &str, config: &InitConfig) -> Result<Self, LoadError> {
        // SAFETY: loading a dynamic library runs its constructors, so this is
        // inherently unsafe. The path comes from an explicitly configured
        // environment entry or a project-local `.pi/native/` file; the host
        // treats that configuration as trusted code, exactly like a JS
        // extension path. The `Library` is kept in `self` for the whole life
        // of every symbol pointer resolved below.
        let library = unsafe { Library::new(path) }.map_err(|source| LoadError::Open {
            name: name.to_string(),
            path: path.to_path_buf(),
            source,
        })?;

        // SAFETY: the ABI contract requires `pi_ext_abi_version` to be a
        // `fn() -> u32` with no side effects. The symbol borrows `library`,
        // which outlives this function and every later call through `self`.
        let abi_version: AbiVersionFn = *unsafe {
            library
                .get::<AbiVersionFn>(SYMBOL_ABI_VERSION.as_bytes())
                .map_err(|_| LoadError::MissingSymbol {
                    name: name.to_string(),
                    symbol: SYMBOL_ABI_VERSION,
                })?
        };
        // SAFETY: pure version getter; see the symbol contract above.
        let actual = unsafe { abi_version() };
        if actual != ABI_VERSION {
            return Err(LoadError::AbiMismatch {
                name: name.to_string(),
                expected: ABI_VERSION,
                actual,
            });
        }

        // SAFETY: each symbol must match the ABI signature; resolving a symbol
        // is unsafe because a wrong type would make later calls undefined.
        // The `library` field keeps the code mapped for as long as we hold
        // these pointers, and the fields are private to this module.
        let init: InitFn = *unsafe {
            library
                .get::<InitFn>(SYMBOL_INIT.as_bytes())
                .map_err(|_| LoadError::MissingSymbol {
                    name: name.to_string(),
                    symbol: SYMBOL_INIT,
                })?
        };
        let call: CallFn = *unsafe {
            library
                .get::<CallFn>(SYMBOL_CALL.as_bytes())
                .map_err(|_| LoadError::MissingSymbol {
                    name: name.to_string(),
                    symbol: SYMBOL_CALL,
                })?
        };
        let free_string: FreeStringFn = *unsafe {
            library
                .get::<FreeStringFn>(SYMBOL_FREE_STRING.as_bytes())
                .map_err(|_| LoadError::MissingSymbol {
                    name: name.to_string(),
                    symbol: SYMBOL_FREE_STRING,
                })?
        };
        // `pi_ext_shutdown` is optional; absence is not a load failure.
        let shutdown: Option<ShutdownFn> = unsafe {
            library
                .get::<ShutdownFn>(SYMBOL_SHUTDOWN.as_bytes())
                .ok()
                .map(|symbol| *symbol)
        };

        let config_json =
            serde_json::to_string(config).map_err(|error| LoadError::InvalidPayload {
                name: name.to_string(),
                what: "config",
                message: error.to_string(),
            })?;
        let config_c = CString::new(config_json).map_err(|error| LoadError::InvalidPayload {
            name: name.to_string(),
            what: "config",
            message: error.to_string(),
        })?;

        let mut loaded = Self {
            name: name.to_string(),
            path: path.to_path_buf(),
            library,
            init,
            call,
            free_string,
            shutdown,
            initialized: false,
            tools: Vec::new(),
            hooks: Vec::new(),
        };
        let raw = loaded.invoke_init(&config_c)?;
        let json = loaded.take_string(raw, "init result")?;
        let result: InitResult =
            serde_json::from_str(&json).map_err(|error| LoadError::InvalidPayload {
                name: loaded.name.clone(),
                what: "init result",
                message: error.to_string(),
            })?;
        if !result.ok {
            return Err(LoadError::InitFailed {
                name: loaded.name.clone(),
                message: result.error.unwrap_or_else(|| "unknown error".to_string()),
            });
        }
        loaded.tools = result.tools;
        loaded.hooks = result.hooks;
        loaded.initialized = true;
        Ok(loaded)
    }

    /// Extension name (the file stem used during discovery).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Path this library was loaded from.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Tools declared during init.
    #[must_use]
    pub fn tools(&self) -> &[DeclaredTool] {
        &self.tools
    }

    /// Hooks declared during init.
    #[must_use]
    pub fn hooks(&self) -> &[String] {
        &self.hooks
    }

    /// Run one `pi_ext_call` request.
    pub fn call(&self, request: &CallRequest) -> Result<CallResult, LoadError> {
        let request_json =
            serde_json::to_string(request).map_err(|error| LoadError::InvalidPayload {
                name: self.name.clone(),
                what: "call request",
                message: error.to_string(),
            })?;
        let request_c = CString::new(request_json).map_err(|error| LoadError::InvalidPayload {
            name: self.name.clone(),
            what: "call request",
            message: error.to_string(),
        })?;
        // SAFETY: `self.call` was resolved from `pi_ext_call` in `open`, which
        // checked the ABI version first. `request_c.as_ptr()` is a live,
        // NUL-terminated `C` string; the contract says the extension only
        // reads it for the duration of this call. `self.library` is still
        // alive because `self` is alive.
        let raw = unsafe { (self.call)(request_c.as_ptr()) };
        let json = self.take_string(raw, "call result")?;
        let result: CallResult =
            serde_json::from_str(&json).map_err(|error| LoadError::InvalidPayload {
                name: self.name.clone(),
                what: "call result",
                message: error.to_string(),
            })?;
        if !result.ok {
            return Err(LoadError::Extension {
                name: self.name.clone(),
                message: result.error.unwrap_or_else(|| "unknown error".to_string()),
            });
        }
        Ok(result)
    }

    /// Notify the extension that the host is done with it. Idempotent.
    pub fn shutdown(&mut self) {
        if !self.initialized {
            return;
        }
        self.initialized = false;
        if let Some(shutdown) = self.shutdown {
            // SAFETY: the optional `pi_ext_shutdown` symbol was resolved with
            // the `fn()` signature and the library is still loaded. It is
            // called at most once, guarded by `initialized`.
            unsafe { shutdown() };
        }
    }

    fn invoke_init(&self, config: &CStr) -> Result<*mut c_char, LoadError> {
        // SAFETY: `self.init` is `pi_ext_init` resolved after the version
        // check; `config` is a live NUL-terminated string. The extension may
        // retain parsed data but must not retain the pointer itself.
        let raw = unsafe { (self.init)(config.as_ptr()) };
        if raw.is_null() {
            return Err(LoadError::InvalidPayload {
                name: self.name.clone(),
                what: "init result",
                message: "extension returned a null pointer".to_string(),
            });
        }
        Ok(raw)
    }

    /// Decode an extension-owned `C` string and hand the pointer back to the
    /// extension's own `pi_ext_free_string`.
    fn take_string(&self, raw: *mut c_char, what: &'static str) -> Result<String, LoadError> {
        // SAFETY: the ABI contract requires `pi_ext_init` / `pi_ext_call` to
        // return either null or a live NUL-terminated UTF-8 `C` string owned by
        // the extension. We copy it into a Rust `String` before freeing.
        let decoded = unsafe { CStr::from_ptr(raw) }
            .to_str()
            .map_err(|error| LoadError::InvalidPayload {
                name: self.name.clone(),
                what,
                message: error.to_string(),
            })?
            .to_string();
        // SAFETY: `raw` came from this extension and `self.free_string` is its
        // exported `pi_ext_free_string`, the matching deallocator. It is
        // called exactly once. The copy above no longer borrows `raw`.
        unsafe { (self.free_string)(raw) };
        Ok(decoded)
    }

    /// Exposed only for diagnostics: the decoded `JSON` of a ping response.
    pub fn ping(&self) -> Result<Value, LoadError> {
        Ok(self.call(&CallRequest::Ping)?.result)
    }
}

impl Drop for LoadedLibrary {
    fn drop(&mut self) {
        self.shutdown();
    }
}
