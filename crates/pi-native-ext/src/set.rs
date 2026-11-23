//! Discovery-agnostic registry of loaded native extensions.
//!
//! Additive pi-rs capability; upstream pi has no native extension host. This
//! module deliberately knows nothing about `ToolRegistry`, hooks being wired
//! into modes, or where libraries come from. It only owns the mapping from
//! declared tool / hook names to the extension that handles them and the
//! serialized dispatch of [`CallRequest`] values.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::Value;

use crate::abi::{ABI_VERSION, CallRequest, CallResult, DeclaredTool, InitConfig, InitResult};
use crate::loader::{LoadError, LoadedLibrary};

/// One dynamic library to load. `name` is used in every diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadSpec {
    /// Extension name, normally the file stem.
    pub name: String,
    /// Path of the `cdylib`.
    pub path: PathBuf,
}

impl LoadSpec {
    /// Build a spec from a path, using its file stem as the name.
    #[must_use]
    pub fn from_path(path: &Path) -> Self {
        let stem = path.file_stem().map_or_else(
            || path.to_string_lossy().into_owned(),
            |stem| stem.to_string_lossy().into_owned(),
        );
        // Unix shared libraries carry a `lib` prefix; the crate / extension
        // name is what follows it.
        let name = stem
            .strip_prefix("lib")
            .filter(|rest| !rest.is_empty())
            .unwrap_or(&stem)
            .to_string();
        Self {
            name,
            path: path.to_path_buf(),
        }
    }
}

/// In-tree extension with the same method surface as the C ABI.
///
/// Implement this when an extension should be compiled into the host instead
/// of shipped as a `cdylib`. It goes through exactly the same [`CallRequest`]
/// / [`CallResult`] dispatch as a dynamically loaded library, so swapping
/// between in-tree and dynamic changes deployment, not behaviour.
pub trait NativeExtension: Send + Sync {
    /// Extension name, used in diagnostics and lookup.
    fn name(&self) -> &str;

    /// ABI version this extension speaks. Defaults to the current version.
    fn abi_version(&self) -> u32 {
        ABI_VERSION
    }

    /// Declare tools and hooks; analogous to `pi_ext_init`.
    fn init(&self, _config: &InitConfig) -> InitResult {
        InitResult {
            ok: true,
            tools: Vec::new(),
            hooks: Vec::new(),
            error: None,
        }
    }

    /// Handle one call; analogous to `pi_ext_call`.
    fn call(&self, request: &CallRequest) -> Result<CallResult, String>;

    /// Notification before the extension is dropped; analogous to
    /// `pi_ext_shutdown`.
    fn shutdown(&self) {}
}

/// The common shape of a dynamic library and an in-tree extension.
trait Dispatch: Send + Sync {
    fn call(&self, request: &CallRequest) -> Result<CallResult, LoadError>;
    fn shutdown(&mut self);
}

impl Dispatch for LoadedLibrary {
    fn call(&self, request: &CallRequest) -> Result<CallResult, LoadError> {
        LoadedLibrary::call(self, request)
    }

    fn shutdown(&mut self) {
        LoadedLibrary::shutdown(self);
    }
}

struct InTreeAdapter {
    extension: Box<dyn NativeExtension>,
}

impl Dispatch for InTreeAdapter {
    fn call(&self, request: &CallRequest) -> Result<CallResult, LoadError> {
        self.extension
            .call(request)
            .map_err(|message| LoadError::Extension {
                name: self.extension.name().to_string(),
                message,
            })
    }

    fn shutdown(&mut self) {
        self.extension.shutdown();
    }
}

/// All loaded native extensions, with tool / hook owner maps.
///
/// Calls are serialized under one mutex. Extension authors therefore do not
/// need to make `pi_ext_call` reentrant, and the host never holds the lock
/// across unrelated work.
pub struct ExtensionSet {
    dispatch: Mutex<Vec<Box<dyn Dispatch>>>,
    owners: HashMap<String, usize>,
    subscribers: HashMap<String, Vec<usize>>,
    tools: Vec<DeclaredTool>,
}

impl Default for ExtensionSet {
    fn default() -> Self {
        Self::new()
    }
}

