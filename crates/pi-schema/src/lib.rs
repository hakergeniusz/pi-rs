//! JSON Schema builders, compilation and validation without eval.
//!
//! Rust port of `@earendil-works/pi-schema` (TS source:
//! `packages/schema/src`). The TS library provides typed builders (`obj`,
//! `str`, `num`, `arr`, `union`, …) that return plain JSON Schema objects,
//! plus a `compile()` that builds validators into plain closures for drafts
//! 2020-12 and 7.
//!
//! Planned Rust API (implemented in P1+):
//! - builders: [`obj`], [`str`], [`num`], [`arr`], [`union`], …
//! - `Schema` newtype over `serde_json::Value`
//! - `compile(&Schema) -> Validator` with typed errors
//!
//! Nothing here is final; this module only anchors the crate for P0.

/// A JSON Schema document. Plain data so it serializes losslessly.
pub type JsonSchema = serde_json::Value;
