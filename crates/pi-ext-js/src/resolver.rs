//! Module resolution for extension imports.
//!
//! Ports the three upstream resolution surfaces it replaces (E1 subset, §5.4
//! of `docs/EXTENSIONS.md`): the virtual-module table
//! (`core/extensions/virtual-modules.ts:14-42` — bundled modules every
//! extension may import), the relative-path resolution of the extension
//! directory (jiti), and the entry rules (`loader.ts:748-750`: `.ts`/`.js`
//! only; `loader.ts:761-791`: `index.ts` before `index.js`).
//!
//! E1 deliberately resolved *no* `node_modules`; E4 adds the `node:*` and
//! host-package virtual modules served by [`node`] while keeping that same
//! no-`node_modules` rule. Unknown specifiers still error with the E1
//! explanation (`docs/EXTENSIONS.md` §6.3). The same table also serves CJS
//! `require()` calls: [`crate::cjs::wrap`] lowers each static require into an
//! ESM import, so both formats share this resolver and the virtual modules
//! (upstream's jiti bridge, `core/extensions/loader.ts:42, 571-602`).

use std::path::{Path, PathBuf};

use rquickjs::{Ctx, Module, Result as JsResult};

#[path = "node/mod.rs"]
pub mod node;

/// Sources of the virtual modules the host serves.
///
/// `typebox` and `@earendil-works/pi-codemode/declarations` are the
/// specifiers the bundled `tool-search` extension imports (E1); E4 adds the
/// `node:*` built-ins and the host packages in [`node`]. Every other
/// specifier is rejected with the documented E1 explanation.
pub fn virtual_module_source(specifier: &str) -> Option<&'static str> {
    match specifier {
        "typebox" | "@sinclair/typebox" => Some(crate::glue::TYPEBOX_SHIM),
        "@earendil-works/pi-codemode/declarations" => Some(crate::glue::DECLARATIONS_SHIM),
        _ => node::source(specifier).or_else(|| crate::hostpkg::source(specifier)),
    }
}

/// Whether a specifier is a host package the core does not serve. E4 serves
/// the state-backed packages in [`node::HOST_PACKAGES`]; the rest keep the
/// upstream (E1) rejection string.
pub fn is_unavailable_host_module(specifier: &str) -> bool {
    !node::HOST_PACKAGES.contains(&specifier)
        && !crate::hostpkg::HOST_PACKAGE_SPECIFIERS.contains(&specifier)
        && (specifier.starts_with("@earendil-works/")
            || specifier.starts_with("@mariozechner/")
            || specifier.starts_with("pi-")
            || specifier == "pi-coding-agent")
}

/// Reads a `pi.extensions` manifest from a `package.json` body
/// (`core/pi-manifest.ts:13-42`: strips the BOM, requires a `pi` object,
/// keeps only arrays of strings).
pub fn read_pi_manifest(package_json: &str) -> Option<Vec<String>> {
    let without_bom = package_json
        .strip_prefix('\u{feff}')
        .unwrap_or(package_json);
    let value: serde_json::Value = serde_json::from_str(without_bom).ok()?;
    let pi = value.get("pi")?.as_object()?;
    let extensions = pi.get("extensions")?.as_array()?;
    if !extensions.iter().all(serde_json::Value::is_string) {
        return None;
    }
    Some(
        extensions
            .iter()
            .filter_map(|entry| entry.as_str().map(str::to_string))
            .collect(),
    )
}

/// Extension entries for one directory, in upstream order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectoryEntry {
    /// A `package.json` with a `pi.extensions` manifest listing these paths.
    Manifest(Vec<PathBuf>),
    /// `index.ts` (preferred) or `index.js`.
    Index(PathBuf),
}

/// Resolves a directory to its extension entries
/// (`loader.ts:761-791`): a `package.json` `pi.extensions` manifest whose
/// declared paths exist wins, else `index.ts`, else `index.js`, else none.
pub fn resolve_directory_entries(dir: &Path) -> Option<DirectoryEntry> {
    let package_json_path = dir.join("package.json");
    if let Ok(package_json) = std::fs::read_to_string(&package_json_path)
        && let Some(manifest) = read_pi_manifest(&package_json)
    {
        let entries: Vec<PathBuf> = manifest
            .iter()
            .map(|ext_path| dir.join(ext_path))
            .filter(|resolved| resolved.exists())
            .collect();
        if !entries.is_empty() {
            return Some(DirectoryEntry::Manifest(entries));
        }
    }
    let ts_entry = dir.join("index.ts");
    if ts_entry.exists() {
        return Some(DirectoryEntry::Index(ts_entry));
    }
    let js_entry = dir.join("index.js");
    if js_entry.exists() {
        return Some(DirectoryEntry::Index(js_entry));
    }
    None
}

/// Resolves one relative specifier against the importing module's
/// directory: the exact file when it is already `.ts`/`.js`, then the
/// `.ts`/`.js` suffixed forms, then the `index.ts`/`index.js` directory
/// forms (the upstream entry rules).
pub fn resolve_relative(base_dir: &Path, specifier: &str) -> Option<PathBuf> {
    let candidate = base_dir.join(specifier);
    if is_extension_file(&candidate) && candidate.is_file() {
        return Some(candidate);
    }
    for suffix in [".ts", ".js"] {
        let with_suffix = with_appended(&candidate, suffix);
        if with_suffix.is_file() {
            return Some(with_suffix);
        }
    }
    for index in ["index.ts", "index.js"] {
        let index_path = candidate.join(index);
        if index_path.is_file() {
            return Some(index_path);
        }
    }
    None
}

