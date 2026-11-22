//! Host wiring for the telemetry seam at startup and shutdown.
//!
//! Ground truth (all line numbers refer to earendil-works/pi v1.5.2):
//!
//! * `packages/coding-agent/src/modes/interactive/interactive-mode.ts:1373-1403`
//!   — `getChangelogForDisplay`: resumed sessions (with messages) skip the
//!   changelog and the ping; a missing saved version is a fresh install (ping
//!   and record); newer entries are an update (ping and record); an unreadable
//!   saved version is recorded without a ping (`:1391-1395`).
//! * `.../interactive-mode.ts:1472-1489` — `reportInstallTelemetry` and its
//!   `PI_OFFLINE`/telemetry gates.
//! * `packages/coding-agent/src/core/telemetry.ts:3-13` — the env-over-setting
//!   install-telemetry gate.
//! * `packages/coding-agent/src/utils/changelog.ts:193-205` — `getNewEntries`.
//!
//! # Deviation (mechanism only)
//!
//! Upstream v1.5.2 has no span/metric exporter at all. The endpoint setting
//! ([`SETTING_OTLP_ENDPOINT`]) and environment variable
//! ([`ENV_OTLP_ENDPOINT`]) are additive: the exporter is built only when one
//! of them names a non-empty endpoint, and even then it posts through the
//! inert [`exporters::NoopHttpTransport`] unless a host injects a real one via
//! [`HostTelemetry::with_transport`]. The ping likewise uses the inert
//! [`ping::NoopPingTransport`] by default. With default wiring no socket is
//! ever opened, and neither payload carries credentials, prompts or session
//! data (the encoder redacts attributes, `exporters.rs:42-138`). A host can
//! opt into real I/O with [`install_from_env`] /
//! [`HostTelemetry::with_real_transports`], which install the reqwest-backed
//! [`exporters::ReqwestHttpTransport`] and [`ping::HttpPingTransport`] behind
//! exactly the same gates.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::changelog::{ChangelogEntry, get_new_entries};

use super::exporters::{
    BatchExporter, ExportReport, HttpTransport, NoopExporter, NoopHttpTransport, OtlpHttpExporter,
    ReqwestHttpTransport, TelemetryExporter, export_telemetry,
};
use super::ping::{ENV_OFFLINE, HttpPingTransport, NoopPingTransport, PingOutcome, PingTransport};
use super::{ENV_TELEMETRY, Telemetry, construct_telemetry, is_install_telemetry_enabled};

/// Additive OTLP endpoint override. `PI_RS_` keeps it distinct from any
/// variable a real TypeScript pi install might read.
pub const ENV_OTLP_ENDPOINT: &str = "PI_RS_OTLP_ENDPOINT";
/// Additive global setting naming the same endpoint (empty means unset).
pub const SETTING_OTLP_ENDPOINT: &str = "telemetryOtlpEndpoint";
/// `lastChangelogVersion` (`interactive-mode.ts:1379`, `:1393`, `:1397`).
pub const LAST_CHANGELOG_VERSION_SETTING: &str = "lastChangelogVersion";

/// The three env inputs read at startup; a default value is inert.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WiringEnv {
    /// `process.env.PI_TELEMETRY` (`core/telemetry.ts:10`).
    pub telemetry: Option<String>,
    /// `process.env.PI_OFFLINE` (`interactive-mode.ts:1473`).
    pub offline: Option<String>,
    /// Additive OTLP endpoint override.
    pub otlp_endpoint: Option<String>,
}

impl WiringEnv {
    /// Read the process environment once at startup. The binary alone reads
    /// real env; tests pass an explicit [`WiringEnv::default`] or fixture.
    #[must_use]
    pub fn from_process() -> Self {
        Self {
            telemetry: std::env::var(ENV_TELEMETRY).ok(),
            offline: std::env::var(ENV_OFFLINE).ok(),
            otlp_endpoint: std::env::var(ENV_OTLP_ENDPOINT).ok(),
        }
    }
}

/// The resolved, side-effect-free startup decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostTelemetryConfig {
    /// `settingsManager.getEnableInstallTelemetry()` (`core/telemetry.ts:12`).
    pub settings_enabled: bool,
    /// `PI_TELEMETRY`, which overrides the setting when present.
    pub telemetry_env: Option<String>,
    /// `PI_OFFLINE`, whose JS truthiness blocks the ping.
    pub offline_env: Option<String>,
    /// Endpoint from the env override or the setting; `None` disables export.
    pub otlp_endpoint: Option<String>,
}

