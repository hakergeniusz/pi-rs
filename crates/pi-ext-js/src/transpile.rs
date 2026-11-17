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
//!
//! # Disk cache
//!
//! Upstream's loader creates jiti with only `moduleCache: false`
//! (`loader.ts:585-599`), leaving jiti 2.7.0's `fsCache` at its default
//! `true`, so transformed modules are reused from a filesystem cache across
//! runs. The `oxc` port initially re-transformed every source on every run —
//! measured at ~24 ms per file on a ~260-file extension set (~6 s per pi
//! start). [`transpile`] therefore mirrors upstream's effective behavior
//! with a content-addressed disk cache under the XDG cache root
//! (`$XDG_CACHE_HOME/pi-rs` else `~/.cache/pi-rs`, mirroring
//! `config.rs:84-101`; `pi-ext-js` cannot depend on `pi-coding-agent`
//! because it is feature-gated into it, so the resolver is mirrored here —
//! keep the two in sync). The cache key hashes the pipeline version, the
//! source path, and the full source, so edits and pipeline changes
//! invalidate naturally; entries are written atomically and any cache I/O
//! failure falls back to transpiling (the cache can never break a load).
//!
//! Deliberate deviation from jiti's on-disk format (recorded in
//! `HANDOFF.md`): one flat content-addressed file per source instead of
//! jiti's per-path cache files. Same reuse semantics, simpler invalidation.

use oxc_allocator::Allocator;
use oxc_codegen::{Codegen, CodegenOptions};
use oxc_parser::Parser;
use oxc_span::SourceType;
use oxc_transformer::{TransformOptions, Transformer};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Cache invalidation tag: bump when the transform pipeline's semantics
/// change (oxc upgrade, new transform options), never when only this file's
/// internals change.
const TRANSPILE_CACHE_VERSION: &str = "pi-rs-oxc-transpile-v1/oxc-0.158";

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
/// returns JavaScript in ESM form, reusing the process-lifetime disk cache
/// under the default XDG cache root.
///
/// `path` only selects the source kind from its extension; the file is not
/// read here.
pub fn transpile(path: &Path, source: &str) -> Result<String, TranspileError> {
    transpile_with_cache_dir(path, source, default_cache_dir().as_deref())
}

/// [`transpile`] against an explicit cache directory (`None` disables the
/// disk cache; the transform still runs). Tests and future host wiring use
/// this to pin or skip the cache location.
pub fn transpile_with_cache_dir(
    path: &Path,
    source: &str,
    cache_dir: Option<&Path>,
) -> Result<String, TranspileError> {
    let source_type = source_type_for(path)?;
    if !source_type.is_typescript() {
        return Ok(source.to_string());
    }
    let key = cache_key(path, source);
    if let Some(cache_dir) = cache_dir {
        if let Some(cached) = read_cache_entry(cache_dir, &key) {
            return Ok(cached);
        }
    }
    let printed = transpile_typescript(path, source, source_type)?;
    if let Some(cache_dir) = cache_dir {
        write_cache_entry(cache_dir, &key, &printed);
    }
    Ok(printed)
}

/// The default disk-cache directory: `<XDG cache root>/transpile`, where the
/// root is `$XDG_CACHE_HOME/pi-rs` else `~/.cache/pi-rs` (mirroring
/// `config.rs:84-101`). `None` when no home can be resolved.
fn default_cache_dir() -> Option<PathBuf> {
    let xdg = std::env::var("XDG_CACHE_HOME").ok();
    let home = std::env::var("HOME").ok();
    cache_root_for(xdg.as_deref(), home.as_deref()).map(|root| root.join("transpile"))
}

/// The pure core of [`default_cache_dir`]: the `pi-rs` cache root for the
/// explicit `$XDG_CACHE_HOME` value and home directory.
fn cache_root_for(xdg_value: Option<&str>, home_value: Option<&str>) -> Option<PathBuf> {
    let base = match xdg_value.filter(|value| !value.is_empty() && Path::new(value).is_absolute()) {
        Some(base) => PathBuf::from(base),
        None => PathBuf::from(home_value?).join(".cache"),
    };
    Some(base.join("pi-rs"))
}

