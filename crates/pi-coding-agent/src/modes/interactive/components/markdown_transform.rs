//! The extension markdown-transform plumbing — `createMarkdownTransform` +
//! `applyMarkdownTransformers` (`modes/interactive/components/
//! markdown-transform.ts`) and the application half of the mode's
//! `getMarkdownTransformers` (`interactive-mode.ts:2362-2364`).
//!
//! Upstream builds one closure per markdown surface from the live
//! transformer list; the port clones a [`MarkdownTransformCtx`] into every
//! markdown component, and each surface's transform closure walks the
//! resolved chain — one extension transformer per load-order slot, each
//! leaving markdown of unregistered surfaces unchanged (`runner.ts:793-803`),
//! throws and non-string returns keeping the current markdown
//! (`markdown-transform.ts:28-38`).

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use pi_tui::components::markdown::MarkdownTransform;

#[cfg(feature = "ext-js")]
use std::sync::Mutex;
#[cfg(feature = "ext-js")]
use std::sync::atomic::Ordering;

/// The resolved extension chain shared with the mode. Upstream reads
/// `extensionRunner.getMarkdownTransformers()` live per component
/// construction; the port resolves once at setup and `/reload` — the same
/// lifecycle, because the chain only changes when the runner reloads.
#[derive(Clone, Default)]
pub struct MarkdownTransformCtx {
    /// The chain + the runner, only with the extensions feature.
    #[cfg(feature = "ext-js")]
    inner: Option<MarkdownTransformInner>,
}

#[cfg(feature = "ext-js")]
#[derive(Clone)]
struct MarkdownTransformInner {
    chain: Arc<Mutex<Vec<pi_ext_js::ResolvedMarkdownTransformer>>>,
    runner: Option<Arc<Mutex<pi_ext_js::ExtensionRunner>>>,
}

impl MarkdownTransformCtx {
    /// The mode-side construction: creates the shared chain handle and
    /// returns it next to the context, so the setup path can refill the
    /// chain in place (constructor + `/reload`).
    #[cfg(feature = "ext-js")]
    pub(crate) fn for_runner(
        runner: Option<Arc<Mutex<pi_ext_js::ExtensionRunner>>>,
    ) -> (
        Self,
        Arc<Mutex<Vec<pi_ext_js::ResolvedMarkdownTransformer>>>,
    ) {
        let chain = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                inner: Some(MarkdownTransformInner {
                    chain: Arc::clone(&chain),
                    runner,
                }),
            },
            chain,
        )
    }

    /// The transform for one markdown surface
    /// (`createMarkdownTransform`, `markdown-transform.ts:15-26`):
    /// `None` when no extension registered a transformer, leaving the
    /// component on plain options. `streaming` is read per render through
    /// the flag, so a reused component sees a stream end; `message_id` and
    /// `timestamp` are the persisted-entry fields, which this port's
    /// components do not carry yet (absent upstream too for live
    /// messages).
    pub fn make(
        &self,
        message_type: &'static str,
        streaming: Option<Arc<AtomicBool>>,
        message_id: Option<String>,
        timestamp: Option<String>,
    ) -> Option<MarkdownTransform> {
        #[cfg(feature = "ext-js")]
        {
            let inner = self.inner.as_ref()?;
            if inner
                .chain
                .lock()
                .expect("markdown transformers")
                .is_empty()
            {
                return None;
            }
            let closure_inner = inner.clone();
            Some(Arc::new(move |markdown: &str, width: usize| -> String {
                apply_markdown_transformers(
                    markdown,
                    width,
                    message_type,
                    streaming.as_ref().map(|flag| flag.load(Ordering::Relaxed)),
                    message_id.as_deref(),
                    timestamp.as_deref(),
                    &closure_inner,
                )
            }))
        }
        #[cfg(not(feature = "ext-js"))]
        {
            let _ = (message_type, streaming, message_id, timestamp);
            None
        }
    }
}

/// `applyMarkdownTransformers` (`markdown-transform.ts:28-38`): chain the
/// transformers in load order; a throw or a non-string return keeps the
/// current markdown and continues. The context crosses as JSON with the
/// optional fields absent — not `null` — when undefined.
#[cfg(feature = "ext-js")]
fn apply_markdown_transformers(
    markdown: &str,
    width: usize,
    message_type: &str,
    is_streaming: Option<bool>,
    message_id: Option<&str>,
    timestamp: Option<&str>,
    inner: &MarkdownTransformInner,
) -> String {
    use serde_json::{Map as JsonMap, Value as Json};

    let chain = inner.chain.lock().expect("markdown transformers").clone();
    if chain.is_empty() {
        return markdown.to_string();
    }
    // `MarkdownTransformContext` (`types.ts:1573-1582`).
    let mut context = JsonMap::new();
    context.insert("messageType".to_string(), Json::from(message_type));
    context.insert(
        "isStreaming".to_string(),
        Json::from(is_streaming.unwrap_or(false)),
    );
    context.insert("availableWidth".to_string(), Json::from(width));
    if let Some(id) = message_id {
        context.insert("messageId".to_string(), Json::from(id));
    }
    if let Some(timestamp) = timestamp {
        context.insert("timestamp".to_string(), Json::from(timestamp));
    }
    let context_json = Json::Object(context).to_string();

    let mut current = markdown.to_string();
    for resolved in &chain {
        if !resolved
            .message_types
            .iter()
            .any(|surface| surface == message_type)
        {
            continue;
        }
        let Some(runner) = inner.runner.as_ref() else {
            break;
        };
        if let Some(transformed) = runner
            .lock()
            .expect("extension runner mutex poisoned")
            .markdown_transform(resolved.ext_index, &current, &context_json)
        {
            current = transformed;
        }
    }
    current
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without the engine the context is inert: `make` is `None`, so every
    /// markdown component stays on plain options.
    #[test]
    fn default_ctx_makes_no_transform() {
        let ctx = MarkdownTransformCtx::default();
        assert!(ctx.make("assistant", None, None, None).is_none());
    }
}
