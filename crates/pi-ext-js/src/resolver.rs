//! Module resolution for extension imports.
//!
//! Ports the three upstream resolution surfaces it replaces (E1 subset, §5.4
//! of `docs/EXTENSIONS.md`): the virtual-module table
//! (`core/extensions/virtual-modules.ts:14-42` — bundled modules every
//! extension may import), the relative-path resolution of the extension
//! directory (jiti), and the entry rules (`loader.ts:748-750`: `.ts`/`.js`
//! only; `loader.ts:761-791`: `index.ts` before `index.js`).
//!
//! E4 adds the `node:*` and host-package virtual modules served by [`node`],
//! plus the `node_modules` walk upstream delegates to Node/jiti through
//! `createRequire` (`core/extensions/loader.ts:42, 78-89`). A bare specifier
//! is looked up in `node_modules` directories from the importing file's
//! directory upwards; the package's `main`/`exports` string target (or its
//! `index.ts`/`index.js`) is then resolved with the same entry rules as a
//! relative import. Virtual modules and aliases keep priority, matching
//! upstream's `getAliases()` table (`loader.ts:72-131`).
//!
//! Genuinely missing modules still error with the documented E1 explanation
//! verbatim (`docs/EXTENSIONS.md` §6.3), for compatibility with the existing
//! diagnostic; only the resolution path behind it changed. The same table
//! also serves CJS `require()` calls: [`crate::cjs::wrap`] lowers each static
//! require into an ESM import, so both formats share this resolver and the
//! virtual modules (upstream's jiti bridge, `core/extensions/loader.ts:42,
//! 571-602`). [`fold_require_expressions`] lifts dynamically computed but
//! statically resolvable `require(...)` specifiers into that same static path;
//! expressions that cannot be folded keep the original E1 error.
//!
//! Remaining delta from full Node/jiti resolution: no `exports` patterns or
//! conditional-target arrays beyond the string/`default`/`import` subset,
//! no `imports`/`tsconfigPaths`, no `package.json` `type: module` distinction
//! (both `.ts` and `.js` are transpiled by the same oxc strip), no `.mjs`,
//! `.cjs` or `.json` extensions, and no symlink/realpath-aware caching
//! (upstream's pnpm case, `test/suite/regressions/8092`).

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

/// Splits a bare specifier into its package name and subpath. Scoped names
/// keep both segments (`@scope/pkg`), matching Node's package boundary; a
/// relative or absolute specifier, a backslash, and `..` segments are not
/// package names and return `None`.
fn package_parts(specifier: &str) -> Option<(&str, &str)> {
    if specifier.is_empty() || specifier.starts_with('.') || specifier.starts_with('/') {
        return None;
    }
    if specifier.contains('\\') {
        return None;
    }
    let name_end = if let Some(scoped) = specifier.strip_prefix('@') {
        let first = scoped.find('/')?;
        let second = scoped[first + 1..]
            .find('/')
            .map_or(scoped.len(), |offset| first + 1 + offset);
        if first == 0 || second == first + 1 {
            return None;
        }
        1 + second
    } else {
        specifier.find('/').unwrap_or(specifier.len())
    };
    let (name, rest) = specifier.split_at(name_end);
    let subpath = rest.strip_prefix('/').unwrap_or(rest);
    if subpath
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
        && !subpath.is_empty()
    {
        return None;
    }
    Some((name, subpath))
}

fn package_json_field(dir: &Path, field: &str) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(dir.join("package.json")).ok()?;
    let without_bom = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let value: serde_json::Value = serde_json::from_str(without_bom).ok()?;
    value.get(field).cloned()
}

/// The string target of one `exports` entry. Node's conditional objects are
/// limited here to the string/condition-string subset; arrays and wildcard
/// patterns are documented as a remaining delta in the module docs.
fn exports_target<'a>(exports: &'a serde_json::Value, subpath: &str) -> Option<&'a str> {
    if subpath.is_empty()
        && let Some(target) = exports.as_str()
    {
        return Some(target);
    }
    let key = if subpath.is_empty() {
        ".".to_string()
    } else {
        format!("./{subpath}")
    };
    let target = exports.as_object()?.get(&key)?;
    if let Some(text) = target.as_str() {
        return Some(text);
    }
    let conditions = target.as_object()?;
    for condition in ["import", "default", "node", "require"] {
        if let Some(text) = conditions
            .get(condition)
            .and_then(serde_json::Value::as_str)
        {
            return Some(text);
        }
    }
    None
}

