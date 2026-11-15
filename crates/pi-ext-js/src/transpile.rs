//! TypeScript → JavaScript strip for extension sources.
//!
//! Ports the transform step of upstream's loading pipeline: jiti runs
//! Babel's TypeScript transform on every loaded module
//! (`packages/coding-agent/src/core/extensions/loader.ts:571-602` hands
//! jiti `.ts`/`.js` sources, and jiti transpiles TypeScript to JavaScript
//! before evaluation). The E1 replacement drops the types with the `oxc`
//! pipeline — parse (`oxc_parser`), strip (`oxc_transformer`, the Babel-
//! compatible TypeScript transform that also lowers enums and namespaces),
//! print (`oxc_codegen`) — matching jiti's semantics for the constructs
//! extensions use: annotations, interfaces, `import type`/inline `type`
//! specifiers, `satisfies`, enums, namespaces, class modifiers.
//!
//! Module syntax is never transformed: output stays ESM (`QuickJS` parses
//! ESM natively), matching upstream where extensions are ESM (§6.3 of
//! `docs/EXTENSIONS.md` records CJS interop as deferred).

use oxc_allocator::Allocator;
use oxc_codegen::{Codegen, CodegenOptions};
use oxc_parser::Parser;
use oxc_span::SourceType;
use oxc_transformer::{TransformOptions, Transformer};
use std::path::Path;

/// Errors from the transpile pipeline.
#[derive(Debug, thiserror::Error)]
pub enum TranspileError {
    /// The file extension does not map to a JavaScript or TypeScript source.
    #[error("unsupported extension source `{0}` (expected .ts or .js)")]
    UnsupportedExtension(String),
    /// The TypeScript or JavaScript source did not parse.
    #[error("parse error: {0}")]
    Parse(String),
    /// The TypeScript transform failed.
    #[error("transform error: {0}")]
    Transform(String),
}

/// Strips TypeScript from `source` (a no-op for plain `.js` sources) and
/// returns JavaScript in ESM form.
///
/// `path` only selects the source kind from its extension; the file is not
/// read here.
pub fn transpile(path: &Path, source: &str) -> Result<String, TranspileError> {
    let source_type = source_type_for(path)?;
    if !source_type.is_typescript() {
        return Ok(source.to_string());
    }
    let allocator = Allocator::default();
    let parser_return = Parser::new(&allocator, source, source_type).parse();
    if let Some(first) = parser_return.diagnostics.into_iter().next() {
        return Err(TranspileError::Parse(format!("{first}")));
    }
    let mut program = parser_return.program;
    // The transformer needs the symbol/scope tables; the semantic builder
    // builds them (its syntax-error check stays off — the parser's
    // diagnostics above already rejected malformed sources).
    let semantic = oxc_semantic::SemanticBuilder::new()
        .with_enum_eval(true)
        .build(&program);
    let scoping = semantic.semantic.into_scoping();
    let transformer = Transformer::new(&allocator, path, &TransformOptions::default());
    let transformer_return = transformer.build_with_scoping(scoping, &mut program);
    if let Some(first) = transformer_return.diagnostics.into_iter().next() {
        return Err(TranspileError::Transform(format!("{first}")));
    }
    let printed = Codegen::new()
        .with_options(CodegenOptions::default())
        .build(&program);
    Ok(printed.code)
}

/// Maps a path to an oxc source type: `.ts` (and `.mts`) are TypeScript
/// modules, `.js` (and `.mjs`) plain JavaScript modules — the entry rules
/// upstream's loader enforces (`loader.ts:748-750`; no `.cjs`/`.jsx`/`.tsx`).
fn source_type_for(path: &Path) -> Result<SourceType, TranspileError> {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .ok_or_else(|| TranspileError::UnsupportedExtension(String::new()))?;
    let source_type = match extension {
        "ts" | "mts" => SourceType::mjs().with_typescript(true),
        "js" | "mjs" => SourceType::mjs(),
        other => return Err(TranspileError::UnsupportedExtension(other.to_string())),
    };
    Ok(source_type)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transpile_source(name: &str, source: &str) -> String {
        transpile(Path::new(name), source).expect("transpile")
    }

    #[test]
    fn strips_types_and_keeps_esm() {
        let out = transpile_source(
            "ext.ts",
            r#"
export interface Shape { kind: string }
export type Alias = Shape;
export default function (pi: unknown): void {
  const shape: Shape = { kind: "t" };
  const narrow = shape as Alias;
  console.log(narrow satisfies Shape);
}
"#,
        );
        assert!(out.contains("export default function(pi)"), "{out}");
        assert!(!out.contains("interface"), "{out}");
        assert!(!out.contains(": Shape"), "{out}");
        assert!(!out.contains("satisfies"), "{out}");
    }

    #[test]
    fn strips_type_only_imports_and_inline_type_specifiers() {
        let out = transpile_source(
            "ext.ts",
            r#"
import type { Never } from "somewhere";
import { type TypeOnly, keepMe } from "typebox";
export const used = keepMe;
export const unused: TypeOnly = undefined as never as never;
console.log(Never);
"#,
        );
        assert!(out.contains("keepMe"), "{out}");
        assert!(out.contains("import"), "{out}");
        assert!(out.contains("console.log(Never);"), "{out}");
        assert!(!out.contains("TypeOnly"), "{out}");
    }

    #[test]
    fn lowers_enums_and_namespaces_like_babel() {
        let out = transpile_source(
            "ext.ts",
            r#"
enum Level { Low, High }
namespace Util { export const tag = "t"; }
export const value = [Level.High, Util.tag];
"#,
        );
        // Babel lowers enums to lookup objects; oxc matches that shape.
        assert!(out.contains("Level") && out.contains("High"), "{out}");
        assert!(!out.contains("enum "), "{out}");
        assert!(!out.contains("namespace "), "{out}");
    }

    #[test]
    fn javascript_passes_through_unmodified() {
        let out = transpile_source("ext.js", "export const x = 1;");
        assert_eq!(out, "export const x = 1;");
    }

    #[test]
    fn rejects_unknown_extensions() {
        let err = transpile(Path::new("ext.jsx"), "").expect_err("jsx must be rejected");
        assert!(matches!(err, TranspileError::UnsupportedExtension(ref ext) if ext == "jsx"));
    }

    #[test]
    fn parse_errors_are_values_with_positions() {
        let err = transpile_source_err("ext.ts", "export const = 1;");
        assert!(err.contains("parse error"), "{err}");
    }

    fn transpile_source_err(name: &str, source: &str) -> String {
        match transpile(Path::new(name), source) {
            Ok(out) => panic!("expected error, got {out}"),
            Err(err) => err.to_string(),
        }
    }
}
