//! Grammar-constrained sampling hook for the codemode tool declaration.
//!
//! Ground truth:
//! `packages/coding-agent/src/extensions/codemode/tool.ts:433-440` declares
//! the `codemode` tool with `constrainedSampling: { type: "grammar", variants:
//! { openai_lark: CODEMODE_SOURCE_GRAMMAR } }`, and
//! `packages/codemode/src/source.ts:18-30` defines the grammar (ported as
//! [`crate::CODEMODE_SOURCE_GRAMMAR`]). The typed shape is pi-ai's
//! `ConstrainedSampling::Grammar { variants }`
//! (`packages/ai/src/types.ts:693-713`; Rust: `crates/pi-ai/src/constrained_sampling.rs:37-83`).
//!
//! This crate stays dependency-free: adding `pi-ai` just for the four-line
//! declaration would require a `Cargo.lock` change, which this slice cannot
//! make (the coordinator gates dependency changes serially). Instead the hook
//! returns the exact wire shape and the host converts it with
//! `serde_json::from_value::<pi_ai::ConstrainedSampling>(...)`. Tests below
//! compare against the typed serialization shape byte-for-byte.

use serde::Serialize;
use serde_json::{Value as Json, json};

use crate::CODEMODE_SOURCE_GRAMMAR;

/// A provider that can describe the codemode tool's constrained-sampling
/// config. Hosts implement this for their own tool-declaration type, using
/// `pi_ai::constrained_sampling::ConstrainedSampling::openai_lark` if they
/// already depend on pi-ai.
pub trait ConstrainedSamplingHook {
    /// The host's declaration type.
    type Config: Serialize;

    /// The config for a tool whose input is raw codemode source.
    fn codemode_constrained_sampling(&self) -> Self::Config;
}

/// The dependency-free implementation: upstream's grammar object as JSON.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CodemodeSourceGrammar;

impl ConstrainedSamplingHook for CodemodeSourceGrammar {
    type Config = Json;

    fn codemode_constrained_sampling(&self) -> Self::Config {
        codemode_grammar_sampling()
    }
}

/// Upstream's `constrainedSampling` value for the `codemode` tool
/// (`tool.ts:437`). Serializes to `{"type":"grammar","variants":{"openai_lark":"…"}}`,
/// the same bytes pi-ai's `ConstrainedSampling::openai_lark` produces.
#[must_use]
pub fn codemode_grammar_sampling() -> Json {
    json!({
        "type": "grammar",
        "variants": { "openai_lark": CODEMODE_SOURCE_GRAMMAR },
    })
}

/// The variant discriminant (`constrained_sampling.rs:89-95`).
#[must_use]
pub const fn constrained_sampling_type_name() -> &'static str {
    "grammar"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grammar_config_has_upstream_wire_shape() {
        let config = codemode_grammar_sampling();
        assert_eq!(config["type"], "grammar");
        assert_eq!(config["variants"]["openai_lark"], CODEMODE_SOURCE_GRAMMAR);
        assert!(
            config
                .get("variants")
                .and_then(Json::as_object)
                .is_some_and(|map| map.len() == 1)
        );
        assert_eq!(constrained_sampling_type_name(), "grammar");
    }

    #[test]
    fn serialization_matches_pi_ai_typed_shape() {
        // pi-ai's ConstrainedSampling serializes fields in this order: `type`,
        // then `variants` (`constrained_sampling.rs:98-115`). serde_json's map
        // is key-sorted by default, which gives the same order here.
        let expected = format!(
            "{{\"type\":\"grammar\",\"variants\":{{\"openai_lark\":{}}}}}",
            serde_json::to_string(CODEMODE_SOURCE_GRAMMAR).expect("grammar string")
        );
        assert_eq!(
            serde_json::to_string(&codemode_grammar_sampling()).expect("serialize"),
            expected
        );
    }

    #[test]
    fn host_hook_can_return_a_custom_type() {
        struct Host;
        impl ConstrainedSamplingHook for Host {
            type Config = &'static str;
            fn codemode_constrained_sampling(&self) -> Self::Config {
                "grammar"
            }
        }
        assert_eq!(Host.codemode_constrained_sampling(), "grammar");
        assert_eq!(
            CodemodeSourceGrammar.codemode_constrained_sampling(),
            codemode_grammar_sampling()
        );
    }
}