fn resolve_package_file(package_dir: &Path, subpath: &str) -> Option<PathBuf> {
    if let Some(exports) = package_json_field(package_dir, "exports")
        && let Some(target) = exports_target(&exports, subpath)
        && let Some(resolved) = resolve_relative(package_dir, target.trim_start_matches("./"))
    {
        return Some(resolved);
    }
    if subpath.is_empty() {
        if let Some(main) = package_json_field(package_dir, "main")
            && let Some(main) = main.as_str()
            && let Some(resolved) = resolve_relative(package_dir, main)
        {
            return Some(resolved);
        }
        return resolve_relative(package_dir, ".");
    }
    resolve_relative(package_dir, subpath)
}

/// Resolves a bare specifier by walking `node_modules` from `base_dir`
/// through its ancestors (the Node/jiti algorithm upstream gets from
/// `createRequire`, `loader.ts:42`). The nearest installed package wins;
/// inside one package `exports`, then `main`, then `index.ts`/`index.js`
/// decide the file. Returns `None` for genuinely missing modules so the
/// caller raises the unchanged E1 explanation.
pub fn resolve_bare(base_dir: &Path, specifier: &str) -> Option<PathBuf> {
    let (name, subpath) = package_parts(specifier)?;
    for ancestor in base_dir.ancestors() {
        let package_dir = ancestor.join("node_modules").join(name);
        if package_dir.is_dir()
            && let Some(resolved) = resolve_package_file(&package_dir, subpath)
        {
            return Some(resolved);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Dynamic `require` lifting.
//
// Upstream's jiti resolves `require(expr)` at runtime. This host's CJS shim
// only creates ESM imports for literal specifiers, so the source is folded
// first: a `require(...)` argument built from string literals, simple `const`
// string bindings and `+` becomes the equivalent literal require (and thus
// the same static import/namespace). Anything else is left untouched, so the
// shim's existing unresolved-module error stays byte-identical.
// ---------------------------------------------------------------------------

fn is_ident_start(character: char) -> bool {
    character.is_ascii_alphabetic() || character == '_' || character == '$'
}

fn is_ident_continue(character: char) -> bool {
    is_ident_start(character) || character.is_ascii_digit()
}

fn skip_whitespace(chars: &[char], mut index: usize) -> usize {
    while index < chars.len() && chars[index].is_whitespace() {
        index += 1;
    }
    index
}

fn skip_quoted(chars: &[char], start: usize) -> Option<usize> {
    let delimiter = chars.get(start).copied()?;
    let mut index = start + 1;
    let mut escaped = false;
    while index < chars.len() {
        match (chars[index], escaped) {
            (character, false) if character == delimiter => return Some(index + 1),
            ('\\', false) => escaped = true,
            (_, true) => escaped = false,
            _ => {}
        }
        index += 1;
    }
    None
}

fn skip_comment(chars: &[char], index: usize) -> Option<usize> {
    match chars.get(index + 1) {
        Some('/') => {
            let mut at = index + 2;
            while at < chars.len() && chars[at] != '\n' {
                at += 1;
            }
            Some(at)
        }
        Some('*') => {
            let mut at = index + 2;
            while at + 1 < chars.len() && !(chars[at] == '*' && chars[at + 1] == '/') {
                at += 1;
            }
            Some((at + 2).min(chars.len()))
        }
        _ => None,
    }
}

/// Finds the closing parenthesis of `require(...)`; `argument_end` excludes
/// it and `close` is the index just after it.
fn call_argument(chars: &[char], start: usize) -> Option<(usize, usize)> {
    let mut index = start;
    let mut depth = 0_usize;
    while index < chars.len() {
        match chars[index] {
            '\'' | '"' | '`' => index = skip_quoted(chars, index)?,
            '/' if matches!(chars.get(index + 1), Some('/' | '*')) => {
                index = skip_comment(chars, index)?;
            }
            '(' | '[' | '{' => {
                depth += 1;
                index += 1;
            }
            ')' if depth == 0 => return Some((index, index + 1)),
            ')' | ']' | '}' => {
                depth = depth.checked_sub(1)?;
                index += 1;
            }
            _ => index += 1,
        }
    }
    None
}

/// Where a `const NAME = <expression>` initializer ends (semicolon, comma,
/// newline at depth zero, or EOF).
fn initializer_end(chars: &[char], start: usize) -> usize {
    let mut index = start;
    let mut depth = 0_usize;
    while index < chars.len() {
        match chars[index] {
            '\'' | '"' | '`' => match skip_quoted(chars, index) {
                Some(next) => index = next,
                None => return chars.len(),
            },
            '/' if matches!(chars.get(index + 1), Some('/' | '*')) => {
                index = skip_comment(chars, index).unwrap_or(chars.len());
            }
            '(' | '[' | '{' => {
                depth += 1;
                index += 1;
            }
            ')' | ']' | '}' | ';' | ',' | '\n' if depth == 0 => return index,
            ')' | ']' | '}' => {
                depth -= 1;
                index += 1;
            }
            _ => index += 1,
        }
    }
    index
}

fn wraps_single_expression(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    if chars.first() != Some(&'(') || chars.last() != Some(&')') {
        return false;
    }
    let mut index = 0_usize;
    let mut depth = 0_i32;
    while index < chars.len() {
        match chars[index] {
            '\'' | '"' | '`' => match skip_quoted(&chars, index) {
                Some(next) => index = next,
                None => return false,
            },
            '(' => {
                depth += 1;
                index += 1;
            }
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
                index += 1;
            }
            _ => index += 1,
        }
    }
    depth == 0
}

/// Folds the deliberately small expression grammar the port can resolve
/// without a JS evaluator: string literals, literal templates, `const`
/// string bindings already seen in the file, parentheses and `+`.
fn fold_expression(expression: &str, consts: &[(String, String)]) -> Option<String> {
    let text = expression.trim();
    if text.is_empty() {
        return None;
    }
    if wraps_single_expression(text) {
        return fold_expression(&text[1..text.len() - 1], consts);
    }
    let first = text.chars().next()?;
    if matches!(first, '\'' | '"') && text.len() >= 2 && text.ends_with(first) {
        let inner = &text[1..text.len() - 1];
        return (!inner.contains('\\')).then(|| inner.to_string());
    }
    if first == '`' && text.len() >= 2 && text.ends_with('`') {
        let inner = &text[1..text.len() - 1];
        return (!inner.contains("${") && !inner.contains('\\')).then(|| inner.to_string());
    }
    if text.chars().all(is_ident_continue) {
        return consts
            .iter()
            .rev()
            .find(|(name, _)| name == text)
            .map(|(_, value)| value.clone());
    }
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0_usize;
    let mut depth = 0_i32;
    let mut plus = None;
    while index < chars.len() {
        match chars[index] {
            '\'' | '"' | '`' => index = skip_quoted(&chars, index)?,
            '(' | '[' | '{' => {
                depth += 1;
                index += 1;
            }
            ')' | ']' | '}' => {
                depth -= 1;
                index += 1;
            }
            '+' if depth == 0 => plus = Some(index),
            _ => index += 1,
        }
    }
    if let Some(at) = plus {
        let left: String = chars[..at].iter().collect();
        let right: String = chars[at + 1..].iter().collect();
        return Some(fold_expression(&left, consts)? + &fold_expression(&right, consts)?);
    }
    None
}

/// Rewrites statically resolvable `require(expr)` arguments into string
/// literals. Strings, templates and comments are skipped so prose and
/// already-static requires are never touched; unsupported expressions keep
/// their source verbatim and therefore the original error at runtime.
#[allow(clippy::too_many_lines)] // one lexical pass; split only when extending the grammar
pub fn fold_require_expressions(js: &str) -> String {
    let chars: Vec<char> = js.chars().collect();
    let mut consts: Vec<(String, String)> = Vec::new();
    let mut replacements: Vec<(usize, usize, String)> = Vec::new();
    let mut index = 0_usize;
    while index < chars.len() {
        match chars[index] {
            '\'' | '"' | '`' => {
                index = skip_quoted(&chars, index).unwrap_or(chars.len());
                continue;
            }
            '/' if matches!(chars.get(index + 1), Some('/' | '*')) => {
                index = skip_comment(&chars, index).unwrap_or(chars.len());
                continue;
            }
            _ => {}
        }
        if !is_ident_start(chars[index]) {
            index += 1;
            continue;
        }
        let start = index;
        while index < chars.len() && is_ident_continue(chars[index]) {
            index += 1;
        }
        let word: String = chars[start..index].iter().collect();
        let preceded_by_dot = start > 0 && chars[start - 1] == '.';
        if preceded_by_dot {
            continue;
        }
        if word == "const" {
            let name_at = skip_whitespace(&chars, index);
            if let Some(&first) = chars.get(name_at)
                && is_ident_start(first)
            {
                let mut name_end = name_at + 1;
                while name_end < chars.len() && is_ident_continue(chars[name_end]) {
                    name_end += 1;
                }
                let equals = skip_whitespace(&chars, name_end);
                if chars.get(equals) == Some(&'=') && !matches!(chars.get(equals + 1), Some('=')) {
                    let value_at = skip_whitespace(&chars, equals + 1);
                    let value_end = initializer_end(&chars, value_at);
                    let value: String = chars[value_at..value_end].iter().collect();
                    if let Some(value) = fold_expression(&value, &consts) {
                        let name: String = chars[name_at..name_end].iter().collect();
                        consts.push((name, value));
                        index = value_end;
                        continue;
                    }
                }
            }
        }
        if word == "require" {
            let open = skip_whitespace(&chars, index);
            if chars.get(open) == Some(&'(')
                && let Some((argument_end, close)) = call_argument(&chars, open + 1)
            {
                let argument: String = chars[open + 1..argument_end].iter().collect();
                if let Some(literal) = fold_expression(&argument, &consts) {
                    let quoted = serde_json::to_string(&literal).unwrap_or_else(|_| "\"\"".into());
                    replacements.push((open + 1, argument_end, quoted));
                    index = close;
                }
            }
        }
    }
    let mut out = String::new();
    let mut at = 0_usize;
    for (start, end, replacement) in replacements {
        out.extend(chars[at..start].iter().copied());
        out.push_str(&replacement);
        at = end;
    }
    out.extend(chars[at..].iter().copied());
    out
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
        let base_dir = Path::new(base)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let is_relative = name.starts_with("./") || name.starts_with("../");
        if is_relative || name.starts_with('/') {
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
        // Bare specifiers walk ancestor `node_modules`, like upstream's
        // `createRequire`. Host packages we cannot serve keep their E1 error
        // instead of being shadowed by an installed copy (jiti aliases win).
        if !name.starts_with("node:")
            && !is_unavailable_host_module(name)
            && let Some(resolved) = resolve_bare(&base_dir, name)
        {
            return Ok(resolved.to_string_lossy().into_owned());
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
        let source = fold_require_expressions(&source);
        let source = if crate::cjs::detect(&source) == crate::cjs::ModuleFormat::Cjs {
            crate::cjs::wrap(Path::new(name), &source)
        } else {
            source
        };
        self.transpiled.push((name.to_string(), source.clone()));
        Module::declare(ctx.clone(), name, source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HostCore, JsHost};

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir parent");
        }
        std::fs::write(path, contents).expect("write");
    }

    fn test_host() -> JsHost {
        JsHost::new(HostCore {
            cwd: "/tmp".to_string(),
            mode: "print".to_string(),
            settings: serde_json::json!({}),
            ..Default::default()
        })
        .expect("host")
    }

    #[test]
    fn node_modules_resolution_walks_ancestors_and_reads_scoped_manifests() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let nested = root.join("project").join("nested");
        write(
            &root.join("node_modules/ancestor-pkg/index.ts"),
            "export const tag: string = \"ancestor\";\n",
        );
        write(
            &root.join("node_modules/@scope/tool/package.json"),
            r#"{"main":"dist/main.js"}"#,
        );
        write(
            &root.join("node_modules/@scope/tool/dist/main.js"),
            "module.exports = {};\n",
        );
        write(
            &root.join("node_modules/empty/index.json"),
            "{\"not\":\"an entry\"}",
        );
        write(
            &nested.join("node_modules/local-pkg/index.js"),
            "module.exports = {};\n",
        );

        assert_eq!(
            resolve_bare(&nested, "local-pkg"),
            Some(nested.join("node_modules/local-pkg/index.js"))
        );
        assert_eq!(
            resolve_bare(&nested, "ancestor-pkg"),
            Some(root.join("node_modules/ancestor-pkg/index.ts"))
        );
        assert_eq!(
            resolve_bare(&nested, "@scope/tool"),
            Some(root.join("node_modules/@scope/tool/dist/main.js"))
        );
        assert_eq!(resolve_bare(&nested, "empty"), None);
        assert_eq!(resolve_bare(&nested, "not-installed"), None);
        assert_eq!(resolve_bare(&nested, "../relative"), None);
        assert_eq!(resolve_bare(&nested, "@scope/tool/../../escape"), None);
        // Genuinely missing modules keep the E1 wording verbatim.
        assert_eq!(
            unavailable_module_message("not-installed"),
            "Module \"not-installed\" could not be resolved by the extensions host (E1 supports relative paths and the bundled virtual modules; node_modules resolution lands in E4)"
        );
    }

    #[test]
    fn node_modules_resolves_exports_strings_and_subpaths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let package = dir.path().join("node_modules/exported");
        write(
            &package.join("package.json"),
            r#"{"exports":{".":"./dist/index.js","./util":{"import":"./src/util.ts","default":"./src/util.js"}},"main":"legacy.js"}"#,
        );
        write(&package.join("dist/index.js"), "module.exports = {};\n");
        write(
            &package.join("src/util.ts"),
            "export const tag: string = \"u\";\n",
        );
        write(&package.join("src/util.js"), "module.exports = {};\n");
        write(&package.join("raw/extra.js"), "module.exports = {};\n");

        assert_eq!(
            resolve_bare(dir.path(), "exported"),
            Some(package.join("dist/index.js"))
        );
        assert_eq!(
            resolve_bare(dir.path(), "exported/util"),
            Some(package.join("src/util.ts"))
        );
        // With no matching exports key, fall back to the file rules inside
        // the package (Node's own direct-subpath behavior).
        assert_eq!(
            resolve_bare(dir.path(), "exported/raw/extra"),
            Some(package.join("raw/extra.js"))
        );
        assert_eq!(resolve_bare(dir.path(), "exported/missing"), None);
    }

    #[test]
    fn fold_require_expressions_lifts_simple_specifiers() {
        let source = r#"
const PREFIX = "type" + "box";
const first = require(PREFIX);
const second = require('@scope/' + 'pkg');
const third = require(`node:path`);
const fourth = require(( "a" + "b" ));
const dynamic = require(globalThis.__name);
const note = "require('not-a-real-import')";
// require("comment-only")
"#;
        let folded = fold_require_expressions(source);
        assert!(folded.contains("require(\"typebox\")"), "{folded}");
        assert!(folded.contains("require(\"@scope/pkg\")"), "{folded}");
        assert!(folded.contains("require(\"node:path\")"), "{folded}");
        assert!(folded.contains("require(\"ab\")"), "{folded}");
        assert!(folded.contains("require(globalThis.__name)"), "{folded}");
        assert!(
            folded.contains("\"require('not-a-real-import')\""),
            "{folded}"
        );
        assert!(folded.contains("// require(\"comment-only\")"), "{folded}");
    }

    #[test]
    fn dynamic_require_and_node_modules_share_the_static_import_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            &dir.path().join("node_modules/pi-test-dep/index.js"),
            "exports.tag = \"from-node-modules\";\n",
        );
        let entry = dir.path().join("entry.js");
        write(
            &entry,
            "const name = \"pi-\" + \"test-dep\";\nconst dep = require(name);\nmodule.exports = function () { globalThis.__dyn = dep.tag; };\n",
        );
        let prepared = crate::cjs::entry_source(&entry).expect("prepared");
        assert!(
            prepared.contains("import * as __piCjsDep0 from \"pi-test-dep\""),
            "{prepared}"
        );
        let host = test_host();
        let index = host
            .begin_extension(&entry.to_string_lossy())
            .expect("begin");
        host.load_extension_module(index, &entry.to_string_lossy(), Some(&prepared))
            .expect("load");
        assert_eq!(
            host.eval_for_tests("globalThis.__dyn").expect("eval"),
            "from-node-modules"
        );
    }

    #[test]
    fn unsupported_dynamic_require_keeps_the_documented_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let entry = dir.path().join("entry.js");
        write(
            &entry,
            "module.exports = function () { return require(globalThis.__name); };\n",
        );
        let host = test_host();
        host.eval_for_tests("globalThis.__name = \"no-such-pkg\";")
            .expect("setup");
        let prepared = crate::cjs::entry_source(&entry).expect("prepared");
        assert!(!prepared.contains("import * as"), "{prepared}");
        let index = host
            .begin_extension(&entry.to_string_lossy())
            .expect("begin");
        let error = host
            .load_extension_module(index, &entry.to_string_lossy(), Some(&prepared))
            .expect_err("unresolved");
        assert!(
            error
                .to_string()
                .contains(&unavailable_module_message("no-such-pkg")),
            "{error}"
        );
    }
}
