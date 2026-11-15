//! The constrained-sampling config a tool declares: pi's
//! `ConstrainedSamplingConfig` (`packages/ai/src/types.ts:693-719`).
//!
//! It lives in pi-ai because every consumer names it: pi-agent's `ToolSpec`,
//! the provider adapters that resolve it onto the wire, and the extension host
//! that reads it off a JS `ToolDefinition`. Only the shape lives here — the
//! resolver is adapter-side.
//!
//! The serialization is hand-written because pi's type is a union of a boolean
//! and two tagged objects — `false`, `{"type":"json_schema","strict":"prefer"}`
//! and `{"type":"grammar","variants":{"openai_lark":"…"}}` — which no derive
//! reproduces. Key order is pi's, because the declaration is compared
//! serialized (`toToolDeclaration`, `transcript.ts:123-130`).

use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// pi's `GrammarFormat` (`types.ts:693-694`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrammarFormat {
    Lark,
    Regex,
}

impl GrammarFormat {
    /// The value `OpenAI`'s `format.grammar.syntax` field takes.
    pub fn as_wire(&self) -> &'static str {
        match self {
            GrammarFormat::Lark => "lark",
            GrammarFormat::Regex => "regex",
        }
    }
}

/// pi's `GrammarVariants` (`types.ts:696`): provider-specific encodings of the
/// same language. A variant absent from the map is absent from the wire.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrammarVariants {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openai_lark: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openai_regex: Option<String>,
}

/// pi's `strict` (`"prefer" | "require"`), `types.ts:710-713`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StrictMode {
    Prefer,
    Require,
}

/// pi's `ConstrainedSamplingConfig` (`types.ts:699-713`) as it appears on a
/// tool: `false` opts out entirely, and the two variants ask the provider to
/// constrain the model's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConstrainedSampling {
    /// pi's `constrainedSampling: false` — never constrain this tool.
    Disabled,
    JsonSchema {
        strict: StrictMode,
    },
    Grammar {
        variants: GrammarVariants,
    },
}

impl ConstrainedSampling {
    /// pi's `{ type: "grammar", variants: { openai_lark } }`.
    pub fn openai_lark(grammar: impl Into<String>) -> ConstrainedSampling {
        ConstrainedSampling::Grammar {
            variants: GrammarVariants {
                openai_lark: Some(grammar.into()),
                openai_regex: None,
            },
        }
    }

    /// pi's `{ type: "json_schema", strict: "prefer" }`.
    pub fn json_schema_prefer() -> ConstrainedSampling {
        ConstrainedSampling::JsonSchema {
            strict: StrictMode::Prefer,
        }
    }

    /// The config's wire discriminant: `false`, `json_schema`, or `grammar`.
    pub fn type_name(&self) -> &'static str {
        match self {
            ConstrainedSampling::Disabled => "false",
            ConstrainedSampling::JsonSchema { .. } => "json_schema",
            ConstrainedSampling::Grammar { .. } => "grammar",
        }
    }
}

impl Serialize for ConstrainedSampling {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            ConstrainedSampling::Disabled => serializer.serialize_bool(false),
            ConstrainedSampling::JsonSchema { strict } => {
                let mut state = serializer.serialize_struct("ConstrainedSampling", 2)?;
                state.serialize_field("type", "json_schema")?;
                state.serialize_field("strict", strict)?;
                state.end()
            }
            ConstrainedSampling::Grammar { variants } => {
                let mut state = serializer.serialize_struct("ConstrainedSampling", 2)?;
                state.serialize_field("type", "grammar")?;
                state.serialize_field("variants", variants)?;
                state.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for ConstrainedSampling {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        if let Some(flag) = value.as_bool() {
            return if flag {
                Err(serde::de::Error::custom(
                    "constrainedSampling: true is not a valid config",
                ))
            } else {
                Ok(ConstrainedSampling::Disabled)
            };
        }
        let Some(object) = value.as_object() else {
            return Err(serde::de::Error::custom(
                "constrainedSampling must be a boolean or an object",
            ));
        };
        let kind = object
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| serde::de::Error::custom("constrainedSampling is missing its `type`"))?;
        match kind {
            "json_schema" => {
                let strict = object
                    .get("strict")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        serde::de::Error::custom(
                            "json_schema constrainedSampling is missing `strict`",
                        )
                    })?;
                Ok(ConstrainedSampling::JsonSchema {
                    strict: match strict {
                        "prefer" => StrictMode::Prefer,
                        "require" => StrictMode::Require,
                        other => {
                            return Err(serde::de::Error::custom(format!(
                                "unknown strict mode `{other}`"
                            )));
                        }
                    },
                })
            }
            "grammar" => {
                let variants = object.get("variants").cloned().ok_or_else(|| {
                    serde::de::Error::custom("grammar constrainedSampling is missing `variants`")
                })?;
                Ok(ConstrainedSampling::Grammar {
                    variants: serde_json::from_value(variants)
                        .map_err(|error| serde::de::Error::custom(error.to_string()))?,
                })
            }
            other => Err(serde::de::Error::custom(format!(
                "unknown constrainedSampling type `{other}`"
            ))),
        }
    }
}
