//! Node-compatible built-ins and host packages for extensions (E4).
//!
//! Upstream exposes a fixed virtual-module table to every extension
//! (`core/extensions/virtual-modules.ts:14-42`) and lets jiti fall back to
//! real Node built-ins for `node:*` specifiers. The embedded host has no
//! Node process, so this module supplies the subset the bundled extensions
//! and the documented examples import (`docs/EXTENSIONS.md:358-363`):
//!
//! - `node:fs` / `node:fs/promises` — the sync file calls used by the
//!   bundled MCP extension (`extensions/mcp/{config,log,cli}.ts`) and
//!   `node:fs/promises` used by the llama provider
//!   (`extensions/llama/huggingface.ts:1`).
//! - `node:path` — `extensions/mcp/runtime.ts:8`, `extensions/mcp/index.ts:29`,
//!   `extensions/codemode/tool.ts:24`, `extensions/llama/huggingface.ts:3`.
//! - `node:os` — `extensions/mcp/runtime.ts:7`, `extensions/llama/huggingface.ts:2`.
//! - `node:url` — `extensions/mcp/runtime.ts:10`, `core/extensions/loader.ts:9`.
//! - `node:crypto` — `extensions/mcp/oauth.ts:13`.
//! - `node:child_process`, `node:buffer`, `node:util`, `node:process` — the
//!   remaining calls an extension commonly reaches for.
//!
//! Host packages get real in-process surfaces where the port has state:
//! `@earendil-works/pi-coding-agent` reads settings, session metadata and
//! tools from [`HostCore`], and `@earendil-works/pi-mcp` ports the naming and
//! config helpers from `packages/mcp/src/{naming,config}.ts`. Packages the
//! port genuinely cannot supply still reject with the E1 error string kept
//! in [`crate::resolver`].
//!
//! Every shim is a JavaScript module (`fs.js`, `host/pi-mcp.js`, …) served
//! through the virtual-module loader. They never touch the Rust filesystem
//! directly: each one goes through the `__piOps` entries installed by
//! [`install_ops`], so the host remains the single owner of cwd, settings,
//! environment and process lifecycle.

use std::fmt::Write as _;
use std::io::Read as _;
use std::path::Path;
use std::sync::{Arc, Mutex};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;

use base64::Engine as _;
use rquickjs::{Ctx, Function, Object};
use serde_json::Value as Json;
use sha2::{Digest, Sha256, Sha512};

use crate::HostCore;
use crate::error::JsHostError;

/// The `node:*` specifiers this host serves. Kept public so the resolver and
/// the wiring doc can enumerate coverage without duplicating the table.
pub const NODE_BUILTINS: &[&str] = &[
    "node:buffer",
    "node:child_process",
    "node:crypto",
    "node:fs",
    "node:fs/promises",
    "node:os",
    "node:path",
    "node:process",
    "node:url",
    "node:util",
];

/// Host packages the host serves from in-process state. Other
/// `@earendil-works/*` specifiers keep the E1 rejection message.
pub const HOST_PACKAGES: &[&str] = &[
    "@earendil-works/pi-coding-agent",
    "@earendil-works/pi-mcp",
    "@mariozechner/pi-coding-agent",
    "@mariozechner/pi-mcp",
];

/// JavaScript source for one virtual specifier served by this module.
pub(crate) fn source(specifier: &str) -> Option<&'static str> {
    Some(match specifier {
        "node:buffer" => include_str!("buffer.js"),
        "node:child_process" => include_str!("child_process.js"),
        "node:crypto" => include_str!("hash.js"),
        "node:fs" => include_str!("fs.js"),
        "node:fs/promises" => include_str!("fs_promises.js"),
        "node:os" => include_str!("os.js"),
        "node:path" => include_str!("path.js"),
        "node:process" => include_str!("process.js"),
        "node:url" => include_str!("url.js"),
        "node:util" => include_str!("util.js"),
        "@earendil-works/pi-coding-agent" | "@mariozechner/pi-coding-agent" => {
            include_str!("host/pi-coding-agent.js")
        }
        "@earendil-works/pi-mcp" | "@mariozechner/pi-mcp" => include_str!("host/pi-mcp.js"),
        _ => return None,
    })
}