impl HostTelemetryConfig {
    /// Compose settings, env and the additive endpoint into one decision. An
    /// empty endpoint (from either source) is treated as unconfigured.
    #[must_use]
    pub fn resolve(
        settings_enabled: bool,
        endpoint_setting: Option<&str>,
        env: &WiringEnv,
    ) -> Self {
        let otlp_endpoint = env
            .otlp_endpoint
            .as_deref()
            .or(endpoint_setting)
            .map(str::trim)
            .filter(|endpoint| !endpoint.is_empty())
            .map(str::to_string);
        Self {
            settings_enabled,
            telemetry_env: env.telemetry.clone(),
            offline_env: env.offline.clone(),
            otlp_endpoint,
        }
    }

    /// The exact `isInstallTelemetryEnabled` gate (`core/telemetry.ts:8-13`).
    #[must_use]
    pub fn install_telemetry_enabled(&self) -> bool {
        is_install_telemetry_enabled(self.settings_enabled, self.telemetry_env.as_deref())
    }

    /// `if (process.env.PI_OFFLINE)` (`interactive-mode.ts:1473`).
    #[must_use]
    pub fn offline(&self) -> bool {
        super::ping::offline_enabled(self.offline_env.as_deref())
    }

    /// Exporting is additive and requires all three: telemetry enabled, not
    /// offline, and an explicitly configured endpoint.
    #[must_use]
    pub fn export_enabled(&self) -> bool {
        self.install_telemetry_enabled() && !self.offline() && self.otlp_endpoint.is_some()
    }
}

/// What `getChangelogForDisplay` should do (`interactive-mode.ts:1378-1399`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangelogPingDecision {
    /// Resumed session, unreadable update, or no newer entries.
    Skip,
    /// Unreadable saved version: record the current one, but do not ping.
    RecordVersionOnly,
    /// Fresh install or detected update: record the version and ping.
    PingAndRecord,
}

/// The pure changelog half of the startup wiring. `has_messages` is upstream's
/// `this.session.state.messages.length > 0` (`:1377`).
#[must_use]
pub fn changelog_ping_decision(
    has_messages: bool,
    last_version: Option<&str>,
    entries: &[ChangelogEntry],
) -> ChangelogPingDecision {
    if has_messages {
        return ChangelogPingDecision::Skip;
    }
    let Some(last_version) = last_version else {
        return ChangelogPingDecision::PingAndRecord; // fresh install (`:1381-1386`)
    };
    match get_new_entries(entries, last_version) {
        None => ChangelogPingDecision::RecordVersionOnly, // `:1391-1395`
        Some(new_entries) if new_entries.is_empty() => ChangelogPingDecision::Skip,
        Some(_) => ChangelogPingDecision::PingAndRecord, // `:1396-1399`
    }
}

/// The binary's startup/shutdown telemetry surface: the recorder, the
/// exporter (noop unless configured) and the install-ping transport.
pub struct HostTelemetry {
    config: HostTelemetryConfig,
    telemetry: Arc<dyn Telemetry>,
    exporter: Arc<BatchExporter>,
    ping: Arc<dyn PingTransport>,
    /// A flush exports the recorder's full snapshot, so doing it twice would
    /// duplicate every span and metric. Startup/shutdown flushes once.
    flushed: AtomicBool,
}

impl HostTelemetry {
    /// Inert wiring: no ping request and no HTTP transport.
    #[must_use]
    pub fn new(config: HostTelemetryConfig) -> Self {
        Self::with_transport(
            config,
            NoopHttpTransport,
            Arc::new(NoopPingTransport) as Arc<dyn PingTransport>,
        )
    }

