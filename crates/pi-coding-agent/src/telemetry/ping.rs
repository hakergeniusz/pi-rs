//! Anonymous install/update ping, ported from the coding-agent host.
//!
//! Ground truth (all line numbers refer to earendil-works/pi v1.5.2):
//!
//! * `packages/coding-agent/src/modes/interactive/interactive-mode.ts:1472-1489`
//!   — `reportInstallTelemetry`: skip when `PI_OFFLINE` is truthy, skip when
//!   install telemetry is disabled, then fire a `GET` to
//!   `https://pi.dev/api/report-install?version=…` with pi's `User-Agent` and
//!   a 5-second abort signal. Errors are swallowed. The call sites at
//!   `:1383-1386` and `:1396-1399` tie it to a fresh install / detected
//!   update via the changelog.
//! * `packages/coding-agent/src/core/telemetry.ts:3-13` — the
//!   `PI_TELEMETRY`/setting gate reused here.
//! * `packages/coding-agent/src/utils/pi-user-agent.ts:4-6` and
//!   `packages/ai/src/utils/identity.ts:75-82` — `formatUserAgent` in its
//!   detailed form.
//! * `packages/coding-agent/src/core/client-identity.ts:10-16` and
//!   `packages/ai/src/utils/identity.ts:24` — `getClientIdentity` and
//!   `PI_ATTRIBUTION`.
//!
//! # Deviation (mechanism only)
//!
//! Upstream calls `fetch` unconditionally when the gates pass. This port
//! keeps the exact gates and payload but routes the request through
//! [`PingTransport`]; the default transport is inert, so no network call is
//! made unless the host injects one. [`HttpPingTransport`] is the opt-in
//! reqwest-backed implementation; [`super::host`] records the wiring.

use std::fmt::Write;
use std::sync::{Arc, Mutex, PoisonError};

use super::{is_install_telemetry_enabled, is_truthy_env_flag};

/// `PI_OFFLINE` (`interactive-mode.ts:1473`; `cli/args.ts:465`).
pub const ENV_OFFLINE: &str = "PI_OFFLINE";
/// `pi.dev` install-report endpoint (`interactive-mode.ts:1481`).
pub const INSTALL_PING_ENDPOINT: &str = "https://pi.dev/api/report-install";
/// Upstream's request timeout, `AbortSignal.timeout(5000)` (`:1485`).
pub const PING_TIMEOUT_MS: u64 = 5000;

/// `ClientAttribution` / `PI_ATTRIBUTION`
/// (`packages/ai/src/utils/identity.ts:24`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClientAttribution {
    pub name: &'static str,
    pub url: &'static str,
    pub categories: &'static [&'static str],
}

/// The attribution sent to providers while install telemetry is enabled
/// (`identity.ts:24`, `client-identity.ts:14`).
pub const PI_ATTRIBUTION: ClientAttribution = ClientAttribution {
    name: "pi",
    url: "https://pi.dev",
    categories: &["cli-agent"],
};

/// `ClientIdentity` (`client-identity.ts:10-16`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientIdentity {
    pub user_agent: &'static str,
    pub version: String,
    pub attribution: Option<ClientAttribution>,
}

/// `getClientIdentity` (`client-identity.ts:10-16`): attribution is present
/// only when the same install-telemetry gate enables the ping.
#[must_use]
pub fn get_client_identity(
    version: &str,
    settings_enabled: bool,
    telemetry_env: Option<&str>,
) -> ClientIdentity {
    ClientIdentity {
        user_agent: "detailed",
        version: version.to_string(),
        attribution: is_install_telemetry_enabled(settings_enabled, telemetry_env)
            .then_some(PI_ATTRIBUTION),
    }
}

/// `formatUserAgent` (`identity.ts:75-82`), in its detailed form. Rust has no
/// `process.versions`, so the runtime token is the literal `rust`; no
/// hostname, path, user or credential is ever included.
#[must_use]
pub fn format_user_agent(version: &str) -> String {
    format!(
        "pi/{version} ({}; rust; {})",
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

/// `encodeURIComponent` in the query string (`interactive-mode.ts:1481`).
fn percent_encode_query(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(byte));
            }
            other => {
                let _ = write!(out, "%{other:02X}");
            }
        }
    }
    out
}

/// The complete ping URL: only the version is sent (`:1481`).
#[must_use]
pub fn build_install_ping_url(version: &str) -> String {
    format!(
        "{INSTALL_PING_ENDPOINT}?version={}",
        percent_encode_query(version)
    )
}

/// `if (process.env.PI_OFFLINE)` (`interactive-mode.ts:1473`): upstream uses
/// JavaScript truthiness here — any non-empty string, including `0`, blocks
/// the ping. The CLI sets `PI_OFFLINE=1` for `--offline`, so both gates agree
/// in practice.
#[must_use]
pub fn offline_enabled(value: Option<&str>) -> bool {
    value.is_some_and(|text| !text.is_empty())
}