/// Encodes one host filesystem error the way Node's `fs` errors look to
/// extension code: an `{error, code}` object the shim turns into an `Error`
/// with a `.code` property.
fn io_error(error: &std::io::Error) -> Json {
    let code = match error.raw_os_error() {
        Some(2) => "ENOENT",
        Some(13) => "EACCES",
        Some(17) => "EEXIST",
        Some(20) => "ENOTDIR",
        Some(21) => "EISDIR",
        Some(39) => "ENOTEMPTY",
        _ => match error.kind() {
            std::io::ErrorKind::NotFound => "ENOENT",
            std::io::ErrorKind::PermissionDenied => "EACCES",
            std::io::ErrorKind::AlreadyExists => "EEXIST",
            _ => "EIO",
        },
    };
    serde_json::json!({ "error": error.to_string(), "code": code })
}

/// Decodes bytes with one of the encodings Node's `Buffer` accepts.
fn decode_bytes(bytes: &[u8], encoding: &str) -> Result<String, String> {
    match encoding {
        "" | "utf8" | "utf-8" => {
            String::from_utf8(bytes.to_vec()).map_err(|error| error.to_string())
        }
        "latin1" | "binary" => Ok(bytes.iter().copied().map(char::from).collect()),
        "hex" => {
            let mut out = String::with_capacity(bytes.len() * 2);
            for byte in bytes {
                let _ = write!(out, "{byte:02x}");
            }
            Ok(out)
        }
        "base64" => Ok(base64::engine::general_purpose::STANDARD.encode(bytes)),
        other => Err(format!("Unknown encoding: {other}")),
    }
}

/// Encodes a string into bytes for the same set of encodings.
fn encode_text(text: &str, encoding: &str) -> Result<Vec<u8>, String> {
    match encoding {
        "" | "utf8" | "utf-8" => Ok(text.as_bytes().to_vec()),
        "latin1" | "binary" => Ok(text
            .chars()
            .map(|character| u8::try_from(character).unwrap_or(b'?'))
            .collect()),
        "hex" => {
            let bytes = text.as_bytes();
            if bytes.len() % 2 != 0 {
                return Err("Invalid hex string".to_string());
            }
            let mut out = Vec::with_capacity(bytes.len() / 2);
            for pair in bytes.chunks_exact(2) {
                let hi = (pair[0] as char).to_digit(16);
                let lo = (pair[1] as char).to_digit(16);
                match (hi, lo) {
                    (Some(hi), Some(lo)) => out.push((hi * 16 + lo) as u8),
                    _ => return Err("Invalid hex string".to_string()),
                }
            }
            Ok(out)
        }
        "base64" => base64::engine::general_purpose::STANDARD
            .decode(text)
            .map_err(|error| error.to_string()),
        other => Err(format!("Unknown encoding: {other}")),
    }
}

/// Lexical POSIX normalization, the algorithm `node:path` uses (no syscalls,
/// no symlink resolution).
fn normalize_posix(input: &str) -> String {
    let absolute = input.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in input.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|last| *last != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            _ => parts.push(part),
        }
    }
    let mut out = String::new();
    if absolute {
        out.push('/');
    }
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            out.push('/');
        }
        out.push_str(part);
    }
    if out.is_empty() {
        out.push('.');
    }
    out
}

/// `node:path.resolve`: absolute segments reset the accumulated path; the
/// host cwd is the base for relative ones (`loader.ts:9`'s `fileURLToPath`
/// callers rely on this).
fn path_resolve(cwd: &str, args: &[String]) -> String {
    let mut current = String::new();
    if args.first().is_none_or(|first| !first.starts_with('/')) {
        current.clone_from(cwd);
    }
    for arg in args {
        if arg.starts_with('/') {
            current.clear();
        }
        if current.is_empty() {
            current.push_str(arg);
        } else {
            current.push('/');
            current.push_str(arg);
        }
    }
    normalize_posix(&current)
}