    /// Wire custom (still caller-provided) transports. Used by tests and by a
    /// host that wants real I/O; the binary's [`Self::new`] stays inert.
    #[must_use]
    pub fn with_transport<T: HttpTransport + 'static>(
        config: HostTelemetryConfig,
        transport: T,
        ping: Arc<dyn PingTransport>,
    ) -> Self {
        let telemetry = construct_telemetry(config.install_telemetry_enabled());
        let inner: Arc<dyn TelemetryExporter> = if config.export_enabled() {
            let endpoint = config
                .otlp_endpoint
                .clone()
                .expect("export_enabled implies an endpoint");
            Arc::new(
                OtlpHttpExporter::new(endpoint, transport)
                    .with_header("Content-Type", "application/json")
                    .with_signals(true, true),
            )
        } else {
            Arc::new(NoopExporter)
        };
        Self {
            config,
            telemetry,
            exporter: Arc::new(BatchExporter::new(inner)),
            ping,
            flushed: AtomicBool::new(false),
        }
    }

    /// Real, reqwest-backed transports behind the same gates. [`Self::new`]
    /// stays inert; a host opts in to I/O by calling this (or
    /// [`install_from_env`]). Every request still depends on the resolved
    /// config: the ping needs telemetry enabled and no `PI_OFFLINE`, the
    /// exporter additionally needs an explicitly configured endpoint.
    #[must_use]
    pub fn with_real_transports(config: HostTelemetryConfig) -> Self {
        Self::with_transport(
            config,
            ReqwestHttpTransport::new(),
            Arc::new(HttpPingTransport::new()) as Arc<dyn PingTransport>,
        )
    }

    /// The resolved gate, for diagnostics and tests.
    #[must_use]
    pub fn config(&self) -> &HostTelemetryConfig {
        &self.config
    }

    /// The recorder handed to startup code (in-memory only when enabled).
    #[must_use]
    pub fn telemetry(&self) -> Arc<dyn Telemetry> {
        Arc::clone(&self.telemetry)
    }

    /// The batched exporter, for diagnostics and tests.
    #[must_use]
    pub fn exporter(&self) -> &Arc<BatchExporter> {
        &self.exporter
    }

    /// `reportInstallTelemetry` (`interactive-mode.ts:1472-1489`) with the
    /// injected transport. Errors are swallowed upstream; the outcome exists
    /// only for observability.
    pub fn report_install_ping(&self, version: &str) -> PingOutcome {
        super::ping::report_install_ping(
            version,
            self.config.settings_enabled,
            self.config.telemetry_env.as_deref(),
            self.config.offline_env.as_deref(),
            self.ping.as_ref(),
        )
    }

    /// Flush the recorded snapshot once; later calls are no-ops (a second
    /// pass would duplicate every record). With default wiring this is also
    /// a no-op (the inner exporter is [`NoopExporter`]).
    pub fn flush(&self) -> ExportReport {
        if self.flushed.swap(true, Ordering::SeqCst) || !self.config.export_enabled() {
            return ExportReport::default();
        }
        let mut report = export_telemetry(self.telemetry.as_ref(), self.exporter.as_ref());
        if let Err(error) = self.exporter.flush() {
            report.errors.push(error);
        }
        report
    }

    /// Best-effort shutdown: drain only queued batches, never re-export the
    /// recorder's snapshot. Never fails the CLI.
    pub fn shutdown(&self) {
        self.exporter.shutdown();
    }
}

/// Build a [`HostTelemetry`] with real transports whose gates are resolved from
/// the process environment plus the caller's persisted settings. This is the
/// integration entry point for the binary: with default env and no endpoint,
/// neither the ping nor the exporter opens a socket.
///
/// `settings_enabled` is the result of `settingsManager.getEnableInstallTelemetry()`;
/// `endpoint_setting` is the additive OTLP setting (`None` means env-only).
#[must_use]
pub fn install_from_env_with_settings(
    settings_enabled: bool,
    endpoint_setting: Option<&str>,
) -> HostTelemetry {
    HostTelemetry::with_real_transports(HostTelemetryConfig::resolve(
        settings_enabled,
        endpoint_setting,
        &WiringEnv::from_process(),
    ))
}