/// The complete anonymous request. There is deliberately no body, no session
/// id and no user data: only a version in the URL and pi's `User-Agent`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallPingRequest {
    pub url: String,
    pub user_agent: String,
}

/// A minimal response; upstream ignores it entirely.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PingResponse {
    pub status: u16,
}

/// A transport error. Upstream's `.catch(() => undefined)` (`:1488`) swallows
/// it, so this type exists only for hosts that want diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PingError {
    pub message: String,
}

/// The network seam for the ping. The default implementation is inert.
pub trait PingTransport: Send + Sync {
    fn send(&self, request: &InstallPingRequest) -> Result<PingResponse, PingError>;
}

impl<T: PingTransport + ?Sized> PingTransport for Arc<T> {
    fn send(&self, request: &InstallPingRequest) -> Result<PingResponse, PingError> {
        (**self).send(request)
    }
}

/// Default transport: returns `202` without opening a socket.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopPingTransport;

impl PingTransport for NoopPingTransport {
    fn send(&self, _request: &InstallPingRequest) -> Result<PingResponse, PingError> {
        Ok(PingResponse { status: 202 })
    }
}

/// The opt-in, reqwest-backed ping transport. It performs the exact `GET`
/// upstream issues (`interactive-mode.ts:1481-1486`) — version-only URL,
/// `User-Agent`, 5-second timeout — behind [`PingTransport`]. The gates in
/// [`report_install_ping`] still decide whether any socket is opened, and the
/// crate's default wiring never constructs this type ([`super::host`]).
#[derive(Clone, Debug)]
pub struct HttpPingTransport {
    client: reqwest::Client,
}

impl Default for HttpPingTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpPingTransport {
    /// A client whose timeout matches upstream's `AbortSignal.timeout(5000)`.
    #[must_use]
    pub fn new() -> Self {
        Self::with_timeout(std::time::Duration::from_millis(PING_TIMEOUT_MS))
    }

    /// A client with an explicit timeout (used by the loopback tests).
    #[must_use]
    pub fn with_timeout(timeout: std::time::Duration) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .expect("build telemetry ping client"),
        }
    }
}

impl PingTransport for HttpPingTransport {
    fn send(&self, request: &InstallPingRequest) -> Result<PingResponse, PingError> {
        let sent = crate::runtime::block_on(async {
            self.client
                .get(&request.url)
                .header(reqwest::header::USER_AGENT, request.user_agent.as_str())
                .send()
                .await
        });
        match sent {
            Ok(response) => Ok(PingResponse {
                status: response.status().as_u16(),
            }),
            Err(error) => Err(PingError {
                message: error.to_string(),
            }),
        }
    }
}

/// A fake transport that records requests and can fail a fixed number of
/// times, for tests that must not touch the network.
#[derive(Debug, Default)]
pub struct RecordingPingTransport {
    requests: Mutex<Vec<InstallPingRequest>>,
    failures: Mutex<usize>,
}

impl RecordingPingTransport {
    /// A transport failing the next `failures` calls.
    #[must_use]
    pub fn new(failures: usize) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            failures: Mutex::new(failures),
        }
    }

    /// All captured requests, in order.
    #[must_use]
    pub fn requests(&self) -> Vec<InstallPingRequest> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl PingTransport for RecordingPingTransport {
    fn send(&self, request: &InstallPingRequest) -> Result<PingResponse, PingError> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request.clone());
        let mut failures = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
        if *failures > 0 {
            *failures -= 1;
            return Err(PingError {
                message: "fake ping failure".to_string(),
            });
        }
        Ok(PingResponse { status: 200 })
    }
}

/// Why a ping was or was not attempted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PingOutcome {
    SkippedOffline,
    SkippedDisabled,
    Sent { status: u16 },
    Failed,
}

/// `reportInstallTelemetry` (`interactive-mode.ts:1472-1489`), with its
/// `fetch` replaced by `transport`. Errors are swallowed exactly as upstream
/// does; the outcome is returned only for observability.
pub fn report_install_ping(
    version: &str,
    settings_enabled: bool,
    telemetry_env: Option<&str>,
    offline_env: Option<&str>,
    transport: &dyn PingTransport,
) -> PingOutcome {
    if offline_enabled(offline_env) {
        return PingOutcome::SkippedOffline;
    }
    if !is_install_telemetry_enabled(settings_enabled, telemetry_env) {
        return PingOutcome::SkippedDisabled;
    }
    let request = InstallPingRequest {
        url: build_install_ping_url(version),
        user_agent: format_user_agent(version),
    };
    match transport.send(&request) {
        Ok(response) => PingOutcome::Sent {
            status: response.status,
        },
        Err(_) => PingOutcome::Failed,
    }
}