/// `node:path.parse` without touching the filesystem.
fn path_parse(input: &str) -> Json {
    let root = if input.starts_with('/') { "/" } else { "" };
    let base = input.rsplit('/').next().unwrap_or_default();
    let dir = match input.rfind('/') {
        Some(0) => "/",
        Some(index) => &input[..index],
        None => "",
    };
    let (name, extension) = match base.rfind('.') {
        Some(0) | None => (base, ""),
        Some(index) => (&base[..index], &base[index..]),
    };
    serde_json::json!({
        "root": root, "dir": dir, "base": base, "ext": extension, "name": name,
    })
}

/// Optional argument of a `node:path` call; missing arguments read as `""`,
/// matching Node's coercion of `undefined` for these operations.
fn arg_at(args: &[String], index: usize) -> &str {
    args.get(index).map_or("", String::as_str)
}

/// `node:path` dispatches by operation name; the shim passes all arguments
/// as one JSON array so `join`/`resolve` can be variadic.
fn path_op(op: &str, args: &[String], cwd: &str) -> Json {
    match op {
        "join" => Json::String(normalize_posix(&args.join("/"))),
        "resolve" => Json::String(path_resolve(cwd, args)),
        "normalize" => Json::String(normalize_posix(arg_at(args, 0))),
        "isAbsolute" => Json::Bool(arg_at(args, 0).starts_with('/')),
        "basename" => {
            let base = arg_at(args, 0).rsplit('/').next().unwrap_or_default();
            let suffix = arg_at(args, 1);
            Json::String(if !suffix.is_empty() && base.ends_with(suffix) {
                base[..base.len() - suffix.len()].to_string()
            } else {
                base.to_string()
            })
        }
        "dirname" => Json::String(match arg_at(args, 0).rfind('/') {
            Some(0) => "/".to_string(),
            Some(index) => arg_at(args, 0)[..index].to_string(),
            None => ".".to_string(),
        }),
        "extname" => {
            let base = arg_at(args, 0).rsplit('/').next().unwrap_or_default();
            Json::String(match base.rfind('.') {
                Some(0) | None => String::new(),
                Some(index) => base[index..].to_string(),
            })
        }
        "relative" => {
            let from = path_resolve(cwd, &[arg_at(args, 0).to_string()]);
            let to = path_resolve(cwd, &[arg_at(args, 1).to_string()]);
            let from_parts: Vec<&str> = from.split('/').filter(|part| !part.is_empty()).collect();
            let to_parts: Vec<&str> = to.split('/').filter(|part| !part.is_empty()).collect();
            let common = from_parts
                .iter()
                .zip(&to_parts)
                .take_while(|(left, right)| left == right)
                .count();
            let mut pieces: Vec<&str> = vec![".."; from_parts.len() - common];
            pieces.extend_from_slice(&to_parts[common..]);
            Json::String(if pieces.is_empty() {
                ".".to_string()
            } else {
                pieces.join("/")
            })
        }
        "parse" => path_parse(arg_at(args, 0)),
        _ => Json::Null,
    }
}

/// Reads one small text file used by `node:os`; absent files answer `""`.
fn os_file(path: &str) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// The subset of `node:os` upstream's bundled extensions call.
fn os_value(op: &str) -> Json {
    match op {
        "homedir" => Json::String(std::env::var("HOME").unwrap_or_else(|_| "/root".to_string())),
        "tmpdir" => Json::String(std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string())),
        "platform" => Json::String(std::env::consts::OS.to_string()),
        "arch" => Json::String(
            match std::env::consts::ARCH {
                "x86_64" => "x64",
                "aarch64" => "arm64",
                other => other,
            }
            .to_string(),
        ),
        "eol" => Json::String("\n".to_string()),
        "type" => Json::String("Linux".to_string()),
        "release" => Json::String(os_file("/proc/sys/kernel/osrelease")),
        "hostname" => {
            let host = os_file("/etc/hostname");
            Json::String(if host.is_empty() {
                "localhost".to_string()
            } else {
                host
            })
        }
        "cpus" => {
            let model = std::fs::read_to_string("/proc/cpuinfo")
                .ok()
                .and_then(|info| {
                    info.lines()
                        .find_map(|line| line.strip_prefix("model name"))
                        .and_then(|line| line.split_once(':'))
                        .map(|(_, model)| model.trim().to_string())
                })
                .unwrap_or_default();
            serde_json::json!([{ "model": model, "speed": 0 }])
        }
        "availableParallelism" => Json::String(
            std::thread::available_parallelism()
                .map_or(1, |count| count.get())
                .to_string(),
        ),
        _ => Json::Null,
    }
}

