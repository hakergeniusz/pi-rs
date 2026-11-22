//! `CommonJS` interop for extension modules (the E4 deviation closing the
//! deferred item in `docs/EXTENSIONS.md` §6.3: "ESM-only first — upstream
//! extensions are ESM; CJS interop deferred").
//!
//! Upstream loads every extension through jiti
//! (`core/extensions/loader.ts:571-602`), which transparently accepts both
//! module formats because jiti's `require`/`import` bridge handles
//! `module.exports`, `exports.x`, and `require()` calls. The port's engine
//! declares modules as native ESM, so a CJS source needs a small JS shim
//! evaluated by rquickjs before it reaches that path. This module produces
//! that shim: it wraps the CJS body with `module`/`exports`/`require`
//! bindings, turns each *static* `require("spec")` into an ESM
//! `import * as … from "spec"`, and exports `module.exports` as the default
//! factory the engine expects (`loader.ts:593-597`).
//!
//! The generated imports go through the same
//! [`crate::resolver::ExtensionResolver`] / virtual-module table as ordinary
//! ESM imports, and each wrapped module exports a `__piCjs` marker so
//! `require` can return `module.exports` for CJS dependencies (and the
//! namespace object for ESM ones), mirroring jiti's interop shape.
//!
//! The invalid-module diagnostic is upstream's
//! "Extension does not export a valid factory function: {path}"
//! (`loader.ts:659`); [`invalid_factory_error`] is used by the CLI loading
//! path so a CJS module whose export is not a function reports identically.
//! Unresolvable `require` specifiers reuse [`crate::resolver`]'s exact ESM
//! resolution error strings rather than inventing another message. The
//! resolver also folds statically resolvable `require(expr)` arguments into
//! literal requires before this shim runs, so computed specifiers share the
//! static-import namespace and only genuinely dynamic expressions keep the
//! unresolved-module error.

use std::fmt::Write as _;
use std::path::Path;

/// A module's format as detected from its JavaScript source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleFormat {
    /// Native ESM (`import`/`export`); the engine declares it as-is.
    Esm,
    /// `CommonJS` (`module.exports`, `exports.x`, `require(...)`); needs the
    /// shim from [`wrap`] before declaration.
    Cjs,
}

/// The outcome of scanning one source: format markers plus the static
/// `require` specifiers found in code (strings and comments are ignored).
#[derive(Debug)]
struct Scan {
    esm: bool,
    cjs: bool,
    requires: Vec<String>,
}

impl Scan {
    fn format(&self) -> ModuleFormat {
        if self.esm {
            ModuleFormat::Esm
        } else if self.cjs {
            ModuleFormat::Cjs
        } else {
            // No format markers: leave the source to the engine so its
            // MissingFactory error stays the upstream diagnostic.
            ModuleFormat::Esm
        }
    }
}

/// Detects a CJS module from its (already TypeScript-stripped) JavaScript.
///
/// A single `import`/`export` keyword wins over CJS markers, matching jiti's
/// preference for native ESM when a file mixes both. The scan skips string,
/// template, and comment bodies so prose like `"module.exports"` or
/// `// require("x")` never flips the format.
pub fn detect(js: &str) -> ModuleFormat {
    scan(js).format()
}