/// [`install_from_env_with_settings`] without a persisted endpoint setting.
#[must_use]
pub fn install_from_env(settings_enabled: bool) -> HostTelemetry {
    install_from_env_with_settings(settings_enabled, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::{
        SpanOptions, exporters::FakeHttpTransport, ping::RecordingPingTransport,
    };

    fn entry(major: u32, minor: u32, patch: u32) -> ChangelogEntry {
        ChangelogEntry {
            major,
            minor,
            patch,
            content: String::new(),
        }
    }

    /// Config resolution mirrors the upstream env-over-setting gate and keeps
    /// an unconfigured endpoint absent (no exporter, no network).
    #[test]
    fn config_resolution_is_inert_by_default() {
        let env = WiringEnv::default();
        let config = HostTelemetryConfig::resolve(false, None, &env);
        assert!(!config.install_telemetry_enabled());
        assert!(!config.offline());
        assert!(!config.export_enabled());
        assert_eq!(config.otlp_endpoint, None);

        // Env overrides the setting in both directions, exactly like
        // `isInstallTelemetryEnabled` (core/telemetry.ts:8-13).
        let enabled = HostTelemetryConfig::resolve(
            false,
            None,
            &WiringEnv {
                telemetry: Some("yes".to_string()),
                ..WiringEnv::default()
            },
        );
        assert!(enabled.install_telemetry_enabled());
        let disabled = HostTelemetryConfig::resolve(
            true,
            None,
            &WiringEnv {
                telemetry: Some("0".to_string()),
                ..WiringEnv::default()
            },
        );
        assert!(!disabled.install_telemetry_enabled());

        // Only a non-empty endpoint from either source enables exporting,
        // and the env override wins over the setting.
        assert!(!HostTelemetryConfig::resolve(true, Some(""), &env).export_enabled());
        assert!(!HostTelemetryConfig::resolve(true, Some("  "), &env).export_enabled());
        let from_setting = HostTelemetryConfig::resolve(true, Some(" http://collector "), &env);
        assert_eq!(
            from_setting.otlp_endpoint.as_deref(),
            Some("http://collector")
        );
        assert!(from_setting.export_enabled());
        let from_env = HostTelemetryConfig::resolve(
            true,
            Some("http://setting"),
            &WiringEnv {
                otlp_endpoint: Some("http://env".to_string()),
                ..WiringEnv::default()
            },
        );
        assert_eq!(from_env.otlp_endpoint.as_deref(), Some("http://env"));

        // `PI_OFFLINE` uses JS truthiness: even "0" blocks the ping/export.
        let offline = HostTelemetryConfig::resolve(
            true,
            Some("http://collector"),
            &WiringEnv {
                offline: Some("0".to_string()),
                ..WiringEnv::default()
            },
        );
        assert!(offline.offline());
        assert!(!offline.export_enabled());
    }

    /// The changelog half of `getChangelogForDisplay`
    /// (`interactive-mode.ts:1373-1403`): fresh install, update, unreadable
    /// saved version and resumed session.
    #[test]
    fn changelog_decision_mirrors_upstream() {
        let entries = [entry(1, 2, 3), entry(1, 1, 0)];
        assert_eq!(
            changelog_ping_decision(false, None, &entries),
            ChangelogPingDecision::PingAndRecord
        );
        assert_eq!(
            changelog_ping_decision(false, Some("1.1.9"), &entries),
            ChangelogPingDecision::PingAndRecord
        );
        // Prerelease suffixes only read the leading triple (changelog.ts:185).
        assert_eq!(
            changelog_ping_decision(false, Some("1.1.9-20261020.7"), &entries),
            ChangelogPingDecision::PingAndRecord
        );
        assert_eq!(
            changelog_ping_decision(false, Some("1.2.3"), &entries),
            ChangelogPingDecision::Skip
        );
        assert_eq!(
            changelog_ping_decision(false, Some("not a version"), &entries),
            ChangelogPingDecision::RecordVersionOnly
        );
        assert_eq!(
            changelog_ping_decision(true, None, &entries),
            ChangelogPingDecision::Skip
        );
        // pi-rs ships no CHANGELOG.md (changelog.rs:6-8); without a saved
        // version it is still a fresh install and must ping.
        assert_eq!(
            changelog_ping_decision(false, None, &[]),
            ChangelogPingDecision::PingAndRecord
        );
    }

    /// The ping passes through upstream's gates; the default transport never
    /// records a request even when the gate opens.
    #[test]
    fn ping_is_gated_and_default_transport_is_inert() {
        let recorder = Arc::new(RecordingPingTransport::default());
        let disabled = HostTelemetry::with_transport(
            HostTelemetryConfig::resolve(false, None, &WiringEnv::default()),
            NoopHttpTransport,
            Arc::clone(&recorder) as Arc<dyn PingTransport>,
        );
        assert_eq!(
            disabled.report_install_ping("1.0.0"),
            PingOutcome::SkippedDisabled
        );
        assert_eq!(recorder.requests(), Vec::new());

        let offline = HostTelemetry::with_transport(
            HostTelemetryConfig::resolve(
                true,
                None,
                &WiringEnv {
                    offline: Some("1".to_string()),
                    ..WiringEnv::default()
                },
            ),
            NoopHttpTransport,
            Arc::clone(&recorder) as Arc<dyn PingTransport>,
        );
        assert_eq!(
            offline.report_install_ping("1.0.0"),
            PingOutcome::SkippedOffline
        );
        assert_eq!(recorder.requests(), Vec::new());

        let enabled = HostTelemetry::with_transport(
            HostTelemetryConfig::resolve(true, None, &WiringEnv::default()),
            NoopHttpTransport,
            Arc::clone(&recorder) as Arc<dyn PingTransport>,
        );
        assert_eq!(
            enabled.report_install_ping("1.0.0"),
            PingOutcome::Sent { status: 200 }
        );
        assert_eq!(recorder.requests().len(), 1);
        assert_eq!(
            recorder.requests()[0].url,
            "https://pi.dev/api/report-install?version=1.0.0"
        );

        // The binary's default wiring returns 202 without touching the
        // recording transport at all.
        let inert = HostTelemetry::new(HostTelemetryConfig::resolve(
            true,
            None,
            &WiringEnv::default(),
        ));
        assert_eq!(
            inert.report_install_ping("1.0.0"),
            PingOutcome::Sent { status: 202 }
        );
    }

    /// Flushing without an endpoint cannot send anything, even if the
    /// recorder has data. With an injected transport and endpoint, both
    /// signals are posted and the batch drains.
    #[test]
    fn exporter_flush_requires_an_endpoint_and_drains_when_configured() {
        let inert = HostTelemetry::new(HostTelemetryConfig::resolve(
            true,
            None,
            &WiringEnv::default(),
        ));
        inert
            .telemetry()
            .start_span(SpanOptions::new("work"), &mut |_| {});
        assert_eq!(inert.flush(), ExportReport::default());
        assert_eq!(inert.exporter().exports(), 0);

        let http = Arc::new(FakeHttpTransport::default());
        let configured = HostTelemetry::with_transport(
            HostTelemetryConfig::resolve(true, Some("http://collector"), &WiringEnv::default()),
            Arc::clone(&http) as Arc<dyn HttpTransport>,
            Arc::new(NoopPingTransport),
        );
        configured
            .telemetry()
            .start_span(SpanOptions::new("work"), &mut |_| {});
        configured
            .telemetry()
            .counter("count", &super::super::InstrumentOptions::default())
            .add(1.0, crate::telemetry::SpanAttributes::new());
        let report = configured.flush();
        assert_eq!(report.spans, 1);
        assert_eq!(report.metrics, 1);
        assert_eq!(report.errors, Vec::new());
        assert_eq!(http.requests().len(), 2);
        assert!(http.requests().iter().all(|request| {
            request.endpoint == "http://collector"
                && request.headers.get("Content-Type").map(String::as_str)
                    == Some("application/json")
        }));
        assert_eq!(configured.exporter().queued(), (0, 0));
        // A second flush and shutdown must not duplicate the snapshot.
        assert_eq!(configured.flush(), ExportReport::default());
        configured.shutdown();
        assert_eq!(http.requests().len(), 2);
    }

    /// Real transports are wired, but the resolved gate still blocks all I/O
    /// when telemetry is disabled: no ping and no export, even though the
    /// clients exist.
    #[test]
    fn real_transports_stay_gated_by_default() {
        let host = HostTelemetry::with_real_transports(HostTelemetryConfig::resolve(
            false,
            None,
            &WiringEnv::default(),
        ));
        assert_eq!(
            host.report_install_ping("1.0.0"),
            PingOutcome::SkippedDisabled
        );
        assert_eq!(host.flush(), ExportReport::default());
        assert_eq!(host.exporter().exports(), 0);

        // `PI_OFFLINE` is JS truthy (even "0"), so it blocks the real ping.
        let offline = HostTelemetry::with_real_transports(HostTelemetryConfig::resolve(
            true,
            None,
            &WiringEnv {
                offline: Some("0".to_string()),
                ..WiringEnv::default()
            },
        ));
        assert_eq!(
            offline.report_install_ping("1.0.0"),
            PingOutcome::SkippedOffline
        );
    }
}
