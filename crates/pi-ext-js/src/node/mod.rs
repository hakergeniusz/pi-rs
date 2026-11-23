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
//! - `node:module` — `createRequire` for extensions that need a synchronous
//!   require (real-world sweep: `pi-gemini-router/index.ts:55,339`).
//! - `node:child_process`, `node:buffer`, `node:util`, `node:process` — the
//!   remaining calls an extension commonly reaches for. The sweep also
//!   surfaced `execFile` (`pi-guardrails/index.ts:49,567`) and `fs.openSync`
//!   (`pi-cord/src/{chat,lock}.ts`), so those are served by the same ops.
//!   `fs.mkdtempSync` (`pi-sol-pi/src/sol-pi/runtime-paths.ts:26`) and
//!   `fs.chmodSync` (`pi-ssh/src/session.ts:434`) are served there too: the
//!   former creates a unique directory with Node's six-character suffix, the
//!   latter maps to real Unix permissions and is a documented no-op elsewhere.
//!   `fs/promises.open` and its `FileHandle` (`pi-sol-pi`'s observation pack)
//!   wrap the same opaque fd table; numeric POSIX flags are accepted alongside
//!   the string forms.
//! - `node:http` — resolution-only shim for `pi-zed-models/src/browser-auth.ts`,
//!   which imports `http.createServer` for an OAuth redirect listener. The
//!   host has no socket seam, so the module resolves and exposes the Node
//!   shape but its calls throw an explicit unsupported error (documented in
//!   `http.js`).
//!
//! The global `process` object is installed once at engine creation by
//! [`process_global_source`] (`process_global.js`), because third-party
//! extensions reference it at module top level without importing
//! `node:process` (`pi-esc-send-queued`, `pi-output-cap`, `pi-skill-loader`,
//! `pi-subagents`, `pi-title`, ...). `node:process` re-exports that same
//! object. It mirrors Node's shape but never exits the host or delivers
//! signals; all lifecycle calls are inert by design. The same bootstrap
//! also installs `structuredClone` (used at load time by
//! `pi-guardrails/index.ts:166`) and an `Intl.NumberFormat` subset (used by
//! `pi-sol-pi/src/sol-pi/tui.ts:14`), since `QuickJS` ships neither.
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
//!
//! ## Fidelity ground truth
//!
//! Unlike the rest of the port, these shims mirror **Node's** documented
//! semantics rather than a TypeScript file (upstream only *imports* these as
//! real Node built-ins):
//!
//! - `node:url` → WHATWG URL Standard + `node:url` (`fileURLToPath`,
//!   `pathToFileURL`, `URL`, `URL.canParse`, `URLSearchParams`); the static is
//!   used by upstream's `web-url.ts:9` and `extensions/mcp/tools.ts:161`,
//! - `node:fs` → `fs.Stats` / `fs.lstatSync` (symlinks are *not* followed),
//! - `node:crypto` → `crypto.createHmac` (RFC 2104 / FIPS 198-1, SHA-256 and
//!   SHA-512 block sizes). The asymmetric calls `pi-zed-models` imports are
//!   shape-only stubs (no RSA backend in the host); their padding constants
//!   are real Node values,
//! - `node:process` → `process.env` key rules and the string-valued
//!   `cwd`/`platform`/`arch`/`version` properties.
//! - `node:module` → `createRequire` resolving through the host's own
//!   resolver/loader (`loader.ts:42,78-89`, `loader.ts:571-602`).
//! - `node:http` → only the module shape is portable here; without a socket
//!   seam the shim resolves but its `createServer`/`request`/`get` calls throw
//!   an explicit unsupported error (a deliberate, documented scope limit).
//!
//! The fd handle behind `fs.openSync` is deliberately minimal: positive
//! integer fds index a process-lifetime table in this module, flags `r`,
//! `r+`, `w`, `wx`, `a` are mapped to `OpenOptions`, and only
//! `readSync`/`writeSync`/`closeSync` accept those fds. No file streams,
//! `fs.open` promises, `fchmod`/`fsync`/`ftruncate`, or real OS fd numbers
//! are provided; `mode` is accepted but ignored (the host must not chmod).
//! Likewise `child_process.execFile` runs the file + argv through the
//! sandbox's `exec` op (never a shell), delivering the callback on the next
//! microtask; no argument is shell-interpolated.
//!
//! Platform note: native Windows has no `/dev/urandom`, so `random_uuid`
//! takes the time-derived fallback. A `getrandom` swap needs a Cargo.lock
//! change and is tracked separately by the platform-portability audit
//! (NEEDS-DEP). The filesystem and path shims have explicit Windows branches
//! (`symlink_file`/`symlink_dir`, `mode = 0`), so no Unix API leaks here.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::{Read as _, Seek as _, SeekFrom, Write as IoWrite};
use std::path::Path;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;