/// `process.env` with credential-shaped names removed. Upstream extensions
/// can read the whole environment; the port deliberately filters keys that
/// look like secrets (see `WIRING.md`) so a fixture can never leak a live
/// token into a transcript.
fn process_env() -> Json {
    let mut env = serde_json::Map::new();
    for (key, value) in std::env::vars() {
        let lower = key.to_ascii_lowercase();
        if ["key", "token", "secret", "password", "credential", "auth"]
            .into_iter()
            .any(|needle| lower.contains(needle))
        {
            continue;
        }
        env.insert(key, Json::String(value));
    }
    Json::Object(env)
}

/// `node:process` values backed by host state rather than the ambient
/// process (cwd and mode come from [`HostCore`], upstream's `ctx` values).
fn process_value(core: &HostCore, op: &str) -> Json {
    match op {
        "cwd" => Json::String(core.cwd.clone()),
        "platform" => Json::String(std::env::consts::OS.to_string()),
        "arch" => Json::String(
            match std::env::consts::ARCH {
                "x86_64" => "x64",
                "aarch64" => "arm64",
                other => other,
            }
            .to_string(),
        ),
        "version" => Json::String("v22.0.0".to_string()),
        "argv" => serde_json::json!(["node"]),
        "title" => Json::String("pi".to_string()),
        _ => Json::Null,
    }
}

/// Hex-encodes a digest with `write!` rather than repeated concatenation.
fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The hash algorithms the bundled MCP OAuth flow needs. Algorithms outside
/// this set reject with a Node-shaped message.
fn crypto_digest(algorithm: &str, bytes: &[u8]) -> Result<String, String> {
    match algorithm {
        "sha256" => Ok(to_hex(&Sha256::digest(bytes))),
        "sha512" => Ok(to_hex(&Sha512::digest(bytes))),
        other => Err(format!("Digest method not supported: {other}")),
    }
}

/// RFC 4122 v4 UUID from `/dev/urandom`, with a time-derived fallback on
/// platforms where that device is absent.
fn random_uuid() -> String {
    let mut bytes = [0_u8; 16];
    let mut source = std::fs::File::open("/dev/urandom");
    let filled = source
        .as_mut()
        .ok()
        .and_then(|file| file.read_exact(&mut bytes).ok());
    if filled.is_none() {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos() as u64);
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = (seed >> ((index % 8) * 8)) as u8;
        }
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let mut out = String::with_capacity(36);
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The session metadata the `@earendil-works/pi-coding-agent` shim reads.
fn session_metadata(core: &HostCore) -> Json {
    serde_json::json!({
        "name": &core.session_name,
        "cwd": &core.cwd,
        "mode": &core.mode,
        "thinkingLevel": &core.thinking_level,
        "entryCount": core.entries.len(),
        "labelCount": core.labels.len(),
        "toolCount": core.all_tools.len(),
        "activeToolCount": core.active_tools.len(),
    })
}

/// Wraps a host value in the `{ok, error}` envelope every shim parses.
fn ok(value: Json) -> String {
    serde_json::json!({ "ok": value }).to_string()
}

/// Wraps a host error in the same envelope.
fn fail(message: &str, code: &str) -> String {
    serde_json::json!({ "error": message, "code": code }).to_string()
}