/// [`report_install_ping`] with upstream's `process.env` defaults
/// (`interactive-mode.ts:1473`, `core/telemetry.ts:10`). Reads only.
pub fn report_install_ping_from_env(
    version: &str,
    settings_enabled: bool,
    transport: &dyn PingTransport,
) -> PingOutcome {
    report_install_ping(
        version,
        settings_enabled,
        std::env::var(super::ENV_TELEMETRY).ok().as_deref(),
        std::env::var(ENV_OFFLINE).ok().as_deref(),
        transport,
    )
}

/// Whether the env value is a truthy flag (`core/telemetry.ts:3-6`).
/// Re-exported here so hosts can gate on the same rule.
pub fn is_truthy(value: Option<&str>) -> bool {
    is_truthy_env_flag(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two gates upstream applies before any request: JS truthiness for
    /// `PI_OFFLINE` and the exact `PI_TELEMETRY` truthy set.
    #[test]
    fn env_gating_matches_upstream() {
        assert!(!offline_enabled(None));
        assert!(!offline_enabled(Some("")));
        assert!(offline_enabled(Some("0"))); // JS truthiness, not isTruthyEnvFlag
        assert!(offline_enabled(Some("1")));
        assert!(is_truthy(Some("true")));
        assert!(!is_truthy(Some("0")));

        let transport = RecordingPingTransport::default();
        assert_eq!(
            report_install_ping("1.0.0", true, None, Some(""), &transport),
            PingOutcome::Sent { status: 200 }
        );
        assert_eq!(
            report_install_ping("1.0.0", true, None, Some("1"), &transport),
            PingOutcome::SkippedOffline
        );
        assert_eq!(
            report_install_ping("1.0.0", true, Some("0"), None, &transport),
            PingOutcome::SkippedDisabled
        );
        assert_eq!(
            report_install_ping("1.0.0", false, Some("yes"), None, &transport),
            PingOutcome::Sent { status: 200 }
        );
        assert_eq!(transport.requests().len(), 2);
    }

    /// The payload is version-only, URL-encoded, with a runtime-neutral
    /// `User-Agent`; no path, hostname, model or session data leaks.
    #[test]
    fn payload_contains_only_version_and_runtime_neutral_agent() {
        let url = build_install_ping_url("0.1.0 alpha+1/2");
        assert_eq!(
            url,
            "https://pi.dev/api/report-install?version=0.1.0%20alpha%2B1%2F2"
        );
        let agent = format_user_agent("1.2.3");
        assert!(agent.starts_with("pi/1.2.3 ("));
        assert!(agent.contains(std::env::consts::OS));
        assert!(agent.contains(std::env::consts::ARCH));
        assert!(!agent.contains("/home"));
        assert!(
            !format!("{agent}{url}")
                .to_ascii_lowercase()
                .contains("token")
        );
        assert!(
            !format!("{agent}{url}")
                .to_ascii_lowercase()
                .contains("secret")
        );
    }

    /// Transport failures are swallowed; the default transport never opens a
    /// socket. Attribution follows the same gate as the ping.
    #[test]
    fn failures_are_swallowed_and_attribution_follows_gate() {
        let transport = RecordingPingTransport::new(1);
        assert_eq!(
            report_install_ping("1.0.0", true, None, None, &transport),
            PingOutcome::Failed
        );
        assert_eq!(
            report_install_ping("1.0.0", true, None, None, &transport),
            PingOutcome::Sent { status: 200 }
        );
        assert_eq!(
            report_install_ping("1.0.0", true, None, None, &NoopPingTransport),
            PingOutcome::Sent { status: 202 }
        );
        assert_eq!(transport.requests().len(), 2);

        assert_eq!(
            get_client_identity("1.2.3", true, None).attribution,
            Some(PI_ATTRIBUTION)
        );
        assert_eq!(
            get_client_identity("1.2.3", true, Some("0")).attribution,
            None
        );
        assert_eq!(PI_ATTRIBUTION.name, "pi");
        assert_eq!(PI_ATTRIBUTION.categories, &["cli-agent"]);
        assert_eq!(PING_TIMEOUT_MS, 5000);
    }

    /// The opt-in reqwest transport performs a real GET against a synthetic
    /// loopback server: version in the path, neutral `User-Agent`, and the
    /// response status surfaced. No external network is contacted.
    #[test]
    fn real_ping_transport_sends_get_to_loopback() {
        let (base, server) = crate::telemetry::exporters::loopback::spawn();
        let transport = HttpPingTransport::new();
        let request = InstallPingRequest {
            url: format!("{base}/api/report-install?version=1.0.0"),
            user_agent: format_user_agent("1.0.0"),
        };
        let response = transport.send(&request).expect("ping reaches loopback");
        assert_eq!(response.status, 200);

        let raw = server.join().expect("loopback thread");
        assert!(raw.starts_with("GET /api/report-install?version=1.0.0 HTTP/1.1\r\n"));
        let folded = raw.to_ascii_lowercase();
        assert!(
            folded
                .contains(&format!("user-agent: {}\r\n", request.user_agent).to_ascii_lowercase())
        );
        assert!(!folded.contains("authorization:"));
    }
}