use base64::Engine as _;
use rquickjs::loader::{Loader as _, Resolver as _};
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
    "node:http",
    "node:module",
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
        "node:http" => include_str!("http.js"),
        "node:module" => include_str!("module.js"),
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

/// Plain-script source that installs the global `process` object. Evaluated
/// once by [`crate::engine::JsHost::new`] after the ops exist, before any
/// extension module runs; it is not a virtual module because extensions use
/// `process` without importing `node:process`.
pub(crate) fn process_global_source() -> &'static str {
    include_str!("process_global.js")
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

/// Alphabet Node samples from for the six random characters appended by
/// `fs.mkdtempSync` (`[a-zA-Z0-9]`).
const MKDTEMP_ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

/// Six random alphanumerics, sourced from `/dev/urandom` with the same
/// time-derived fallback `random_uuid` uses when that device is absent.
fn random_suffix(length: usize) -> String {
    let mut bytes = vec![0_u8; length];
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
    bytes
        .into_iter()
        .map(|byte| MKDTEMP_ALPHABET[usize::from(byte) % MKDTEMP_ALPHABET.len()] as char)
        .collect()
}

/// `fs.mkdtempSync(prefix)`: append six random characters to `prefix` and
/// create the directory, retrying on `EEXIST` the way Node does. Returns the
/// created path as a string (the shim wraps it when an encoding is asked for).
fn mkdtemp_sync(prefix: &str) -> Result<String, Json> {
    for _ in 0..100 {
        let candidate = format!("{prefix}{}", random_suffix(6));
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(io_error(&error)),
        }
    }
    Err(serde_json::json!({
        "error": "could not create a unique temporary directory",
        "code": "EEXIST",
    }))
}

/// Parses the decimal mode string produced by the `node:fs` shim after its
/// own number/octal-string validation. The host only applies the value,
/// mirroring Node's `fs.chmodSync` acceptance of any non-negative integer.
fn parse_chmod_mode(text: &str) -> Result<u32, String> {
    match text.parse::<u32>() {
        Ok(mode) => Ok(mode),
        Err(error) => Err(format!("invalid mode: {error}")),
    }
}