/// The content-addressed cache key: pipeline version, source path, and full
/// source, so any edit or pipeline change lands on a fresh entry.
fn cache_key(path: &Path, source: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(TRANSPILE_CACHE_VERSION.as_bytes());
    hasher.update([0]);
    hasher.update(path.to_string_lossy().as_bytes());
    hasher.update([0]);
    hasher.update(source.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// The cached transpiled source for `key`, if a readable entry exists.
/// Entries carry their key on the first line; anything whose header does not
/// match (truncated write, wrong content, garbage) is a miss, so a bad entry
/// can never poison a load.
fn read_cache_entry(cache_dir: &Path, key: &str) -> Option<String> {
    let content = std::fs::read_to_string(cache_dir.join(format!("{key}.js"))).ok()?;
    content.strip_prefix(&format!("{key}\n")).map(str::to_owned)
}

/// Store a transpiled source atomically; best effort — a cache write failure
/// (unwritable disk, raced cleanup) must never fail the load.
fn write_cache_entry(cache_dir: &Path, key: &str, printed: &str) {
    if std::fs::create_dir_all(cache_dir).is_err() {
        return;
    }
    let temp = cache_dir.join(format!("{key}.{}.tmp", std::process::id()));
    if std::fs::write(&temp, format!("{key}\n{printed}")).is_ok() {
        let final_path = cache_dir.join(format!("{key}.js"));
        let _ = std::fs::rename(&temp, final_path);
    }
    let _ = std::fs::remove_file(&temp);
}

/// The `oxc` pipeline: parse, strip types, print ESM.
fn transpile_typescript(
    path: &Path,
    source: &str,
    source_type: SourceType,
) -> Result<String, TranspileError> {
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
        transpile_with_cache_dir(Path::new(name), source, None).expect("transpile")
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

    #[test]
    fn disk_cache_stores_one_entry_and_reuses_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = "export interface A { x: number }\nexport const a: A = { x: 1 };\n";
        let expected =
            transpile_with_cache_dir(Path::new("ext.ts"), source, None).expect("transpile");

        let first = transpile_with_cache_dir(Path::new("ext.ts"), source, Some(dir.path()))
            .expect("transpile");
        let entries = std::fs::read_dir(dir.path())
            .expect("read cache dir")
            .count();
        assert_eq!(first, expected);
        assert_eq!(entries, 1, "one content-addressed entry after a cold load");

        let second = transpile_with_cache_dir(Path::new("ext.ts"), source, Some(dir.path()))
            .expect("transpile");
        let entries = std::fs::read_dir(dir.path())
            .expect("read cache dir")
            .count();
        assert_eq!(second, expected);
        assert_eq!(entries, 1, "a warm load writes nothing new");
    }

    #[test]
    fn disk_cache_keys_distinguish_sources() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first =
            transpile_with_cache_dir(Path::new("ext.ts"), "export const a = 1;", Some(dir.path()))
                .expect("transpile");
        let second =
            transpile_with_cache_dir(Path::new("ext.ts"), "export const a = 2;", Some(dir.path()))
                .expect("transpile");
        assert_ne!(first, second);
        assert_eq!(
            std::fs::read_dir(dir.path())
                .expect("read cache dir")
                .count(),
            2,
            "one entry per distinct source"
        );
    }

    #[test]
    fn corrupt_cache_entry_falls_back_to_transpile() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = "export interface A { x: number }\nexport const a: A = { x: 1 };\n";
        let expected =
            transpile_with_cache_dir(Path::new("ext.ts"), source, None).expect("transpile");
        let key = cache_key(Path::new("ext.ts"), source);
        std::fs::write(dir.path().join(format!("{key}.js")), "totally not js").expect("seed cache");

        let out = transpile_with_cache_dir(Path::new("ext.ts"), source, Some(dir.path()))
            .expect("transpile");
        // A header mismatch is a miss: the entry is re-transpiled and
        // rewritten with a valid header, and the load never sees garbage.
        assert_eq!(out, expected);
        let seeded = std::fs::read_to_string(dir.path().join(format!("{key}.js"))).expect("entry");
        assert!(seeded.starts_with(&format!("{key}\n")), "{seeded}");
    }

    #[test]
    fn javascript_sources_are_never_cached() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out =
            transpile_with_cache_dir(Path::new("ext.js"), "export const x = 1;", Some(dir.path()))
                .expect("transpile");
        assert_eq!(out, "export const x = 1;");
        assert_eq!(
            std::fs::read_dir(dir.path())
                .expect("read cache dir")
                .count(),
            0,
            "passthrough sources write no cache entries"
        );
    }

    #[test]
    fn cache_root_resolves_xdg_like_config() {
        assert_eq!(
            cache_root_for(Some("/c"), Some("/h")).as_deref(),
            Some(Path::new("/c/pi-rs"))
        );
        assert_eq!(
            cache_root_for(Some(""), Some("/h")).as_deref(),
            Some(Path::new("/h/.cache/pi-rs"))
        );
        assert_eq!(
            cache_root_for(Some("relative/xdg"), Some("/h")).as_deref(),
            Some(Path::new("/h/.cache/pi-rs"))
        );
        assert_eq!(
            cache_root_for(None, Some("/h")).as_deref(),
            Some(Path::new("/h/.cache/pi-rs"))
        );
        assert_eq!(cache_root_for(None, None), None);
    }

    /// Timing report (run with `cargo test -p pi-ext-js --release
    /// transpile_cache_bench -- --ignored --nocapture`): transpiles a corpus
    /// of distinct TS sources cold and warm and prints both durations, so the
    /// cache's effect stays measurable without a flaky timing assert.
    #[test]
    #[ignore = "timing report, not an assertion"]
    fn transpile_cache_bench_reports_cold_and_warm() {
        let fixture_dir = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/tool-search"
        ));
        let mut sources: Vec<(String, String)> = Vec::new();
        collect_fixture_sources(fixture_dir, &mut sources);
        assert!(!sources.is_empty(), "no fixture sources found");
        // Widen the corpus so the report reflects a realistic extension set.
        for index in 0..240usize {
            let (name, body) = &sources[index % sources.len()];
            sources.push((
                format!("synthetic-{index}-{name}"),
                format!("{body}\nexport const synthetic{index} = {index};\n"),
            ));
        }

        let cold_dir = tempfile::tempdir().expect("tempdir");
        let started = std::time::Instant::now();
        for (name, source) in &sources {
            transpile_with_cache_dir(Path::new(name), source, Some(cold_dir.path()))
                .expect("cold transpile");
        }
        let cold = started.elapsed();

        let started = std::time::Instant::now();
        for (name, source) in &sources {
            transpile_with_cache_dir(Path::new(name), source, Some(cold_dir.path()))
                .expect("warm transpile");
        }
        let warm = started.elapsed();

        println!(
            "transpile cache bench: {} sources | cold {:?} ({:.1} ms/source) | warm {:?} ({:.2} ms/source)",
            sources.len(),
            cold,
            cold.as_secs_f64() * 1000.0 / sources.len() as f64,
            warm,
            warm.as_secs_f64() * 1000.0 / sources.len() as f64,
        );
    }

    fn collect_fixture_sources(dir: &Path, sources: &mut Vec<(String, String)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_fixture_sources(&path, sources);
            } else if matches!(
                path.extension().and_then(|ext| ext.to_str()),
                Some("ts" | "js")
            ) {
                if let Ok(source) = std::fs::read_to_string(&path) {
                    sources.push((path.display().to_string(), source));
                }
            }
        }
    }
}