/// Installs the Rust half of every built-in and host-package shim on the
/// engine's private `__piOps` object.
#[allow(clippy::too_many_lines)] // one op per Node API group; split when extending
pub(crate) fn install_ops(ctx: &Ctx<'_>, core: &Arc<Mutex<HostCore>>) -> Result<(), JsHostError> {
    let ops: Object = ctx.globals().get("__piOps")?;
    macro_rules! op {
        ($name:literal, $capture:ident, || -> $ret:ty $body:block) => {{
            let $capture = core.clone();
            let function = Function::new(ctx.clone(), move || -> $ret { $body })?;
            ops.set($name, function)?;
        }};
        ($name:literal, $capture:ident, |$($arg:ident : $ty:ty),*| -> $ret:ty $body:block) => {{
            let $capture = core.clone();
            let function = Function::new(ctx.clone(), move |$($arg : $ty),*| -> $ret { $body })?;
            ops.set($name, function)?;
        }};
    }

    // ---- node:buffer / node:crypto encodings ----
    op!("nodeBufferEncode", _core, |text: String,
                                    encoding: String|
     -> String {
        match encode_text(&text, &encoding) {
            Ok(bytes) => ok(serde_json::json!(bytes)),
            Err(error) => fail(&error, "EINVAL"),
        }
    });
    op!("nodeBufferDecode", _core, |bytes: String,
                                    encoding: String|
     -> String {
        let values: Vec<u8> = serde_json::from_str(&bytes).unwrap_or_default();
        match decode_bytes(&values, &encoding) {
            Ok(text) => ok(Json::String(text)),
            Err(error) => fail(&error, "EINVAL"),
        }
    });
    op!("nodeCryptoHash", _core, |algorithm: String,
                                  bytes: String,
                                  encoding: String|
     -> String {
        let values: Vec<u8> = serde_json::from_str(&bytes).unwrap_or_default();
        match crypto_digest(&algorithm, &values) {
            Ok(digest) if encoding == "buffer" => ok(serde_json::json!(digest.as_bytes())),
            Ok(digest) => ok(Json::String(digest)),
            Err(error) => fail(&error, "ERR_CRYPTO_INVALID_DIGEST"),
        }
    });
    op!("nodeCryptoRandomUuid", _core, || -> String {
        ok(Json::String(random_uuid()))
    });

    // ---- node:fs ----
    op!("nodeFsReadFileSync", _core, |path: String,
                                      encoding: String|
     -> String {
        match std::fs::read(&path) {
            Ok(bytes) => {
                if encoding == "buffer" || encoding.is_empty() {
                    ok(serde_json::json!(bytes))
                } else {
                    match decode_bytes(&bytes, &encoding) {
                        Ok(text) => ok(Json::String(text)),
                        Err(error) => fail(&error, "EINVAL"),
                    }
                }
            }
            Err(error) => {
                serde_json::to_string(&io_error(&error)).unwrap_or_else(|_| fail("io error", "EIO"))
            }
        }
    });
    op!("nodeFsWriteFileSync", _core, |path: String,
                                       data: String,
                                       encoding: String|
     -> String {
        match encode_text(&data, &encoding)
            .and_then(|bytes| std::fs::write(&path, bytes).map_err(|error| error.to_string()))
        {
            Ok(()) => ok(Json::Null),
            Err(error) => fail(&error, "EIO"),
        }
    });
    op!("nodeFsAppendFileSync", _core, |path: String,
                                        data: String,
                                        encoding: String|
     -> String {
        use std::io::Write as _;
        let outcome = encode_text(&data, &encoding).and_then(|bytes| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .and_then(|mut file| file.write_all(&bytes))
                .map_err(|error| error.to_string())
        });
        match outcome {
            Ok(()) => ok(Json::Null),
            Err(error) => fail(&error, "EIO"),
        }
    });
    op!("nodeFsExistsSync", _core, |path: String| -> String {
        Json::Bool(Path::new(&path).exists()).to_string()
    });
    op!("nodeFsReadDirSync", _core, |path: String| -> String {
        match std::fs::read_dir(&path) {
            Ok(entries) => {
                let mut names: Vec<String> = entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect();
                names.sort();
                ok(Json::Array(names.into_iter().map(Json::String).collect()))
            }
            Err(error) => {
                serde_json::to_string(&io_error(&error)).unwrap_or_else(|_| fail("io error", "EIO"))
            }
        }
    });
    op!("nodeFsStatSync", _core, |path: String| -> String {
        match std::fs::metadata(&path) {
            Ok(metadata) => {
                let modified = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok());
                #[cfg(unix)]
                let mode = metadata.permissions().mode();
                #[cfg(not(unix))]
                let mode = 0;
                ok(serde_json::json!({
                    "isFile": metadata.is_file(),
                    "isDirectory": metadata.is_dir(),
                    "isSymbolicLink": metadata.file_type().is_symlink(),
                    "size": metadata.len(),
                    "mtimeMs": modified.map_or(0.0, |duration| duration.as_secs_f64() * 1000.0),
                    "mode": mode,
                }))
            }
            Err(error) => {
                serde_json::to_string(&io_error(&error)).unwrap_or_else(|_| fail("io error", "EIO"))
            }
        }
    });
    op!("nodeFsMkdirSync", _core, |path: String,
                                   recursive: String|
     -> String {
        let outcome = if recursive == "true" {
            std::fs::create_dir_all(&path)
        } else {
            std::fs::create_dir(&path)
        };
        match outcome {
            Ok(()) => ok(Json::Null),
            Err(error) => {
                serde_json::to_string(&io_error(&error)).unwrap_or_else(|_| fail("io error", "EIO"))
            }
        }
    });
    op!("nodeFsRenameSync", _core, |from: String,
                                    to: String|
     -> String {
        match std::fs::rename(&from, &to) {
            Ok(()) => ok(Json::Null),
            Err(error) => {
                serde_json::to_string(&io_error(&error)).unwrap_or_else(|_| fail("io error", "EIO"))
            }
        }
    });
    op!("nodeFsUnlinkSync", _core, |path: String| -> String {
        match std::fs::remove_file(&path) {
            Ok(()) => ok(Json::Null),
            Err(error) => {
                serde_json::to_string(&io_error(&error)).unwrap_or_else(|_| fail("io error", "EIO"))
            }
        }
    });
    op!("nodeFsRmSync", _core, |path: String,
                                recursive: String,
                                force: String|
     -> String {
        let metadata = std::fs::symlink_metadata(&path);
        let outcome = match metadata {
            Ok(metadata) if metadata.is_dir() && recursive == "true" => {
                std::fs::remove_dir_all(&path)
            }
            Ok(_) => std::fs::remove_file(&path),
            Err(error) if force == "true" && error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        };
        match outcome {
            Ok(()) => ok(Json::Null),
            Err(error) => {
                serde_json::to_string(&io_error(&error)).unwrap_or_else(|_| fail("io error", "EIO"))
            }
        }
    });
    op!("nodeFsRealpathSync", _core, |path: String| -> String {
        match std::fs::canonicalize(&path) {
            Ok(resolved) => ok(Json::String(resolved.to_string_lossy().into_owned())),
            Err(error) => {
                serde_json::to_string(&io_error(&error)).unwrap_or_else(|_| fail("io error", "EIO"))
            }
        }
    });
    op!("nodeFsReadlinkSync", _core, |path: String| -> String {
        match std::fs::read_link(&path) {
            Ok(target) => ok(Json::String(target.to_string_lossy().into_owned())),
            Err(error) => {
                serde_json::to_string(&io_error(&error)).unwrap_or_else(|_| fail("io error", "EIO"))
            }
        }
    });

    // ---- node:path ----
    op!("nodePath", core, |operation: String,
                           args: String|
     -> String {
        let values: Vec<String> = serde_json::from_str(&args).unwrap_or_default();
        let cwd = core.lock().expect("host core mutex poisoned").cwd.clone();
        serde_json::to_string(&path_op(&operation, &values, &cwd))
            .unwrap_or_else(|_| "null".to_string())
    });

    // ---- node:os / node:process ----
    op!("nodeOs", _core, |operation: String| -> String {
        serde_json::to_string(&os_value(&operation)).unwrap_or_else(|_| "null".to_string())
    });
    op!("nodeProcess", core, |operation: String| -> String {
        let core = core.lock().expect("host core mutex poisoned");
        serde_json::to_string(&process_value(&core, &operation))
            .unwrap_or_else(|_| "null".to_string())
    });
    op!("nodeProcessEnv", _core, || -> String {
        serde_json::to_string(&process_env()).unwrap_or_else(|_| "{}".to_string())
    });

    // ---- node:url ----
    op!("nodeUrlFileToPath", _core, |url: String| -> String {
        let Some(path) = url.strip_prefix("file://") else {
            return fail("Must be a file URL", "ERR_INVALID_URL");
        };
        let (authority, pathname) = match path.split_once('/') {
            Some((authority, rest)) => (authority, format!("/{rest}")),
            None => (path, "/".to_string()),
        };
        if !authority.is_empty() && authority != "localhost" {
            return fail(
                "file URL host must be \"localhost\" or empty",
                "ERR_INVALID_URL_SCHEME",
            );
        }
        let mut decoded = Vec::new();
        let bytes = pathname.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'%'
                && index + 2 < bytes.len()
                && let Some(hi) = (bytes[index + 1] as char).to_digit(16)
                && let Some(lo) = (bytes[index + 2] as char).to_digit(16)
            {
                decoded.push((hi * 16 + lo) as u8);
                index += 3;
            } else {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
        match String::from_utf8(decoded) {
            Ok(path) => ok(Json::String(path)),
            Err(error) => fail(&error.to_string(), "ERR_INVALID_URL"),
        }
    });
    op!("nodeUrlPathToFileUrl", _core, |path: String| -> String {
        let absolute = if path.starts_with('/') {
            path.clone()
        } else {
            format!("/{path}")
        };
        let mut href = String::from("file://");
        for character in absolute.chars() {
            match character {
                ' ' => href.push_str("%20"),
                '#' => href.push_str("%23"),
                '?' => href.push_str("%3F"),
                '%' => href.push_str("%25"),
                other if other.is_ascii() => href.push(other),
                other => {
                    let mut buffer = [0_u8; 4];
                    for byte in other.encode_utf8(&mut buffer).as_bytes() {
                        let _ = write!(href, "%{byte:02X}");
                    }
                }
            }
        }
        ok(Json::String(href))
    });

    // ---- host packages ----
    op!("getSessionMetadata", core, || -> String {
        let core = core.lock().expect("host core mutex poisoned");
        serde_json::to_string(&session_metadata(&core)).unwrap_or_else(|_| "{}".to_string())
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_helpers_match_node_lexical_semantics() {
        assert_eq!(normalize_posix("/a/b/../c/./d"), "/a/b/c/d");
        assert_eq!(normalize_posix("a/../../b"), "../b");
        assert_eq!(
            path_resolve("/work", &["x".into(), "..".into(), "y".into()]),
            "/work/y"
        );
        assert_eq!(
            path_resolve("/work", &["/etc".into(), "hosts".into()]),
            "/etc/hosts"
        );
        assert_eq!(
            path_op("relative", &["/work/a".into(), "/work/b".into()], "/work"),
            Json::String("../b".to_string())
        );
        assert_eq!(
            path_op("parse", &["/a/b.txt".into()], "/work"),
            serde_json::json!({"root": "/", "dir": "/a", "base": "b.txt", "ext": ".txt", "name": "b"})
        );
    }

    #[test]
    fn buffer_encodings_round_trip() {
        for encoding in ["utf8", "hex", "base64", "latin1"] {
            let bytes = encode_text("pi-rs", encoding).expect("encode");
            assert_eq!(decode_bytes(&bytes, encoding).expect("decode"), "pi-rs");
        }
        assert_eq!(
            encode_text("6869", "hex").expect("hex decode"),
            vec![0x68, 0x69]
        );
        assert_eq!(
            decode_bytes(&[0x68, 0x69], "hex").expect("hex encode"),
            "6869"
        );
    }

    #[test]
    fn filesystem_errors_carry_node_codes() {
        let error = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        assert_eq!(io_error(&error)["code"], "ENOENT");
        let error = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        assert_eq!(io_error(&error)["code"], "EACCES");
    }

    #[test]
    fn virtual_sources_cover_every_declared_specifier() {
        for specifier in NODE_BUILTINS.iter().chain(HOST_PACKAGES) {
            assert!(source(specifier).is_some(), "missing shim for {specifier}");
        }
        assert!(source("node:sqlite").is_none());
    }
}