/// Applies `fs.chmodSync(path, mode)`. On Unix this maps to the real
/// `PermissionsExt::from_mode` so the SSH extension's `chmodSync(dir, 0o700)`
/// is enforced. On other platforms the call is a documented no-op: the host
/// has no portable mode abstraction, matching `fs.openSync`'s `mode` handling.
fn chmod_sync(path: &str, text: &str) -> Result<(), Json> {
    let mode = parse_chmod_mode(text)
        .map_err(|message| serde_json::json!({ "error": message, "code": "EINVAL" }))?;
    #[cfg(unix)]
    {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|error| io_error(&error))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

/// One `fs.openSync` handle. The fd is an opaque host id (not an OS fd):
/// extensions only pass it back to `readSync`/`writeSync`/`closeSync`.
struct OpenFile {
    file: File,
}

/// Process-lifetime fd table for the minimal `openSync` handle. Kept here
/// rather than in `HostCore` so the fds survive host re-creation the same
/// way an extension's own captured values do.
fn fd_table() -> &'static Mutex<HashMap<i32, OpenFile>> {
    static TABLE: OnceLock<Mutex<HashMap<i32, OpenFile>>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn allocate_fd(file: File) -> i32 {
    static NEXT: AtomicI32 = AtomicI32::new(100);
    let fd = NEXT.fetch_add(1, Ordering::Relaxed);
    fd_table()
        .lock()
        .expect("fd table mutex poisoned")
        .insert(fd, OpenFile { file });
    fd
}

fn bad_fd() -> Json {
    serde_json::json!({ "error": "Bad file descriptor", "code": "EBADF" })
}

/// Maps the `fs.openSync` flag strings to `OpenOptions`. `mode` is accepted
/// by the shim but ignored: an extension must never chmod through the host.
fn open_fd(path: &str, flags: &str) -> Result<i32, Json> {
    let mut options = OpenOptions::new();
    match flags {
        "r" => {
            options.read(true);
        }
        "r+" => {
            options.read(true).write(true);
        }
        "w" => {
            options.write(true).create(true).truncate(true);
        }
        "wx" | "xw" => {
            options.write(true).create_new(true);
        }
        "w+" => {
            options.read(true).write(true).create(true).truncate(true);
        }
        "a" => {
            options.write(true).create(true).append(true);
        }
        "ax" | "xa" => {
            options.write(true).create_new(true).append(true);
        }
        other if other.parse::<u32>().is_ok() => {
            // Node also accepts the numeric POSIX flags exposed on
            // `fs.constants` (pi-sol-pi uses O_RDONLY | O_NOFOLLOW etc.).
            // `O_NOFOLLOW` has no `OpenOptions` equivalent and is ignored;
            // the host's file abstraction does not resolve symlinks here.
            let bits = other.parse::<u32>().unwrap_or(0);
            match bits & 0o7 {
                0 => {
                    options.read(true);
                }
                1 => {
                    options.write(true);
                }
                2 => {
                    options.read(true).write(true);
                }
                _ => {
                    return Err(serde_json::json!({
                        "error": format!("Unknown file open flag: {other}"),
                        "code": "EINVAL"
                    }));
                }
            }
            if bits & 0o100 != 0 {
                options.create(true);
            }
            if bits & 0o200 != 0 {
                options.create_new(true);
            }
            if bits & 0o1000 != 0 {
                options.truncate(true);
            }
            if bits & 0o2000 != 0 {
                options.append(true);
            }
        }
        other => {
            return Err(serde_json::json!({
                "error": format!("Unknown file open flag: {other}"),
                "code": "EINVAL"
            }));
        }
    }
    let file = options.open(path).map_err(|error| io_error(&error))?;
    Ok(allocate_fd(file))
}

/// Reads at most 16 MiB into a fresh byte vector; the JS shim copies it into
/// the caller's Buffer. `position < 0` continues from the current cursor.
fn read_fd(fd: i32, length: usize, position: i64) -> Result<Vec<u8>, Json> {
    let mut table = fd_table().lock().expect("fd table mutex poisoned");
    let open = table.get_mut(&fd).ok_or_else(bad_fd)?;
    if position >= 0 {
        open.file
            .seek(SeekFrom::Start(position as u64))
            .map_err(|error| io_error(&error))?;
    }
    let mut bytes = vec![0_u8; length.min(16 * 1024 * 1024)];
    let count = open
        .file
        .read(&mut bytes)
        .map_err(|error| io_error(&error))?;
    bytes.truncate(count);
    Ok(bytes)
}

fn write_fd(fd: i32, bytes: &[u8], position: i64) -> Result<usize, Json> {
    let mut table = fd_table().lock().expect("fd table mutex poisoned");
    let open = table.get_mut(&fd).ok_or_else(bad_fd)?;
    if position >= 0 {
        open.file
            .seek(SeekFrom::Start(position as u64))
            .map_err(|error| io_error(&error))?;
    }
    open.file
        .write_all(bytes)
        .map_err(|error| io_error(&error))?;
    Ok(bytes.len())
}

fn close_fd(fd: i32) -> Result<(), Json> {
    fd_table()
        .lock()
        .expect("fd table mutex poisoned")
        .remove(&fd)
        .map(|_| ())
        .ok_or_else(bad_fd)
}

/// Resolves a `createRequire` specifier with the exact resolver an ESM
/// import uses (`resolver.rs`), without adding a second resolution table.
fn require_resolve(ctx: &Ctx<'_>, base: &str, specifier: &str) -> rquickjs::Result<String> {
    crate::resolver::ExtensionResolver.resolve(ctx, base, specifier, None)
}

/// Evaluates a resolved module synchronously and returns its namespace. The
/// loader owns transpilation, CJS wrapping and the virtual-module table, so
/// `require("node:path")` and `require("./helper.ts")` behave like their ESM
/// counterparts.
fn require_module<'js>(
    ctx: &Ctx<'js>,
    base: &str,
    specifier: &str,
) -> rquickjs::Result<rquickjs::Value<'js>> {
    let name = require_resolve(ctx, base, specifier)?;
    let mut loader = crate::resolver::ExtensionLoader::default();
    let module = loader.load(ctx, &name, None)?;
    // Match the ESM entry path: `import.meta.url` must be present for
    // extensions that create their require from it.
    module.meta()?.set("url", format!("file://{name}"))?;
    Ok(module.eval()?.0.namespace()?.into_value())
}