fn scan(source: &str) -> Scan {
    let chars: Vec<char> = source.chars().collect();
    let mut result = Scan {
        esm: false,
        cjs: false,
        requires: Vec::new(),
    };
    let mut index = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut line_comment = false;
    let mut block_comment = false;
    while index < chars.len() {
        let current = chars[index];
        if line_comment {
            if current == '\n' {
                line_comment = false;
            }
            index += 1;
            continue;
        }
        if block_comment {
            if current == '*' && chars.get(index + 1) == Some(&'/') {
                block_comment = false;
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        if let Some(delimiter) = quote {
            if escaped {
                escaped = false;
            } else if current == '\\' {
                escaped = true;
            } else if current == delimiter {
                quote = None;
            }
            index += 1;
            continue;
        }
        if current == '/' && chars.get(index + 1) == Some(&'/') {
            line_comment = true;
            index += 2;
            continue;
        }
        if current == '/' && chars.get(index + 1) == Some(&'*') {
            block_comment = true;
            index += 2;
            continue;
        }
        if matches!(current, '\'' | '"' | '`') {
            quote = Some(current);
            index += 1;
            continue;
        }
        if is_ident_start(current) {
            let start = index;
            while index < chars.len() && is_ident_continue(chars[index]) {
                index += 1;
            }
            let word: String = chars[start..index].iter().collect();
            match word.as_str() {
                "import" | "export" => result.esm = true,
                "module" if starts_with_at(&chars, index, ".exports") => result.cjs = true,
                "exports" if matches!(peek_non_ws(&chars, index), Some('.' | '[')) => {
                    result.cjs = true;
                }
                "require" if peek_non_ws(&chars, index) == Some('(') => {
                    result.cjs = true;
                    if let Some((specifier, end)) = literal_at(&chars, index)
                        && !result.requires.contains(&specifier)
                    {
                        result.requires.push(specifier);
                        index = end;
                    }
                }
                _ => {}
            }
            continue;
        }
        index += 1;
    }
    result
}

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

fn peek_non_ws(chars: &[char], index: usize) -> Option<char> {
    chars.get(skip_whitespace(chars, index)).copied()
}

fn starts_with_at(chars: &[char], index: usize, text: &str) -> bool {
    let at = skip_whitespace(chars, index);
    text.chars()
        .enumerate()
        .all(|(offset, expected)| chars.get(at + offset) == Some(&expected))
}

/// Captures the simple quoted literal in `require("spec")`. Escaped or
/// computed specifiers are left to the runtime `require`, which reports the
/// resolver's unresolved-module error (`literal_at` returns `None` for them).
fn literal_at(chars: &[char], index: usize) -> Option<(String, usize)> {
    let mut at = skip_whitespace(chars, index);
    if chars.get(at) != Some(&'(') {
        return None;
    }
    at = skip_whitespace(chars, at + 1);
    let delimiter = *chars.get(at)?;
    if delimiter != '\'' && delimiter != '"' {
        return None;
    }
    let start = at + 1;
    let mut cursor = start;
    while cursor < chars.len() {
        match chars[cursor] {
            character if character == delimiter => {
                let literal: String = chars[start..cursor].iter().collect();
                if literal.contains('\\') {
                    return None;
                }
                return Some((literal, cursor + 1));
            }
            '\\' => return None,
            _ => cursor += 1,
        }
    }
    None
}

/// Wraps a CJS source into an ESM module whose default export is
/// `module.exports`. `js` must already be JavaScript (TypeScript stripped by
/// [`crate::transpile`]); `path` supplies `__filename`/`__dirname`.
///
/// Static `require` specifiers become ESM imports, so they share the
/// resolver's virtual-module table with ordinary `import` statements. The
/// runtime `require` reads a null-prototype table of those imported
/// namespaces: a wrapped CJS dependency contributes its `module.exports`
/// (marked `__piCjs`), an ESM dependency contributes its namespace object,
/// as under jiti. A specifier outside the table throws the exact error the
/// ESM resolver uses for that specifier.
#[allow(clippy::too_many_lines)] // the shim is one JS source; split only when extending it
pub fn wrap(path: &Path, js: &str) -> String {
    let details = scan(js);
    let mut out = String::new();
    for (index, specifier) in details.requires.iter().enumerate() {
        let _ = writeln!(
            out,
            "import * as __piCjsDep{index} from {};",
            js_string(specifier)
        );
    }
    out.push_str("const __piCjsModule = { exports: {} };\n");
    out.push_str("const module = __piCjsModule;\n");
    out.push_str("let exports = module.exports;\n");
    let filename = path.to_string_lossy();
    let dirname = path
        .parent()
        .map_or_else(String::new, |parent| parent.to_string_lossy().into_owned());
    let _ = writeln!(out, "const __filename = {};", js_string(&filename));
    let _ = writeln!(out, "const __dirname = {};", js_string(&dirname));
    out.push_str("module.filename = __filename;\n");
    out.push_str("module.paths = [];\n");
    out.push_str("const __piCjsDeps = Object.create(null);\n");
    for (index, specifier) in details.requires.iter().enumerate() {
        let _ = writeln!(
            out,
            "__piCjsDeps[{}] = __piCjsDep{index};",
            js_string(specifier)
        );
    }
    out.push_str("const require = (name) => {\n");
    out.push_str("  if (Object.prototype.hasOwnProperty.call(__piCjsDeps, name)) {\n");
    out.push_str("    const ns = __piCjsDeps[name];\n");
    out.push_str("    return ns && ns.__piCjs === true ? ns.__piCjsExports : ns;\n");
    out.push_str("  }\n");
    let _ = writeln!(out, "  throw new Error({});", unresolved_error_expression());
    out.push_str("};\n");
    out.push_str("module.require = require;\n\n");
    // A shebang is only legal as the first line of a script, so drop it
    // before splicing the body after the generated imports/declarations.
    let body = js.strip_prefix("#!").map_or(js, |rest| {
        rest.split_once('\n').map_or("", |(_, tail)| tail)
    });
    out.push_str(body);
    if !body.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("export const __piCjs = true;\n");
    // `require` needs the raw exports value even when it is not a function
    // (`exports.x = …` helpers); the default stays factory-only for the
    // engine's extension entry check (`loader.ts:593-597`).
    out.push_str("export const __piCjsExports = module.exports;\n");
    // Upstream checks `typeof factory !== "function"` before using the
    // jiti import (`loader.ts:593-597`); exporting `undefined` for a
    // non-function makes the engine's MissingFactory path report the same
    // invalid-factory diagnostic instead of a conversion error.
    out.push_str(
        "export default typeof module.exports === \"function\" ? module.exports : undefined;\n",
    );
    out
}

/// Prepares a file as an *entry* module for the CLI loading path
/// ([`crate::loader::load_paths`]): `Some(wrapped)` for a CJS file, `None`
/// when the engine's existing ESM path should read and transpile it.
///
/// The engine's inline-source path skips transpilation, so this helper runs
/// TypeScript stripping itself before the format check. Read/transpile
/// failures return `None` and stay the engine's error, preserving the
/// existing diagnostic and avoiding a duplicate parse.
pub fn entry_source(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let js = crate::transpile::transpile(path, &String::from_utf8_lossy(&bytes)).ok()?;
    // Shallow `require(expr)` folding lives in the resolver so entry and
    // imported modules share one implementation.
    let js = crate::resolver::fold_require_expressions(&js);
    (detect(&js) == ModuleFormat::Cjs).then(|| wrap(path, &js))
}

/// Upstream's invalid-factory diagnostic (`loader.ts:659`), used by the
/// loading path when a CJS module's `module.exports` is not a function.
pub fn invalid_factory_error(path: &str) -> String {
    format!("Extension does not export a valid factory function: {path}")
}

fn js_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

/// Builds the JS expression for the resolver's exact unresolved-module
/// message, using a sentinel so the strings stay owned by `resolver.rs`.
fn unresolved_error_expression() -> String {
    const SENTINEL: &str = "\u{1}";
    let generic = crate::resolver::unavailable_module_message(SENTINEL);
    let node_placeholder = format!("node:{SENTINEL}");
    let node = crate::resolver::unavailable_module_message(&node_placeholder);
    let split = |text: &str, placeholder: &str| -> (String, String) {
        let mut parts = text.split(placeholder);
        let before = parts.next().unwrap_or_default().to_string();
        let after = parts.next().unwrap_or_default().to_string();
        (before, after)
    };
    let (generic_before, generic_after) = split(&generic, SENTINEL);
    let (node_before, node_after) = split(&node, &node_placeholder);
    format!(
        "name.startsWith(\"node:\") ? {} + name + {} : {} + name + {}",
        js_string(&node_before),
        js_string(&node_after),
        js_string(&generic_before),
        js_string(&generic_after),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_cjs_markers_and_ignores_prose() {
        assert_eq!(
            detect("module.exports = function () {};"),
            ModuleFormat::Cjs
        );
        assert_eq!(detect("exports.tag = 1;\n"), ModuleFormat::Cjs);
        assert_eq!(detect("const x = require('typebox');"), ModuleFormat::Cjs);
        assert_eq!(detect("export default function () {};"), ModuleFormat::Esm);
        // Strings and comments never flip the format.
        assert_eq!(
            detect("const text = \"module.exports = 1\"; // require('x')\n"),
            ModuleFormat::Esm
        );
        // A single ESM marker wins even after CJS markers.
        assert_eq!(
            detect("module.exports = 1;\nexport const named = 2;"),
            ModuleFormat::Esm
        );
    }

    #[test]
    fn wrap_exposes_module_exports_and_rewrites_static_requires() {
        let wrapped = wrap(
            Path::new("/exts/main.js"),
            "const { Type } = require(\"typebox\");\nconst helper = require('./helper.js');\nmodule.exports = (pi) => pi.on('e', () => Type.String({}) && helper);\n",
        );
        assert!(
            wrapped.contains("import * as __piCjsDep0 from \"typebox\";"),
            "{wrapped}"
        );
        assert!(
            wrapped.contains("import * as __piCjsDep1 from \"./helper.js\";"),
            "{wrapped}"
        );
        assert!(
            wrapped.contains("let exports = module.exports;"),
            "{wrapped}"
        );
        assert!(
            wrapped.contains("const __dirname = \"/exts\";"),
            "{wrapped}"
        );
        assert!(
            wrapped.contains("export const __piCjs = true;"),
            "{wrapped}"
        );
        assert!(
            wrapped.ends_with(
                "export default typeof module.exports === \"function\" ? module.exports : undefined;\n"
            ),
            "{wrapped}"
        );
        assert!(
            wrapped.contains("ns.__piCjs === true ? ns.__piCjsExports : ns"),
            "{wrapped}"
        );
        assert!(
            wrapped.contains("export const __piCjsExports = module.exports;"),
            "{wrapped}"
        );
        // The original body is preserved verbatim, before the exports.
        assert!(wrapped.contains("module.exports = (pi) =>"), "{wrapped}");
    }

    #[test]
    fn require_errors_reuse_the_esm_resolver_wording() {
        let generic = crate::resolver::unavailable_module_message("unknown-pkg");
        let node = crate::resolver::unavailable_module_message("node:sqlite");
        let expression = unresolved_error_expression();
        assert!(expression.contains("startsWith(\"node:\")"), "{expression}");
        // Both branches are assembled from the resolver's own strings.
        assert!(generic.contains("unknown-pkg"));
        assert!(node.contains("node:sqlite"));
        assert_eq!(
            invalid_factory_error("/exts/bad.js"),
            "Extension does not export a valid factory function: /exts/bad.js"
        );
    }

    #[test]
    fn computed_requires_are_not_statically_imported() {
        let wrapped = wrap(
            Path::new("ext.js"),
            "const name = process.argv[2];\nmodule.exports = require(name);\n",
        );
        assert!(!wrapped.contains("import * as"), "{wrapped}");
        assert!(
            wrapped.contains("export default typeof module.exports"),
            "{wrapped}"
        );
    }
}
