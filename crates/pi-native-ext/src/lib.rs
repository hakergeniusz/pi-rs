//! Native Rust extensions for pi-rs: an additive capability, not a port.
//!
//! Upstream pi (`earendil-works/pi`, pinned 1.5.2) supports extensions only
//! through its `JavaScript` / `TypeScript` runtime (`extensions/index.ts`,
//! `extensions/runner.ts`). There is no upstream file for anything in this
//! crate. This is a deliberate pi-rs extension point so an extension can be
//! written in Rust and change almost any part of the host.
//!
//! # Stability boundary
//!
//! Rust has no stable ABI. A `String`, a `serde_json::Value`, or a trait object
//! passed through a `dlopen` boundary is undefined behaviour across compiler
//! or crate versions. The boundary in [`abi`] is therefore a plain C ABI with
//! a `u32` version check and NUL-terminated `JSON` strings, plus a
//! free-function exported by the extension itself. That contract, not Rust's
//! type system, is what stays stable. Bump [`abi::ABI_VERSION`] whenever the
//! symbol set or a payload shape changes incompatibly.
//!
//! # Layout
//!
//! * [`abi`] - the C ABI: symbol names, version, and `JSON` payload types.
//! * [`loader`] - the only module with `unsafe`: `libloading`-backed loading,
//!   symbol resolution, string marshalling, and shutdown.
//! * [`set`] - discovery-agnostic registry, the [`NativeExtension`] trait for
//!   compiled-in extensions, and serialized tool / hook dispatch.
//!
//! Loading nothing is inert: an empty [`ExtensionSet`] allocates no libraries
//! and every dispatch method is a no-op.

pub mod abi;
pub mod loader;
pub mod set;

pub use abi::{ABI_VERSION, CallRequest, CallResult, DeclaredTool, InitConfig, InitResult};
pub use loader::{LoadError, LoadedLibrary};
pub use set::{ExtensionSet, LoadSpec, NativeExtension};