fn with_appended(path: &Path, suffix: &str) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(suffix);
    PathBuf::from(os)
}

fn is_extension_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("ts" | "js")
    )
}

/// Raises a JavaScript `Error` with `message` so import failures carry the
/// documented explanations instead of an opaque engine error.
fn throw_js_error(ctx: &Ctx<'_>, message: &str) -> rquickjs::Error {
    match serde_json::to_string(message) {
        Ok(json) => match ctx.eval::<rquickjs::Value, _>(format!("new Error({json})")) {
            Ok(error) => ctx.throw(error),
            Err(err) => err,
        },
        Err(_) => rquickjs::Error::Unknown,
    }
}

/// The resolution error text for specifiers this host does not serve. The
/// host-package branch keeps the documented E1 string verbatim — bundled
/// packages we cannot provide must keep failing the same way. Exposed so
/// the CJS `require` shim throws the same message as an ESM import
/// (`crate::cjs`).
pub fn unavailable_module_message(name: &str) -> String {
    if name.starts_with("node:") {
        format!(
            "Module \"{name}\" is not polyfilled by the extensions host; see crates/pi-ext-js/src/node for the supported set"
        )
    } else {
        format!(
            "Module \"{name}\" could not be resolved by the extensions host (E1 supports relative paths and the bundled virtual modules; node_modules resolution lands in E4)"
        )
    }
}

/// The resolution error for specifiers this host does not serve.
fn throw_unavailable_module(ctx: &Ctx<'_>, name: &str) -> rquickjs::Error {
    throw_js_error(ctx, &unavailable_module_message(name))
}

/// The `rquickjs` resolver for extension modules: virtual specifiers map to
/// `virtual:` module names, relative and absolute specifiers resolve
/// against the importing file's directory, everything else is rejected.
#[derive(Default)]
pub struct ExtensionResolver;

impl rquickjs::loader::Resolver for ExtensionResolver {
    fn resolve(
        &mut self,
        ctx: &Ctx<'_>,
        base: &str,
        name: &str,
        _attributes: Option<rquickjs::loader::ImportAttributes<'_>>,
    ) -> JsResult<String> {
        if virtual_module_source(name).is_some() {
            return Ok(format!("virtual:{name}"));
        }
        let is_relative = name.starts_with("./") || name.starts_with("../");
        if is_relative || name.starts_with('/') {
            let base_dir = Path::new(base)
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default();
            let scoped = if name.starts_with('/') {
                PathBuf::from(name)
            } else {
                base_dir.join(name)
            };
            if is_extension_file(&scoped) && scoped.is_file() {
                return Ok(scoped.to_string_lossy().into_owned());
            }
            if let Some(resolved) = resolve_relative(&base_dir, name) {
                return Ok(resolved.to_string_lossy().into_owned());
            }
        }
        Err(throw_unavailable_module(ctx, name))
    }
}

/// The `rquickjs` loader: virtual modules serve their shim sources; files
/// are read, TypeScript-stripped (`transpile::transpile`), and declared.
/// CJS files are additionally wrapped by [`crate::cjs::wrap`] into ESM with
/// a `module.exports` default export, so ESM extensions can `import` a CJS
/// dependency and its static `require` calls resolve through this same
/// loader. A process-lifetime transpile cache keeps each source transformed
/// once per host, mirroring upstream's module cache (`loader.ts:136-158`).
#[derive(Default)]
pub struct ExtensionLoader {
    transpiled: Vec<(String, String)>,
}

impl ExtensionLoader {
    fn cached(&self, name: &str) -> Option<String> {
        self.transpiled
            .iter()
            .find(|(path, _)| path == name)
            .map(|(_, source)| source.clone())
    }
}

impl rquickjs::loader::Loader for ExtensionLoader {
    fn load<'js>(
        &mut self,
        ctx: &rquickjs::Ctx<'js>,
        name: &str,
        _attributes: Option<rquickjs::loader::ImportAttributes<'js>>,
    ) -> JsResult<Module<'js, rquickjs::module::Declared>> {
        if let Some(specifier) = name.strip_prefix("virtual:") {
            if let Some(source) = virtual_module_source(specifier) {
                return Module::declare(ctx.clone(), name, source);
            }
            return Err(rquickjs::Error::Unknown);
        }
        if let Some(source) = self.cached(name) {
            return Module::declare(ctx.clone(), name, source);
        }
        let bytes = std::fs::read(name).map_err(|_| rquickjs::Error::Unknown)?;
        let source =
            match crate::transpile::transpile(Path::new(name), &String::from_utf8_lossy(&bytes)) {
                Ok(source) => source,
                Err(err) => return Err(throw_js_error(ctx, &err.to_string())),
            };
        let source = if crate::cjs::detect(&source) == crate::cjs::ModuleFormat::Cjs {
            crate::cjs::wrap(Path::new(name), &source)
        } else {
            source
        };
        self.transpiled.push((name.to_string(), source.clone()));
        Module::declare(ctx.clone(), name, source)
    }
}