/// The `fs.Stats` payload shared by `statSync` (follows symlinks) and
/// `lstatSync` (uses `symlink_metadata`, so a symlink reports itself).
///
/// `mode` is the raw Unix permission/type bits, matching Node's
/// `stats.mode`; `mtimeMs` is the modification time as fractional
/// milliseconds since the Unix epoch.
fn fs_stats(metadata: &std::fs::Metadata) -> Json {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok());
    #[cfg(unix)]
    let mode = metadata.permissions().mode();
    #[cfg(not(unix))]
    let mode = 0;
    #[cfg(unix)]
    let (is_block_device, is_character_device, is_fifo, is_socket) = {
        use std::os::unix::fs::FileTypeExt as _;
        let file_type = metadata.file_type();
        (
            file_type.is_block_device(),
            file_type.is_char_device(),
            file_type.is_fifo(),
            file_type.is_socket(),
        )
    };
    #[cfg(not(unix))]
    let (is_block_device, is_character_device, is_fifo, is_socket) = (false, false, false, false);
    serde_json::json!({
        "isFile": metadata.is_file(),
        "isDirectory": metadata.is_dir(),
        "isSymbolicLink": metadata.file_type().is_symlink(),
        "isBlockDevice": is_block_device,
        "isCharacterDevice": is_character_device,
        "isFIFO": is_fifo,
        "isSocket": is_socket,
        "size": metadata.len(),
        "mtimeMs": modified.map_or(0.0, |duration| duration.as_secs_f64() * 1000.0),
        "mode": mode,
    })
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
            if !bytes.len().is_multiple_of(2) {
                return Err("Invalid hex string".to_string());
            }
            let mut out = Vec::with_capacity(bytes.len() / 2);
            for pair in bytes.as_chunks::<2>().0 {
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
        current.push_str(cwd);
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
                .map_or(1, std::num::NonZero::get)
                .to_string(),
        ),
        _ => Json::Null,
    }
}

/// A name Node will actually surface in `process.env`. Empty names, names
/// containing `=` or NUL, and names that cannot be written in a shell are
/// dropped. Valid names start with a letter or `_` and continue with ASCII
/// letters, digits or `_` (the portable environment-variable grammar).
fn is_valid_env_key(key: &str) -> bool {
    let mut characters = key.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    if !first.is_ascii_alphabetic() && first != '_' {
        return false;
    }
    characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// Credential-shaped names are filtered deliberately so an extension can
/// never copy a live token into a transcript or a tool call (see
/// `WIRING.md`). This is a port-specific deviation from Node, not a spec
/// behaviour.
fn is_secret_env_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    ["key", "token", "secret", "password", "credential", "auth"]
        .into_iter()
        .any(|needle| lower.contains(needle))
}

/// Pure `process.env` builder so the filtering rules can be unit-tested with
/// synthetic entries without touching the real process environment.
fn filter_env(entries: impl IntoIterator<Item = (String, String)>) -> Json {
    let mut env = serde_json::Map::new();
    for (key, value) in entries {
        if is_valid_env_key(&key) && !is_secret_env_key(&key) {
            env.insert(key, Json::String(value));
        }
    }
    Json::Object(env)
}

/// `process.env` from the ambient process. `vars_os` is used instead of
/// `vars` so a single non-UTF-8 entry cannot panic the whole engine; such
/// entries are simply dropped as invalid names.
fn process_env() -> Json {
    filter_env(
        std::env::vars_os()
            .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?))),
    )
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

