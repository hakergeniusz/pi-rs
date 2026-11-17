//! The interactive model-catalog refresh.
//!
//! Ground truth: `modes/interactive/model-catalog-refresh.ts` (1.5.2) —
//! `refreshModelCatalogs(runtime, signal)` shares one concurrent
//! `modelRuntime.refresh` between callers through a coordinator
//! (`ModelCatalogRefreshCoordinator`, waiters counted per runtime).
//!
//! Deviation (mechanism only, see HANDOFF.md): the port's compose is a
//! synchronous, process-global rebuild ([`crate::models_state`]) on the UI
//! thread, not an async network fetch — there is no concurrency to share and
//! no signal to race, so the coordinator collapses to this thin dispatch
//! (engine builds compose with the runner's registrations; engine-free
//! builds from the static catalog + models.json). Callers keep upstream's
//! refresh-status texts; the timeout arm never fires.

use std::sync::{Arc, Mutex};

use crate::models_state::ModelsRefreshResult;

/// `refreshModelCatalogs` (`model-catalog-refresh.ts:46-50`): one shared
/// all-catalog compose. The engine (when linked and bound) joins the
/// rebuild — extension-registered providers and virtual models recompose
/// with everything else.
#[cfg(feature = "ext-js")]
pub fn refresh_model_catalogs(
    runner: Option<&Arc<Mutex<pi_ext_js::ExtensionRunner>>>,
) -> ModelsRefreshResult {
    match runner {
        Some(runner) => crate::models_state::refresh_result_with_runner(
            &runner.lock().expect("extension runner mutex poisoned"),
        ),
        None => crate::models_state::refresh_result(),
    }
}

/// The engine-free shape: no runner to join.
#[cfg(not(feature = "ext-js"))]
pub fn refresh_model_catalogs(
    _runner: Option<&Arc<Mutex<pi_ext_js::ExtensionRunner>>>,
) -> ModelsRefreshResult {
    crate::models_state::refresh_result()
}