impl ExtensionSet {
    /// An empty set; loading nothing keeps the whole host inert.
    #[must_use]
    pub fn new() -> Self {
        Self {
            dispatch: Mutex::new(Vec::new()),
            owners: HashMap::new(),
            subscribers: HashMap::new(),
            tools: Vec::new(),
        }
    }

    /// Open a `cdylib`, verify its ABI, run `pi_ext_init`, and index what it
    /// declares. First extension to declare a name wins, like JS tools.
    pub fn add_library(&mut self, spec: &LoadSpec, config: &InitConfig) -> Result<(), LoadError> {
        let library = LoadedLibrary::open(&spec.path, &spec.name, config)?;
        let tools = library.tools().to_vec();
        let hooks = library.hooks().to_vec();
        self.push(Box::new(library), tools, &hooks);
        Ok(())
    }

    /// Register a compiled-in extension through [`NativeExtension`].
    pub fn add_in_tree(
        &mut self,
        extension: Box<dyn NativeExtension>,
        config: &InitConfig,
    ) -> Result<(), LoadError> {
        if extension.abi_version() != ABI_VERSION {
            return Err(LoadError::AbiMismatch {
                name: extension.name().to_string(),
                expected: ABI_VERSION,
                actual: extension.abi_version(),
            });
        }
        let InitResult {
            ok,
            tools,
            hooks,
            error,
        } = extension.init(config);
        if !ok {
            return Err(LoadError::InitFailed {
                name: extension.name().to_string(),
                message: error.unwrap_or_else(|| "unknown error".to_string()),
            });
        }
        self.push(Box::new(InTreeAdapter { extension }), tools, &hooks);
        Ok(())
    }

    fn push(&mut self, dispatch: Box<dyn Dispatch>, tools: Vec<DeclaredTool>, hooks: &[String]) {
        let mut guard = self
            .dispatch
            .lock()
            .expect("native extension mutex poisoned");
        let index = guard.len();
        for tool in &tools {
            self.owners.entry(tool.name.clone()).or_insert(index);
        }
        for hook in hooks {
            self.subscribers
                .entry(hook.clone())
                .or_default()
                .push(index);
        }
        self.tools.extend(tools);
        guard.push(dispatch);
    }

    /// `true` when no extension was loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.dispatch
            .lock()
            .expect("native extension mutex poisoned")
            .is_empty()
    }

    /// Every declared tool, in load order (duplicates keep first-wins order).
    #[must_use]
    pub fn tools(&self) -> &[DeclaredTool] {
        &self.tools
    }

    /// Whether any extension subscribes to `hook`.
    #[must_use]
    pub fn declares_hook(&self, hook: &str) -> bool {
        self.subscribers.contains_key(hook)
    }

    /// Dispatch a tool call to the extension that declared `name`.
    pub fn call_tool(&self, name: &str, arguments: &Value) -> Result<CallResult, LoadError> {
        let index = *self.owners.get(name).ok_or_else(|| LoadError::Extension {
            name: name.to_string(),
            message: "no native extension declares this tool".to_string(),
        })?;
        let guard = self
            .dispatch
            .lock()
            .expect("native extension mutex poisoned");
        guard[index].call(&CallRequest::Tool {
            name: name.to_string(),
            arguments: arguments.clone(),
        })
    }

    /// Deliver a hook to every subscriber, in load order. Extension errors
    /// become failed [`CallResult`] values rather than aborting the event.
    #[must_use]
    pub fn dispatch_hook(&self, hook: &str, payload: &Value) -> Vec<CallResult> {
        let Some(indices) = self.subscribers.get(hook) else {
            return Vec::new();
        };
        let guard = self
            .dispatch
            .lock()
            .expect("native extension mutex poisoned");
        let request = CallRequest::Hook {
            hook: hook.to_string(),
            payload: payload.clone(),
        };
        indices
            .iter()
            .map(|&index| {
                guard[index]
                    .call(&request)
                    .unwrap_or_else(|error| CallResult::err(error.to_string()))
            })
            .collect()
    }

    /// Notify every extension explicitly. Safe to call more than once.
    pub fn shutdown(&self) {
        let mut guard = self
            .dispatch
            .lock()
            .expect("native extension mutex poisoned");
        for extension in guard.iter_mut() {
            extension.shutdown();
        }
    }
}