/// Node accepts the OpenSSL spelling of a digest (`SHA256`, `SHA-256`,
/// `sha256`); normalise to the canonical lowercase name so all three work.
fn normalize_digest_name(algorithm: &str) -> String {
    algorithm
        .to_ascii_lowercase()
        .chars()
        .filter(|character| *character != '-')
        .collect()
}

/// The hash algorithms the bundled MCP OAuth flow needs. Algorithms outside
/// this set reject with a Node-shaped message.
fn crypto_digest(algorithm: &str, bytes: &[u8]) -> Result<String, String> {
    match normalize_digest_name(algorithm).as_str() {
        "sha256" => Ok(to_hex(&Sha256::digest(bytes))),
        "sha512" => Ok(to_hex(&Sha512::digest(bytes))),
        _ => Err(format!("Digest method not supported: {algorithm}")),
    }
}

/// HMAC (RFC 2104) implemented directly over `sha2` because the host does
/// not depend on the `hmac` crate. `key` is hashed when longer than the
/// digest's block size and zero-padded otherwise, then combined with the
/// inner (`0x36`) and outer (`0x5c`) pads.
///
/// The algorithm name is normalised like [`crypto_digest`]; the returned
/// string is lowercase hex, matching `digest("hex")`.
#[allow(clippy::type_complexity)] // (block size, digest fn) is clearer inline than a one-use alias
fn crypto_hmac(algorithm: &str, key: &[u8], bytes: &[u8]) -> Result<String, String> {
    // (block size for HMAC padding, digest function)
    let (block_size, digest): (usize, fn(&[u8]) -> Vec<u8>) =
        match normalize_digest_name(algorithm).as_str() {
            "sha256" => (64, |data| Sha256::digest(data).to_vec()),
            "sha512" => (128, |data| Sha512::digest(data).to_vec()),
            _ => return Err(format!("Digest method not supported: {algorithm}")),
        };

    let shortened;
    let normalized_key = if key.len() > block_size {
        shortened = digest(key);
        shortened.as_slice()
    } else {
        key
    };

    let mut inner_pad = vec![0x36_u8; block_size];
    let mut outer_pad = vec![0x5c_u8; block_size];
    for (pad, key_byte) in inner_pad.iter_mut().zip(normalized_key) {
        *pad ^= key_byte;
    }
    for (pad, key_byte) in outer_pad.iter_mut().zip(normalized_key) {
        *pad ^= key_byte;
    }

    let mut inner = Vec::with_capacity(block_size + bytes.len());
    inner.extend_from_slice(&inner_pad);
    inner.extend_from_slice(bytes);
    let inner_digest = digest(&inner);

    let mut outer = Vec::with_capacity(block_size + inner_digest.len());
    outer.extend_from_slice(&outer_pad);
    outer.extend_from_slice(&inner_digest);
    Ok(to_hex(&digest(&outer)))
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
#[allow(clippy::needless_pass_by_value)] // by-value keeps the shim call sites terse
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
pub(crate) fn install_ops<'js>(
    ctx: &Ctx<'js>,
    core: &Arc<Mutex<HostCore>>,
) -> Result<(), JsHostError> {
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
    op!("nodeCryptoHmac", _core, |algorithm: String,
                                  key: String,
                                  bytes: String,
                                  encoding: String|
     -> String {
        let key_bytes: Vec<u8> = serde_json::from_str(&key).unwrap_or_default();
        let values: Vec<u8> = serde_json::from_str(&bytes).unwrap_or_default();
        match crypto_hmac(&algorithm, &key_bytes, &values) {
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
            Ok(metadata) => ok(fs_stats(&metadata)),
            Err(error) => {
                serde_json::to_string(&io_error(&error)).unwrap_or_else(|_| fail("io error", "EIO"))
            }
        }
    });
    // `lstat` does NOT follow the final symlink, so `isSymbolicLink()` is
    // observable here while `statSync` reports the link's target.
    op!("nodeFsLstatSync", _core, |path: String| -> String {
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => ok(fs_stats(&metadata)),
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
    op!("nodeFsMkdtempSync", _core, |prefix: String| -> String {
        match mkdtemp_sync(&prefix) {
            Ok(path) => ok(Json::String(path)),
            Err(error) => serde_json::to_string(&error).unwrap_or_else(|_| fail("io error", "EIO")),
        }
    });
    op!("nodeFsChmodSync", _core, |path: String,
                                   mode: String|
     -> String {
        match chmod_sync(&path, &mode) {
            Ok(()) => ok(Json::Null),
            Err(error) => serde_json::to_string(&error).unwrap_or_else(|_| fail("io error", "EIO")),
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
    op!("nodeFsSymlinkSync", _core, |target: String,
                                     path: String|
     -> String {
        #[cfg(unix)]
        let outcome = std::os::unix::fs::symlink(&target, &path);
        #[cfg(windows)]
        let outcome = if std::path::Path::new(&target).is_dir() {
            std::os::windows::fs::symlink_dir(&target, &path)
        } else {
            std::os::windows::fs::symlink_file(&target, &path)
        };
        #[cfg(not(any(unix, windows)))]
        let outcome: std::io::Result<()> = Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "symlinks are not supported on this platform",
        ));
        match outcome {
            Ok(()) => ok(Json::Null),
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
    // Minimal fd handles: `openSync` returns an opaque host id, and the
    // three companion calls consume it. `mode` is accepted but ignored.
    op!("nodeFsOpenSync", _core, |path: String,
                                  flags: String,
                                  _mode: String|
     -> String {
        match open_fd(&path, &flags) {
            Ok(fd) => ok(Json::Number(fd.into())),
            Err(error) => serde_json::to_string(&error).unwrap_or_else(|_| fail("io error", "EIO")),
        }
    });
    op!("nodeFsReadSync", _core, |fd: i32,
                                  length: i32,
                                  position: i64|
     -> String {
        match read_fd(fd, length.max(0) as usize, position) {
            Ok(bytes) => ok(Json::Array(bytes.into_iter().map(Json::from).collect())),
            Err(error) => serde_json::to_string(&error).unwrap_or_else(|_| fail("io error", "EIO")),
        }
    });
    op!("nodeFsWriteSync", _core, |fd: i32,
                                   bytes: String,
                                   position: i64|
     -> String {
        let values: Vec<u8> = serde_json::from_str(&bytes).unwrap_or_default();
        match write_fd(fd, &values, position) {
            Ok(count) => ok(Json::Number(count.into())),
            Err(error) => serde_json::to_string(&error).unwrap_or_else(|_| fail("io error", "EIO")),
        }
    });
    op!("nodeFsCloseSync", _core, |fd: i32| -> String {
        match close_fd(fd) {
            Ok(()) => ok(Json::Null),
            Err(error) => serde_json::to_string(&error).unwrap_or_else(|_| fail("io error", "EIO")),
        }
    });

    // ---- node:module (createRequire) ----
    // `Ctx` implements `FromParam`, so rquickjs hands the call's context to
    // the closure instead of us capturing one. Capturing a Ctx here would
    // keep the context alive inside its own runtime and QuickJS would abort
    // at teardown with a non-empty gc list.
    let function = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>,
              base: String,
              specifier: String|
              -> rquickjs::Result<rquickjs::Value<'js>> {
            require_module(&ctx, &base, &specifier)
        },
    )?;
    ops.set("nodeRequire", function)?;
    let function = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, base: String, specifier: String| -> rquickjs::Result<String> {
            require_resolve(&ctx, &base, &specifier)
        },
    )?;
    ops.set("nodeRequireResolve", function)?;
    // `import.meta.url` is not populated by the embedded module engine, so
    // `createRequire(import.meta.url)` passes `undefined`. Ask QuickJS for
    // the running module's name instead, skipping the shim's own virtual
    // name so a relative require resolves against the calling extension.
    let function = Function::new(ctx.clone(), move |ctx: Ctx<'js>| -> String {
        let mut name = String::new();
        for level in 0..4 {
            let Some(atom) = ctx.script_or_module_name(level) else {
                break;
            };
            let Ok(candidate) = atom.to_string() else {
                break;
            };
            if candidate.starts_with("virtual:") || candidate.is_empty() {
                continue;
            }
            name = candidate;
            break;
        }
        name
    })?;
    ops.set("nodeCurrentModule", function)?;

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
        assert_eq!(normalize_posix("/a/b/../c/./d"), "/a/c/d");
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
        // hex/base64 encode_text DECODE a textual representation, so the
        // source must itself be valid in that encoding for the round trip.
        for (text, encoding) in [
            ("pi-rs", "utf8"),
            ("pi-rs", "latin1"),
            ("6869", "hex"),
            ("cGktcnM=", "base64"),
        ] {
            let bytes = encode_text(text, encoding).expect("encode");
            assert_eq!(decode_bytes(&bytes, encoding).expect("decode"), text);
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
    fn mkdtemp_sync_creates_a_unique_directory_under_the_prefix() {
        let directory = tempfile::tempdir().expect("temp dir");
        let prefix = format!("{}/sol-pi-", directory.path().display());
        let first = mkdtemp_sync(&prefix).expect("first mkdtemp");
        let second = mkdtemp_sync(&prefix).expect("second mkdtemp");
        assert_ne!(first, second);
        assert!(first.starts_with(&prefix));
        assert_eq!(first.len(), prefix.len() + 6);
        assert!(
            first[prefix.len()..]
                .chars()
                .all(|c| MKDTEMP_ALPHABET.contains(&(c as u8)))
        );
        assert!(std::path::Path::new(&first).is_dir());
        assert!(std::path::Path::new(&second).is_dir());

        let missing = format!("{}/missing-", directory.path().display());
        let error = mkdtemp_sync(&format!("{missing}/child-")).unwrap_err();
        assert_eq!(error["code"], "ENOENT");
    }

    #[test]
    fn chmod_mode_parsing_and_unix_permissions() {
        assert_eq!(parse_chmod_mode("493").expect("decimal"), 0o755);
        assert_eq!(parse_chmod_mode("0").expect("zero"), 0);
        assert!(parse_chmod_mode("-1").is_err());
        assert!(parse_chmod_mode("abc").is_err());

        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("secret");
        std::fs::write(&path, b"x").expect("write");
        #[cfg(unix)]
        {
            chmod_sync(path.to_str().expect("utf8 path"), "448").expect("chmod");
            let permissions = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(permissions & 0o777, 0o700);
        }
        #[cfg(not(unix))]
        {
            chmod_sync(path.to_str().expect("utf8 path"), "448").expect("chmod no-op");
        }
        #[cfg(unix)]
        {
            let error = chmod_sync("/definitely/missing", "448").unwrap_err();
            assert_eq!(error["code"], "ENOENT");
        }
    }

    #[test]
    fn fd_helpers_round_trip_bytes_and_reject_closed_handles() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("fd.bin");
        std::fs::write(&path, b"abc").expect("write");
        let path = path.to_string_lossy().into_owned();
        let fd = open_fd(&path, "r+").expect("open");
        assert_eq!(read_fd(fd, 2, 0).expect("read"), b"ab");
        assert_eq!(write_fd(fd, b"Z", 1).expect("write"), 1);
        assert_eq!(std::fs::read(&path).expect("read file"), b"aZc");
        close_fd(fd).expect("close");
        assert_eq!(read_fd(fd, 1, 0).unwrap_err()["code"], "EBADF");
        assert_eq!(open_fd(&path, "wx").unwrap_err()["code"], "EEXIST");
        assert_eq!(open_fd(&path, "bad-flag").unwrap_err()["code"], "EINVAL");

        // Numeric POSIX flags (the values exported on `fs.constants`).
        let numeric = directory.path().join("numeric.bin");
        let numeric = numeric.to_string_lossy().into_owned();
        let numeric_fd = open_fd(&numeric, "65").expect("O_WRONLY | O_CREAT");
        close_fd(numeric_fd).expect("close");
        assert_eq!(std::fs::read(&numeric).expect("read"), b"");
        assert_eq!(open_fd(&numeric, "193").unwrap_err()["code"], "EEXIST");
        let read_fd_handle = open_fd(&numeric, "131072").expect("O_RDONLY | O_NOFOLLOW");
        close_fd(read_fd_handle).expect("close");
    }

    #[test]
    fn virtual_sources_cover_every_declared_specifier() {
        for specifier in NODE_BUILTINS.iter().chain(HOST_PACKAGES) {
            assert!(source(specifier).is_some(), "missing shim for {specifier}");
        }
        assert!(source("node:sqlite").is_none());
    }

    #[test]
    fn hmac_sha256_matches_rfc4231_vectors() {
        // RFC 4231 cases 1 and 2, plus a key longer than the 64-byte block
        // size to exercise the key-hashing branch.
        let repeated_key = vec![0x0b_u8; 20];
        assert_eq!(
            crypto_hmac("sha256", &repeated_key, b"Hi There").expect("hmac"),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            crypto_hmac("SHA-256", b"Jefe", b"what do ya want for nothing?").expect("hmac"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        let long_key: Vec<u8> = (0_u8..80).collect();
        assert_eq!(
            crypto_hmac("sha256", &long_key, b"long key path").expect("hmac"),
            "12ca2c961396e2355fd356109a525431ff41e6d9db96b68952362b0cdd2dc0d5"
        );
    }

    #[test]
    fn hmac_sha512_matches_rfc4231_and_rejects_unknown_digests() {
        // SHA-512 uses a 128-byte block; case 1 still uses a short key.
        let repeated_key = vec![0x0b_u8; 20];
        assert_eq!(
            crypto_hmac("sha512", &repeated_key, b"Hi There").expect("hmac"),
            concat!(
                "87aa7cdea5ef619d4ff0b4241a1d6cb02379f4e2ce4ec2787ad0b30545e17cde",
                "daa833b7d6b8a702038b274eaea3f4e4be9d914eeb61f1702e696c203a126854"
            )
        );
        assert!(crypto_hmac("md5", b"key", b"data").is_err());
        assert_eq!(
            crypto_digest("SHA-256", b"abc").expect("digest"),
            crypto_digest("sha256", b"abc").expect("digest")
        );
    }

    #[test]
    fn filter_env_drops_empty_invalid_and_credential_keys() {
        let entries = [
            ("GOOD_NAME".to_string(), "one".to_string()),
            (String::new(), "two".to_string()),
            ("BAD=NAME".to_string(), "three".to_string()),
            ("9BAD".to_string(), "four".to_string()),
            ("bad-name".to_string(), "five".to_string()),
            ("API_KEY".to_string(), "six".to_string()),
            ("SESSION_TOKEN".to_string(), "seven".to_string()),
            ("DB_PASSWORD".to_string(), "eight".to_string()),
            ("lower_ok".to_string(), "nine".to_string()),
            ("A1_B2".to_string(), "ten".to_string()),
        ];
        let filtered = filter_env(entries);
        let object = filtered.as_object().expect("env object");
        assert_eq!(object.len(), 3);
        assert_eq!(object["GOOD_NAME"], Json::String("one".to_string()));
        assert_eq!(object["lower_ok"], Json::String("nine".to_string()));
        assert_eq!(object["A1_B2"], Json::String("ten".to_string()));
        for rejected in ["API_KEY", "SESSION_TOKEN", "DB_PASSWORD"] {
            assert!(
                !object.contains_key(rejected),
                "{rejected} must be filtered"
            );
        }
    }

    #[test]
    fn fs_stats_reports_real_metadata_and_distinguishes_lstat() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("stats.txt");
        std::fs::write(&path, b"stats").expect("write");
        let stats = fs_stats(&std::fs::metadata(&path).expect("metadata"));
        assert_eq!(stats["isFile"], Json::Bool(true));
        assert_eq!(stats["isDirectory"], Json::Bool(false));
        assert_eq!(stats["isSymbolicLink"], Json::Bool(false));
        assert_eq!(stats["size"], 5);
        assert!(stats["mtimeMs"].as_f64().is_some_and(|millis| millis > 0.0));
        #[cfg(unix)]
        assert!(stats["mode"].as_u64().is_some_and(|mode| mode > 0));

        #[cfg(unix)]
        {
            let link = directory.path().join("link");
            std::os::unix::fs::symlink(&path, &link).expect("symlink");
            let followed = fs_stats(&std::fs::metadata(&link).expect("follow"));
            assert_eq!(followed["isSymbolicLink"], Json::Bool(false));
            assert_eq!(followed["isFile"], Json::Bool(true));
            let unfollowed = fs_stats(&std::fs::symlink_metadata(&link).expect("lstat"));
            assert_eq!(unfollowed["isSymbolicLink"], Json::Bool(true));
            assert_eq!(unfollowed["isFile"], Json::Bool(false));
        }
    }
}
