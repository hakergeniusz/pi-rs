//! Streamable HTTP transport: JSON-RPC over POST with SSE and JSON
//! responses, a server-to-client GET stream, session capture, and stream
//! resumption.
//!
//! Ports `packages/mcp/src/transports/streamable-http.ts:1-579` (the
//! transport) and `transports/transport.ts:3` (the 16 MiB message cap);
//! the credential hook mirrors `AuthProvider` (`auth-provider.ts:6-24`).
//! Where upstream runs response streams on background tasks and routes
//! messages through client listeners, this port consumes the response
//! stream inline until the awaited reply arrives and hands the rest of the
//! stream to a background task; server-initiated requests and
//! notifications land on [`StreamableHttpTransport::inbound`], which the
//! host drains (the stdio transport's diagnostics channel). A reply the
//! transport never sees (invalid JSON-RPC, no reply in a JSON body)
//! surfaces immediately, where upstream leaves the request to its timeout.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::StreamExt;
use futures::stream::BoxStream;
use reqwest::Method;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::auth_provider::{AuthProvider, McpFetch, UnauthorizedContext};
use crate::error::McpError;
use crate::types::{
    InboundMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, JsonRpcVersion,
    classify_value, json_rpc_error_codes,
};

/// Body bytes an error response keeps (`MAX_ERROR_BODY_BYTES`,
/// `streamable-http.ts:20`); the rest is cancelled.
pub const MAX_ERROR_BODY_BYTES: usize = 8 * 1024;
/// Characters of the error body an error message carries
/// (`ERROR_MESSAGE_BODY_CHARS`, `streamable-http.ts:21`).
const ERROR_MESSAGE_BODY_CHARS: usize = 500;
/// Delay before the first stream reconnection unless the server sent a
/// `retry` field (`DEFAULT_RECONNECT_INITIAL_DELAY_MS`,
/// `streamable-http.ts:22`).
const DEFAULT_RECONNECT_INITIAL_DELAY_MS: u64 = 1_000;
/// Upper bound for the exponential reconnection backoff
/// (`DEFAULT_RECONNECT_MAX_DELAY_MS`, `streamable-http.ts:23`).
const DEFAULT_RECONNECT_MAX_DELAY_MS: u64 = 30_000;
/// Consecutive failed stream reconnections before giving up
/// (`DEFAULT_RECONNECT_MAX_RETRIES`, `streamable-http.ts:24`).
const DEFAULT_RECONNECT_MAX_RETRIES: u32 = 5;
/// Byte cap per SSE event and JSON response (`DEFAULT_MAX_MESSAGE_BYTES`,
/// `transport.ts:3`).
pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
/// How long `close()` waits for the session's DELETE (`AbortSignal.timeout`,
/// `streamable-http.ts:336`).
const CLOSE_TIMEOUT: Duration = Duration::from_secs(1);

/// Accept header of a JSON-RPC POST (`streamable-http.ts:290`).
const POST_ACCEPT: &str = "application/json, text/event-stream";
/// Accept header of the SSE streams (`streamable-http.ts:518`).
const SSE_ACCEPT: &str = "text/event-stream";

/// One parsed SSE event (`SseEvent`, `streamable-http.ts:26-30`).
#[derive(Debug, Clone, PartialEq)]
pub struct SseEvent {
    /// The `event:` field when present.
    pub event: Option<String>,
    /// The `data:` lines joined with newlines.
    pub data: String,
    /// The `id:` field when present.
    pub id: Option<String>,
}

/// One parse outcome, in file order: `id:` and `retry:` fields surface
/// immediately (they update the resumption cursor before the event that
/// carries them arrives).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SseItem {
    Event(SseEvent),
    Id(String),
    Retry(u64),
}

/// Incremental SSE parser (`consumeSseStream`, `streamable-http.ts:41-136`):
/// lines end with CRLF, LF, or a lone CR (including one split across
/// chunks); `:` comments are skipped; `data:` lines join with newlines.
pub(crate) struct SseParser {
    buffered: Vec<u8>,
    /// A CR ended the previous chunk, so a leading LF is its pair.
    skip_leading_lf: bool,
    event_name: Option<String>,
    event_id: Option<String>,
    data_lines: Vec<String>,
    /// Bytes of the pending event's data, including the newline joins, so
    /// events streamed as many short `data:` lines without a terminating
    /// blank line cannot grow without bound (`streamable-http.ts:48-50`).
    data_bytes: usize,
    max_event_bytes: usize,
}

impl SseParser {
    pub(crate) fn new(max_event_bytes: usize) -> Self {
        Self {
            buffered: Vec::new(),
            skip_leading_lf: false,
            event_name: None,
            event_id: None,
            data_lines: Vec::new(),
            data_bytes: 0,
            max_event_bytes,
        }
    }

    fn event_cap_error(&self) -> String {
        format!("MCP SSE event exceeds {} bytes", self.max_event_bytes)
    }

    /// Feeds one body chunk, returning the items completed by it.
    ///
    /// # Errors
    /// When a pending event's data exceeds the byte cap.
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Result<Vec<SseItem>, String> {
        if self.skip_leading_lf && chunk.first() == Some(&b'\n') {
            // A CRLF split across chunks: the CR already ended the line, so
            // drop the LF (`streamable-http.ts:113-117`).
            self.skip_leading_lf = false;
            self.buffered.extend_from_slice(&chunk[1..]);
        } else {
            self.skip_leading_lf = false;
            self.buffered.extend_from_slice(chunk);
        }
        let mut items = Vec::new();
        let len = self.buffered.len();
        let mut start = 0;
        let mut index = 0;
        while index < len {
            if matches!(self.buffered[index], b'\r' | b'\n') {
                let line = self.buffered[start..index].to_vec();
                let mut end = index + 1;
                if self.buffered[index] == b'\r' {
                    if index + 1 == len {
                        // The CR may pair with the next chunk's LF.
                        self.skip_leading_lf = true;
                    } else if self.buffered[index + 1] == b'\n' {
                        end = index + 2;
                    }
                }
                items.extend(self.process_line(&line)?);
                start = end;
                index = end;
            } else {
                index += 1;
            }
        }
        self.buffered.drain(..start);
        if self.buffered.len() > self.max_event_bytes {
            return Err(self.event_cap_error());
        }
        Ok(items)
    }

    /// Flushes the final line the stream ended without (`finish` tail,
    /// `streamable-http.ts:123-128`).
    ///
    /// # Errors
    /// When the flushed line exceeds the byte cap.
    pub(crate) fn finish(&mut self) -> Result<Vec<SseItem>, String> {
        let mut items = Vec::new();
        let rest = std::mem::take(&mut self.buffered);
        if !rest.is_empty() {
            items.extend(self.process_line(&rest)?);
        }
        items.extend(self.dispatch());
        Ok(items)
    }

    /// One field line (`processLine`, `streamable-http.ts:67-86`).
    fn process_line(&mut self, line: &[u8]) -> Result<Vec<SseItem>, String> {
        if line.is_empty() {
            return Ok(self.dispatch());
        }
        if line[0] == b':' {
            return Ok(Vec::new());
        }
        let text = String::from_utf8_lossy(line);
        let (field, value) = match text.find(':') {
            Some(colon) => {
                // One leading space after the colon is stripped
                // (`streamable-http.ts:76`).
                let raw = &text[colon + 1..];
                (&text[..colon], raw.strip_prefix(' ').unwrap_or(raw))
            }
            None => (text.as_ref(), ""),
        };
        if field == "data" {
            self.data_bytes += value.len() + usize::from(!self.data_lines.is_empty());
            if self.data_bytes > self.max_event_bytes {
                return Err(self.event_cap_error());
            }
            self.data_lines.push(value.to_string());
        } else if field == "event" {
            self.event_name = Some(value.to_string());
        } else if field == "id" && !value.contains('\0') {
            self.event_id = Some(value.to_string());
            return Ok(vec![SseItem::Id(value.to_string())]);
        } else if field == "retry"
            && !value.is_empty()
            && value.bytes().all(|byte| byte.is_ascii_digit())
            && let Ok(delay_ms) = value.parse::<u64>()
        {
            return Ok(vec![SseItem::Retry(delay_ms)]);
        }
        Ok(Vec::new())
    }

    /// Emits the pending event at a blank line (`dispatch`,
    /// `streamable-http.ts:53-65`).
    fn dispatch(&mut self) -> Vec<SseItem> {
        let event_name = self.event_name.take();
        let event_id = self.event_id.take();
        if self.data_lines.is_empty() {
            return Vec::new();
        }
        let event = SseEvent {
            event: event_name,
            data: self.data_lines.join("\n"),
            id: event_id,
        };
        self.data_lines.clear();
        self.data_bytes = 0;
        vec![SseItem::Event(event)]
    }
}

/// The body's content type without parameters, lowercased
/// (`contentType`, `streamable-http.ts:188-190`).
fn content_type(response: &reqwest::Response) -> Option<String> {
    let value = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)?
        .to_str()
        .ok()?;
    Some(
        value
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_lowercase(),
    )
}

/// `describeHttpFailure` (`streamable-http.ts:235-239`).
fn describe_http_failure(status: u16, body: &str) -> String {
    let text = body.trim();
    let snippet = if text.chars().count() > ERROR_MESSAGE_BODY_CHARS {
        let cut = text
            .char_indices()
            .nth(ERROR_MESSAGE_BODY_CHARS - 3)
            .map_or(text.len(), |(index, _)| index);
        format!("{}...", &text[..cut])
    } else {
        text.to_string()
    };
    if snippet.is_empty() {
        format!("MCP HTTP request failed with status {status}")
    } else {
        format!("MCP HTTP request failed with status {status}: {snippet}")
    }
}

/// 401, or 403 with an `insufficient_scope` bearer challenge (step-up
/// authorization) (`needsAuthorization`, `streamable-http.ts:192-197`).
/// The authorized-fetch retry consults this before handing the response to
/// the auth provider's recovery hook.
fn needs_authorization(status: u16, www_authenticate: Option<&str>) -> bool {
    if status == 401 {
        return true;
    }
    if status != 403 {
        return false;
    }
    let Some(challenge) = www_authenticate else {
        return false;
    };
    // /(?:^|[\s,])error="?insufficient_scope"?/i (`streamable-http.ts:196`)
    let lowered = challenge.to_lowercase();
    let mut from = 0;
    while let Some(found) = lowered[from..].find("error=") {
        let at = from + found;
        let preceded = at == 0
            || lowered[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_whitespace() || c == ',');
        let rest_full = &lowered[at + "error=".len()..];
        let rest = rest_full.strip_prefix('"').unwrap_or(rest_full);
        if preceded && rest.starts_with("insufficient_scope") {
            return true;
        }
        from = at + 1;
    }
    false
}

/// Statuses worth retrying when a stream fails to (re)open
/// (`isTransientStatus`, `streamable-http.ts:200-202`).
fn is_transient_status(status: u16) -> bool {
    status == 408 || status == 429 || status >= 500
}

/// Whether a failed stream consumption is worth resuming
/// (`isRetryable`, `streamable-http.ts:540-545`): network failures and
/// transient statuses are; auth, session, and protocol errors are not.
fn is_retryable(error: &McpError) -> bool {
    match error {
        McpError::Http { status, .. } => is_transient_status(*status),
        McpError::Network(_) => true,
        _ => false,
    }
}

fn network_error(error: &reqwest::Error) -> McpError {
    // `fetch` reports network failures, including a connection dropped
    // mid-body, as `TypeError` (`streamable-http.ts:536-539`).
    McpError::Network(error.to_string())
}

/// `initial * 2 ** attempt`, capped, with a server-sent `retry` field
/// overriding (`reconnectDelay`, `streamable-http.ts:547-551`).
fn backoff_delay(initial_ms: u64, max_ms: u64, attempt: u32, server_delay_ms: Option<u64>) -> u64 {
    if let Some(server_delay_ms) = server_delay_ms {
        return server_delay_ms;
    }
    let factor = 1u64 << u32::min(attempt, 62);
    initial_ms.saturating_mul(factor).min(max_ms)
}

/// Credential source for the HTTP transport: [`AuthProvider`]
/// (`packages/mcp/src/auth-provider.ts:6-24`), re-exported through
/// [`crate::auth_provider`]. The OAuth provider implements it via
/// [`crate::oauth::auth_provider::McpAuthProvider`].
//
// Construction options (`StreamableHttpTransportOptions`,
/// `streamable-http.ts:147-157`).
#[derive(Clone)]
pub struct StreamableHttpOptions {
    /// The server endpoint.
    pub url: String,
    /// Headers sent with every request (`options.headers`).
    pub headers: HashMap<String, String>,
    /// Credential source; a token rides `Authorization: Bearer …`.
    pub auth: Option<Arc<dyn AuthProvider>>,
    /// Open the server-to-client GET stream after initialization
    /// (default true).
    pub open_get_stream: bool,
    /// Byte cap per SSE event and JSON response.
    pub max_message_bytes: usize,
    /// Stream reconnection backoff (`StreamableHttpReconnectOptions`,
    /// `streamable-http.ts:138-145`).
    pub reconnect_initial_delay_ms: u64,
    pub reconnect_max_delay_ms: u64,
    pub reconnect_max_retries: u32,
}

impl Default for StreamableHttpOptions {
    fn default() -> Self {
        Self {
            url: String::new(),
            headers: HashMap::new(),
            auth: None,
            open_get_stream: true,
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
            reconnect_initial_delay_ms: DEFAULT_RECONNECT_INITIAL_DELAY_MS,
            reconnect_max_delay_ms: DEFAULT_RECONNECT_MAX_DELAY_MS,
            reconnect_max_retries: DEFAULT_RECONNECT_MAX_RETRIES,
        }
    }
}

/// Mutable transport state, shared with the background stream tasks.
struct HttpState {
    started: bool,
    closed: bool,
    session_id: Option<String>,
    protocol_version: Option<String>,
    /// Access token of the latest request, which `close()` reuses instead
    /// of asking the auth provider (`streamable-http.ts:258-259`).
    last_token: Option<String>,
    get_stream_started: bool,
}

/// The state the background stream tasks share with the transport.
struct Shared {
    url: String,
    /// Config headers, parsed once (`options.headers`).
    headers: Vec<(HeaderName, HeaderValue)>,
    client: reqwest::Client,
    auth: Option<Arc<dyn AuthProvider>>,
    /// Server-initiated requests and notifications.
    inbound: mpsc::Sender<InboundMessage>,
    state: std::sync::Mutex<HttpState>,
    /// Set by `close()`; aborts the stream tasks' waits.
    closed: watch::Sender<bool>,
    max_message_bytes: usize,
    open_get_stream: bool,
    reconnect_initial_delay_ms: u64,
    reconnect_max_delay_ms: u64,
    reconnect_max_retries: u32,
}

impl Shared {
    /// Fetch with auth headers. A 401 (or a 403 asking for more scope) is
    /// handed to the auth provider once, and the request is retried with
    /// whatever credentials it left behind (`authorizedFetch`,
    /// `streamable-http.ts:352-374`). A provider without a recovery hook
    /// (`recovers()`) never sees the response; provider errors bubble in
    /// place of it.
    async fn fetch_with_auth(
        &self,
        method: Method,
        extra: &[(&'static str, String)],
        body: Option<Vec<u8>>,
    ) -> Result<reqwest::Response, McpError> {
        let mut attempt = 0;
        loop {
            let (session_id, protocol_version) = {
                let state = self.state.lock().expect("mcp http state mutex poisoned");
                (state.session_id.clone(), state.protocol_version.clone())
            };
            // `token()` may refresh over the network, so it is awaited per
            // attempt (`headers`, `streamable-http.ts:377-383`).
            let token = match self.auth.as_ref() {
                Some(provider) => provider.token().await?,
                None => None,
            };
            {
                let mut state = self.state.lock().expect("mcp http state mutex poisoned");
                state.last_token.clone_from(&token);
            }
            let mut request =
                self.client
                    .request(method.clone(), &self.url)
                    .headers(self.build_headers(
                        extra,
                        session_id.as_deref(),
                        protocol_version.as_deref(),
                        token.as_deref(),
                    ));
            if let Some(body) = &body {
                request = request.body(body.clone());
            }
            let response = request
                .send()
                .await
                .map_err(|error| network_error(&error))?;
            if attempt > 0 {
                return Ok(response);
            }
            let Some(provider) = self.auth.as_ref().filter(|provider| provider.recovers()) else {
                return Ok(response);
            };
            let challenged = needs_authorization(
                response.status().as_u16(),
                response
                    .headers()
                    .get("www-authenticate")
                    .and_then(|value| value.to_str().ok()),
            );
            if !challenged {
                return Ok(response);
            }
            let context = UnauthorizedContext {
                response,
                server_url: self.url.clone(),
                fetch: McpFetch::with_client(self.client.clone()),
                token: token.clone(),
            };
            // One recovery shot: whatever it leaves behind (or throws)
            // decides the retry (`authorizedFetch`, `streamable-http.ts:366-373`).
            provider.on_unauthorized(context).await?;
            attempt += 1;
        }
    }

    /// Request headers (`buildHeaders`, `streamable-http.ts:382-389`): the
    /// config headers first, the per-request ones overriding, then the
    /// session, the negotiated protocol version, and the bearer token.
    fn build_headers(
        &self,
        extra: &[(&'static str, String)],
        session_id: Option<&str>,
        protocol_version: Option<&str>,
        token: Option<&str>,
    ) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in &self.headers {
            map.insert(name.clone(), value.clone());
        }
        for (name, value) in extra {
            if let Ok(parsed) = HeaderValue::from_str(value) {
                map.insert(HeaderName::from_static(name), parsed);
            }
        }
        if let Some(value) = session_id
            .filter(|id| !id.is_empty())
            .and_then(|id| HeaderValue::from_str(id).ok())
        {
            map.insert(HeaderName::from_static("mcp-session-id"), value);
        }
        if let Some(value) =
            protocol_version.and_then(|version| HeaderValue::from_str(version).ok())
        {
            map.insert(HeaderName::from_static("mcp-protocol-version"), value);
        }
        if let Some(value) = token
            .filter(|token| !token.is_empty())
            .and_then(|token| HeaderValue::from_str(&format!("Bearer {token}")).ok())
        {
            map.insert(reqwest::header::AUTHORIZATION, value);
        }
        map
    }

    /// Captures the session id the server assigned (`captureSession`,
    /// `streamable-http.ts:391-394`).
    fn capture_session(&self, response: &reqwest::Response) {
        let session_id = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
            .filter(|id| !id.is_empty());
        if let Some(session_id) = session_id {
            self.state
                .lock()
                .expect("mcp http state mutex poisoned")
                .session_id = Some(session_id.to_string());
        }
    }

    /// Status failure check (`checkResponse`, `streamable-http.ts:396-403`):
    /// reads only the excerpt the error keeps, so a huge error body is
    /// never buffered. The response returns for the success path.
    ///
    /// # Errors
    /// [`McpError::AuthRequired`] on 401, [`McpError::SessionExpired`] on
    /// a 404 after a session id was captured, [`McpError::Http`] otherwise.
    async fn check_response(
        &self,
        response: reqwest::Response,
    ) -> Result<reqwest::Response, McpError> {
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status().as_u16();
        let www_authenticate = response
            .headers()
            .get("www-authenticate")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let body = read_body_bytes(response, MAX_ERROR_BODY_BYTES)
            .await
            .map_or_else(|_| Vec::new(), |(bytes, _)| bytes);
        let body = String::from_utf8_lossy(&body).into_owned();
        if status == 401 {
            return Err(McpError::AuthRequired {
                www_authenticate,
                body,
            });
        }
        let has_session = self
            .state
            .lock()
            .expect("mcp http state mutex poisoned")
            .session_id
            .is_some();
        if status == 404 && has_session {
            return Err(McpError::SessionExpired { body });
        }
        Err(McpError::Http {
            status,
            message: describe_http_failure(status, &body),
            body,
        })
    }
}

/// Live streamable HTTP connection to one server.
pub struct StreamableHttpTransport {
    shared: Arc<Shared>,
    next_id: AtomicI64,
    get_stream_task: std::sync::Mutex<Option<JoinHandle<()>>>,
    /// Server-initiated requests and notifications (SSE events on the GET
    /// stream and on response streams), like the stdio transport's
    /// diagnostics channel.
    pub inbound: mpsc::Receiver<InboundMessage>,
}

impl StreamableHttpTransport {
    /// Builds the transport (`StreamableHttpTransport` constructor,
    /// `streamable-http.ts:261-269`).
    ///
    /// # Errors
    /// On a non-http(s) URL, an invalid config header, or an HTTP client
    /// that cannot start.
    pub fn new(options: StreamableHttpOptions) -> Result<Self, McpError> {
        if !options.url.starts_with("http://") && !options.url.starts_with("https://") {
            return Err(McpError::Malformed(format!(
                "MCP server url must be an http or https URL: {:?}",
                options.url
            )));
        }
        let mut headers = Vec::new();
        for (key, value) in &options.headers {
            let name = HeaderName::from_bytes(key.as_bytes())
                .map_err(|_| McpError::Malformed(format!("invalid MCP header name {key:?}")))?;
            let parsed = HeaderValue::from_str(value).map_err(|_| {
                McpError::Malformed(format!("invalid MCP header value for {key:?}"))
            })?;
            headers.push((name, parsed));
        }
        let client = reqwest::Client::builder().build().map_err(|error| {
            McpError::Network(format!("failed to build the HTTP client: {error}"))
        })?;
        let (closed, _) = watch::channel(false);
        let (inbound, receiver) = mpsc::channel(256);
        Ok(Self {
            shared: Arc::new(Shared {
                url: options.url,
                headers,
                client,
                auth: options.auth,
                inbound,
                state: std::sync::Mutex::new(HttpState {
                    started: false,
                    closed: false,
                    session_id: None,
                    protocol_version: None,
                    last_token: None,
                    get_stream_started: false,
                }),
                closed,
                max_message_bytes: options.max_message_bytes,
                open_get_stream: options.open_get_stream,
                reconnect_initial_delay_ms: options.reconnect_initial_delay_ms,
                reconnect_max_delay_ms: options.reconnect_max_delay_ms,
                reconnect_max_retries: options.reconnect_max_retries,
            }),
            next_id: AtomicI64::new(0),
            get_stream_task: std::sync::Mutex::new(None),
            inbound: receiver,
        })
    }

    /// Marks the transport open (`start`, `streamable-http.ts:275-279`).
    ///
    /// # Errors
    /// When the transport already started or closed.
    pub fn start(&self) -> Result<(), McpError> {
        let mut state = self
            .shared
            .state
            .lock()
            .expect("mcp http state mutex poisoned");
        if state.started {
            return Err(McpError::Malformed(
                "MCP Streamable HTTP transport already started".to_string(),
            ));
        }
        if state.closed {
            return Err(McpError::ConnectionClosed(
                "MCP connection closed".to_string(),
            ));
        }
        state.started = true;
        Ok(())
    }

    /// Carries the negotiated protocol version in
    /// `MCP-Protocol-Version` (`setProtocolVersion`,
    /// `streamable-http.ts:281-283`).
    pub fn set_protocol_version(&self, version: &str) {
        self.shared
            .state
            .lock()
            .expect("mcp http state mutex poisoned")
            .protocol_version = Some(version.to_string());
    }

    /// The session id the server assigned, when any.
    #[must_use]
    pub fn session_id(&self) -> Option<String> {
        self.shared
            .state
            .lock()
            .expect("mcp http state mutex poisoned")
            .session_id
            .clone()
    }

    fn is_closed(&self) -> bool {
        self.shared
            .state
            .lock()
            .expect("mcp http state mutex poisoned")
            .closed
    }

    fn assert_open(&self) -> Result<(), McpError> {
        let state = self
            .shared
            .state
            .lock()
            .expect("mcp http state mutex poisoned");
        if !state.started || state.closed {
            return Err(McpError::ConnectionClosed(
                "MCP connection closed".to_string(),
            ));
        }
        Ok(())
    }

    fn allocate_id(&self) -> i64 {
        self.next_id.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn reconnect_delay(&self, attempt: u32, server_delay_ms: Option<u64>) -> u64 {
        backoff_delay(
            self.shared.reconnect_initial_delay_ms,
            self.shared.reconnect_max_delay_ms,
            attempt,
            server_delay_ms,
        )
    }

    /// Resolves false when the transport closed while waiting (`sleep`,
    /// `streamable-http.ts:561-578`).
    async fn sleep_closed(&self, ms: u64) -> bool {
        let mut closed = self.shared.closed.subscribe();
        sleep_closed(&mut closed, ms).await
    }

    /// Sends a request and waits for its response.
    ///
    /// # Errors
    /// On HTTP failures, malformed replies, RPC errors, or timeout.
    pub async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, McpError> {
        match tokio::time::timeout(timeout, self.request_inner(method, params)).await {
            Ok(outcome) => outcome,
            Err(_) => Err(McpError::Timeout {
                method: method.to_string(),
                timeout_secs: timeout.as_secs(),
            }),
        }
    }

    /// The `send` request arm (`streamable-http.ts:285-323`).
    async fn request_inner(&self, method: &str, params: Option<Value>) -> Result<Value, McpError> {
        self.assert_open()?;
        let id = self.allocate_id();
        let request = JsonRpcRequest {
            jsonrpc: JsonRpcVersion,
            id,
            method: method.to_string(),
            params,
        };
        let body = serde_json::to_vec(&request).map_err(|error| {
            McpError::Malformed(format!("request serialization failed: {error}"))
        })?;
        let response = self
            .shared
            .fetch_with_auth(
                Method::POST,
                &[
                    ("accept", POST_ACCEPT.to_string()),
                    ("content-type", "application/json".to_string()),
                ],
                Some(body),
            )
            .await?;
        let response = self.shared.check_response(response).await?;
        self.shared.capture_session(&response);
        // A request answered with 202/204 will never get a reply
        // (`streamable-http.ts:305-307`; notifications discard the body
        // instead, in [`StreamableHttpTransport::notify`]).
        let status = response.status().as_u16();
        if status == 202 || status == 204 {
            return Err(McpError::Http {
                status,
                message: format!("MCP server accepted request {method} without a response"),
                body: String::new(),
            });
        }
        match content_type(&response).as_deref() {
            Some("application/json") => {
                let (bytes, truncated) =
                    read_body_bytes(response, self.shared.max_message_bytes).await?;
                if truncated {
                    return Err(McpError::Malformed(format!(
                        "MCP JSON response exceeds {} bytes",
                        self.shared.max_message_bytes
                    )));
                }
                let parsed: Value = serde_json::from_slice(&bytes).map_err(|error| {
                    McpError::Malformed(format!("invalid JSON in MCP response: {error}"))
                })?;
                let items = match parsed {
                    Value::Array(items) => items,
                    single => vec![single],
                };
                for item in &items {
                    if let Some(reply) = self.route_json_item(item, id)? {
                        return if let Some(error) = reply.error {
                            Err(McpError::Rpc {
                                code: error.code,
                                message: error.message,
                            })
                        } else {
                            reply.result.ok_or_else(|| {
                                McpError::Malformed(format!(
                                    "response {id} carried neither result nor error"
                                ))
                            })
                        };
                    }
                }
                Err(McpError::Malformed(format!(
                    "MCP response carried no reply to request {id}"
                )))
            }
            Some("text/event-stream") => self.consume_response_stream(response, id).await,
            other => Err(McpError::Http {
                status: response.status().as_u16(),
                message: format!(
                    "Unsupported MCP response content type: {}",
                    other.unwrap_or("missing")
                ),
                body: String::new(),
            }),
        }
    }

    /// Routes one parsed message of a JSON body (`emitMessage` +
    /// `parseJsonRpcMessage`, `streamable-http.ts:309-315`): the awaited
    /// request's reply is returned; requests and notifications go to the
    /// inbound channel; unknown replies are dropped like the stdio pending
    /// map drops unmatched responses. An invalid message fails the request
    /// with `-32600` here, where an SSE event only reaches diagnostics.
    fn route_json_item(
        &self,
        item: &Value,
        request_id: i64,
    ) -> Result<Option<JsonRpcResponse>, McpError> {
        let raw = serde_json::to_string(item).unwrap_or_default();
        match classify_value(item, &raw) {
            InboundMessage::Response(reply) if reply.id == request_id => Ok(Some(reply)),
            InboundMessage::Response(_) => Ok(None),
            InboundMessage::Request(request) => {
                let _ = self
                    .shared
                    .inbound
                    .try_send(InboundMessage::Request(request));
                Ok(None)
            }
            InboundMessage::Notification(notification) => {
                let _ = self
                    .shared
                    .inbound
                    .try_send(InboundMessage::Notification(notification));
                Ok(None)
            }
            InboundMessage::Malformed(_) => Err(McpError::Rpc {
                code: json_rpc_error_codes::INVALID_REQUEST,
                message: "Invalid JSON-RPC message".to_string(),
            }),
        }
    }

    /// Sends a notification (no reply expected).
    ///
    /// # Errors
    /// On HTTP failures; `notifications/initialized` opens the
    /// server-to-client stream afterwards.
    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), McpError> {
        self.assert_open()?;
        let notification = JsonRpcNotification {
            jsonrpc: JsonRpcVersion,
            method: method.to_string(),
            params,
        };
        let body = serde_json::to_vec(&notification).map_err(|error| {
            McpError::Malformed(format!("request serialization failed: {error}"))
        })?;
        let response = self
            .shared
            .fetch_with_auth(
                Method::POST,
                &[
                    ("accept", POST_ACCEPT.to_string()),
                    ("content-type", "application/json".to_string()),
                ],
                Some(body),
            )
            .await?;
        let response = self.shared.check_response(response).await?;
        self.shared.capture_session(&response);
        // Notifications are acknowledged with 202 and carry no reply; the
        // server-to-client stream may only open once the session is
        // initialized (`streamable-http.ts:298-303`).
        drop(response);
        if method == "notifications/initialized" {
            self.start_get_stream();
        }
        Ok(())
    }

    /// Opens the server-to-client stream once
    /// (`startGetStream`, `streamable-http.ts:480-484`).
    fn start_get_stream(&self) {
        let mut state = self
            .shared
            .state
            .lock()
            .expect("mcp http state mutex poisoned");
        if !self.shared.open_get_stream || state.get_stream_started || state.closed {
            return;
        }
        state.get_stream_started = true;
        drop(state);
        let shared = Arc::clone(&self.shared);
        self.get_stream_task
            .lock()
            .expect("mcp http get-stream mutex poisoned")
            .replace(tokio::spawn(run_get_stream(shared)));
    }

    /// Read the SSE stream answering one request. When the stream ends or
    /// breaks before the response arrives and the server assigned event
    /// ids, resume it with GET and `Last-Event-ID`, as the server may
    /// close response streams at will (`consumeResponseStream`,
    /// `streamable-http.ts:440-478`). Otherwise only this request fails.
    async fn consume_response_stream(
        &self,
        body: reqwest::Response,
        request_id: i64,
    ) -> Result<Value, McpError> {
        let mut cursor = StreamCursor::default();
        let mut failure: Option<McpError> = None;
        // `None` is the "no stream open" state between resumption attempts.
        let mut body: Option<reqwest::Response> = Some(body);
        let mut attempt: u32 = 0;
        loop {
            if let Some(response) = body.take() {
                match self
                    .consume_sse(
                        response,
                        SseParser::new(self.shared.max_message_bytes),
                        &mut cursor,
                        Some(request_id),
                    )
                    .await
                {
                    Ok(Some(reply)) => return reply_to_result(reply),
                    Ok(None) => failure = None,
                    Err(error) => failure = Some(error),
                }
            }
            if self.is_closed() {
                return Err(McpError::ConnectionClosed(
                    "MCP connection closed".to_string(),
                ));
            }
            if let Some(error) = &failure
                && !is_retryable(error)
            {
                break;
            }
            if cursor.last_event_id.is_none() || attempt >= self.shared.reconnect_max_retries {
                break;
            }
            if cursor.received {
                attempt = 0;
            }
            cursor.received = false;
            let delay = self.reconnect_delay(attempt, cursor.retry_ms);
            attempt += 1;
            if !self.sleep_closed(delay).await {
                return Err(McpError::ConnectionClosed(
                    "MCP connection closed".to_string(),
                ));
            }
            match open_sse_stream(&self.shared, cursor.last_event_id.as_deref()).await {
                Ok(Some(response)) => body = Some(response),
                Ok(None) => {}
                Err(error) => {
                    let retryable = is_retryable(&error);
                    failure = Some(error);
                    if !retryable {
                        break;
                    }
                }
            }
        }
        if self.is_closed() {
            return Err(McpError::ConnectionClosed(
                "MCP connection closed".to_string(),
            ));
        }
        let reason = failure.map_or_else(
            || "stream ended without a response".to_string(),
            |error| error.to_string(),
        );
        Err(McpError::Rpc {
            code: json_rpc_error_codes::INTERNAL_ERROR,
            message: format!("MCP response stream failed: {reason}"),
        })
    }

    /// Reads one SSE body until it ends, fails, or — with `request_id` —
    /// the reply arrives (`consumeSse`, `streamable-http.ts:405-433`). The
    /// parser and body move into a background task when the reply arrives,
    /// so the rest of the stream still carries notifications.
    ///
    /// # Errors
    /// On network failures or an oversized event.
    async fn consume_sse(
        &self,
        body: reqwest::Response,
        mut parser: SseParser,
        cursor: &mut StreamCursor,
        request_id: Option<i64>,
    ) -> Result<Option<JsonRpcResponse>, McpError> {
        let mut stream = Box::pin(body.bytes_stream());
        while let Some(chunk) = stream.as_mut().next().await {
            if self.is_closed() {
                return Err(McpError::ConnectionClosed(
                    "MCP connection closed".to_string(),
                ));
            }
            let chunk = chunk.map_err(|error| network_error(&error))?;
            let mut items = parser
                .feed(&chunk)
                .map_err(McpError::Malformed)?
                .into_iter();
            while let Some(item) = items.next() {
                if let Some(reply) = self.handle_sse_item(item, cursor, request_id) {
                    // The rest of the stream still carries notifications;
                    // upstream keeps the same stream running after the
                    // answer (`streamable-http.ts:317-319`). Items already
                    // parsed out of this chunk reach the inbound channel
                    // here; the spawned task only owns future chunks.
                    for item in items {
                        self.handle_sse_item(item, cursor, request_id);
                    }
                    let shared = Arc::clone(&self.shared);
                    tokio::spawn(consume_stream_rest(shared, stream, parser));
                    return Ok(Some(reply));
                }
            }
        }
        let mut items = parser.finish().map_err(McpError::Malformed)?.into_iter();
        while let Some(item) = items.next() {
            if let Some(reply) = self.handle_sse_item(item, cursor, request_id) {
                for item in items {
                    self.handle_sse_item(item, cursor, request_id);
                }
                let shared = Arc::clone(&self.shared);
                tokio::spawn(consume_stream_rest(shared, stream, parser));
                return Ok(Some(reply));
            }
        }
        Ok(None)
    }

    /// One SSE item (`consumeSse`'s callbacks, `streamable-http.ts:411-431`).
    /// `Some` is the awaited reply; events without data prime resumption,
    /// other event types are not JSON-RPC, and a malformed event only
    /// reaches the error listeners upstream, which the port surfaces as an
    /// inbound diagnostic.
    fn handle_sse_item(
        &self,
        item: SseItem,
        cursor: &mut StreamCursor,
        request_id: Option<i64>,
    ) -> Option<JsonRpcResponse> {
        match item {
            SseItem::Id(id) => cursor.last_event_id = Some(id),
            SseItem::Retry(delay_ms) => cursor.retry_ms = Some(delay_ms),
            SseItem::Event(event) => {
                cursor.received = true;
                if event.data.trim().is_empty()
                    || event.event.as_deref().is_some_and(|name| name != "message")
                {
                    return None;
                }
                let Ok(parsed) = serde_json::from_str::<Value>(&event.data) else {
                    let _ = self
                        .shared
                        .inbound
                        .try_send(InboundMessage::Malformed(event.data));
                    return None;
                };
                match classify_value(&parsed, &event.data) {
                    InboundMessage::Response(reply) if request_id == Some(reply.id) => {
                        return Some(reply);
                    }
                    InboundMessage::Response(_) => {}
                    InboundMessage::Request(request) => {
                        let _ = self
                            .shared
                            .inbound
                            .try_send(InboundMessage::Request(request));
                    }
                    InboundMessage::Notification(notification) => {
                        let _ = self
                            .shared
                            .inbound
                            .try_send(InboundMessage::Notification(notification));
                    }
                    InboundMessage::Malformed(_) => {
                        let _ = self
                            .shared
                            .inbound
                            .try_send(InboundMessage::Malformed(event.data));
                    }
                }
            }
        }
        None
    }

    /// Tears the transport down: the stream tasks stop, and the session's
    /// DELETE goes out with the last request's token
    /// (`close`, `streamable-http.ts:325-342`).
    ///
    /// # Errors
    /// Never (the DELETE is best effort: the session expires on the server
    /// anyway, and the auth provider is not asked).
    pub async fn shutdown(self) -> Result<(), McpError> {
        {
            let mut state = self
                .shared
                .state
                .lock()
                .expect("mcp http state mutex poisoned");
            if state.closed {
                return Ok(());
            }
            state.closed = true;
        }
        let _ = self.shared.closed.send(true);
        if let Some(task) = self
            .get_stream_task
            .lock()
            .expect("mcp http get-stream mutex poisoned")
            .take()
        {
            task.abort();
        }
        let (started, session_id, protocol_version, last_token) = {
            let state = self
                .shared
                .state
                .lock()
                .expect("mcp http state mutex poisoned");
            (
                state.started,
                state.session_id.clone(),
                state.protocol_version.clone(),
                state.last_token.clone(),
            )
        };
        if started && let Some(session_id) = session_id {
            let headers = self.shared.build_headers(
                &[],
                Some(&session_id),
                protocol_version.as_deref(),
                last_token.as_deref(),
            );
            let _ = tokio::time::timeout(
                CLOSE_TIMEOUT,
                self.shared
                    .client
                    .delete(&self.shared.url)
                    .headers(headers)
                    .send(),
            )
            .await;
        }
        Ok(())
    }
}

/// The awaited reply as the client sees it (`handleResponse`,
/// upstream `client.ts:481-490`).
fn reply_to_result(reply: JsonRpcResponse) -> Result<Value, McpError> {
    if let Some(error) = reply.error {
        return Err(McpError::Rpc {
            code: error.code,
            message: error.message,
        });
    }
    reply.result.ok_or_else(|| {
        McpError::Malformed(format!(
            "response {} carried neither result nor error",
            reply.id
        ))
    })
}

/// Cursor of one SSE stream's resumption state (`StreamCursor`,
/// `streamable-http.ts:241-246`).
#[derive(Default)]
struct StreamCursor {
    last_event_id: Option<String>,
    retry_ms: Option<u64>,
    /// Whether the stream delivered any event since it was (re)opened.
    received: bool,
}

/// `readBodyText` (`streamable-http.ts:212-233`): the first `max_bytes`
/// body bytes, and whether more arrived (the rest is cancelled).
async fn read_body_bytes(
    response: reqwest::Response,
    max_bytes: usize,
) -> Result<(Vec<u8>, bool), McpError> {
    let mut buffer = Vec::new();
    let mut truncated = false;
    let mut stream = Box::pin(response.bytes_stream());
    while let Some(chunk) = stream.as_mut().next().await {
        let chunk = chunk.map_err(|error| network_error(&error))?;
        if buffer.len() + chunk.len() > max_bytes {
            let keep = max_bytes.saturating_sub(buffer.len());
            buffer.extend_from_slice(&chunk[..keep]);
            truncated = true;
            break;
        }
        buffer.extend_from_slice(&chunk);
    }
    Ok((buffer, truncated))
}

/// Opens a GET SSE stream; `Ok(None)` when the server answers 405 — it does
/// not offer one (`openSseStream`, `streamable-http.ts:515-534`).
///
/// # Errors
/// On HTTP failures or a non-SSE response type.
async fn open_sse_stream(
    shared: &Shared,
    last_event_id: Option<&str>,
) -> Result<Option<reqwest::Response>, McpError> {
    let mut extra: Vec<(&'static str, String)> = vec![("accept", SSE_ACCEPT.to_string())];
    if let Some(last_event_id) = last_event_id {
        extra.push(("last-event-id", last_event_id.to_string()));
    }
    let response = shared.fetch_with_auth(Method::GET, &extra, None).await?;
    if response.status().as_u16() == 405 {
        return Ok(None);
    }
    let response = shared.check_response(response).await?;
    shared.capture_session(&response);
    if content_type(&response).as_deref() != Some("text/event-stream") {
        let status = response.status().as_u16();
        let kind = content_type(&response).unwrap_or_else(|| "missing".to_string());
        return Err(McpError::Http {
            status,
            message: format!("Unsupported MCP GET response content type: {kind}"),
            body: String::new(),
        });
    }
    Ok(Some(response))
}

/// Routes one SSE item of a host-side stream (`consumeSse`'s callbacks,
/// `streamable-http.ts:411-431`): ids and retries update the cursor,
/// message events reach the inbound channel, other events are not
/// JSON-RPC, and malformed events only reach diagnostics.
fn route_stream_item(shared: &Shared, item: SseItem, cursor: &mut StreamCursor) {
    match item {
        SseItem::Id(id) => cursor.last_event_id = Some(id),
        SseItem::Retry(delay_ms) => cursor.retry_ms = Some(delay_ms),
        SseItem::Event(event) => {
            cursor.received = true;
            if event.data.trim().is_empty()
                || event.event.as_deref().is_some_and(|name| name != "message")
            {
                return;
            }
            let Ok(parsed) = serde_json::from_str::<Value>(&event.data) else {
                let _ = shared
                    .inbound
                    .try_send(InboundMessage::Malformed(event.data));
                return;
            };
            match classify_value(&parsed, &event.data) {
                InboundMessage::Request(request) => {
                    let _ = shared.inbound.try_send(InboundMessage::Request(request));
                }
                InboundMessage::Notification(notification) => {
                    let _ = shared
                        .inbound
                        .try_send(InboundMessage::Notification(notification));
                }
                _ => {}
            }
        }
    }
}

/// Consumes one host-side SSE body to its end (`consumeSse` without a
/// request id).
///
/// # Errors
/// On network failures or an oversized event.
async fn consume_host_stream(
    shared: &Shared,
    body: reqwest::Response,
    cursor: &mut StreamCursor,
) -> Result<(), McpError> {
    let mut parser = SseParser::new(shared.max_message_bytes);
    let mut stream = Box::pin(body.bytes_stream());
    while let Some(chunk) = stream.as_mut().next().await {
        let chunk = chunk.map_err(|error| network_error(&error))?;
        for item in parser.feed(&chunk).map_err(McpError::Malformed)? {
            route_stream_item(shared, item, cursor);
        }
    }
    for item in parser.finish().map_err(McpError::Malformed)? {
        route_stream_item(shared, item, cursor);
    }
    Ok(())
}

/// A server diagnostic reaching the inbound channel, where upstream's
/// `emitError` reaches the client's error listeners.
fn surface_diagnostic(shared: &Shared, message: String) {
    let _ = shared.inbound.try_send(InboundMessage::Malformed(message));
}

/// Keeps the server-to-client stream open, reconnecting with backoff when
/// it drops (`runGetStream`, `streamable-http.ts:487-512`).
#[allow(clippy::too_many_lines)] // mirrors the TS runGetStream; split when extending
async fn run_get_stream(shared: Arc<Shared>) {
    let mut closed = shared.closed.subscribe();
    let mut cursor = StreamCursor::default();
    let mut attempt: u32 = 0;
    while !*closed.borrow() {
        match open_sse_stream(&shared, cursor.last_event_id.as_deref()).await {
            Ok(Some(response)) => {
                let opened_at = Instant::now();
                match consume_host_stream(&shared, response, &mut cursor).await {
                    Ok(()) => {
                        // A stream that stayed up for a while counts as
                        // healthy, even if it was idle
                        // (`streamable-http.ts:496-497`).
                        if cursor.received
                            || opened_at.elapsed()
                                > Duration::from_millis(shared.reconnect_max_delay_ms)
                        {
                            attempt = 0;
                        }
                    }
                    Err(error) => {
                        if *closed.borrow() {
                            return;
                        }
                        if !is_retryable(&error) {
                            surface_diagnostic(&shared, error.to_string());
                            return;
                        }
                    }
                }
            }
            // The server does not offer a GET stream
            // (`streamable-http.ts:492-493`).
            Ok(None) => return,
            Err(error) => {
                if *closed.borrow() {
                    return;
                }
                if !is_retryable(&error) {
                    surface_diagnostic(&shared, error.to_string());
                    return;
                }
            }
        }
        cursor.received = false;
        if attempt >= shared.reconnect_max_retries {
            surface_diagnostic(
                &shared,
                "MCP server-to-client stream dropped and could not be reopened".to_string(),
            );
            return;
        }
        let delay_ms = backoff_delay(
            shared.reconnect_initial_delay_ms,
            shared.reconnect_max_delay_ms,
            attempt,
            cursor.retry_ms,
        );
        attempt += 1;
        if !sleep_closed(&mut closed, delay_ms).await {
            return;
        }
    }
}

/// Resolves false when the transport closed while waiting (`sleep`,
/// `streamable-http.ts:561-578`).
async fn sleep_closed(closed: &mut watch::Receiver<bool>, ms: u64) -> bool {
    if *closed.borrow() {
        return false;
    }
    tokio::select! {
        () = tokio::time::sleep(Duration::from_millis(ms)) => true,
        _ = closed.changed() => false,
    }
}

/// The rest of an answered response stream: it keeps emitting
/// notifications until the server closes it — upstream's first
/// `consumeSse` keeps running after the answer
/// (`streamable-http.ts:317-319` with `void`). Its resumption cursor is
/// bookkeeping only: no stream follows the answer.
async fn consume_stream_rest(
    shared: Arc<Shared>,
    mut stream: BoxStream<'static, reqwest::Result<Bytes>>,
    mut parser: SseParser,
) {
    let mut closed = shared.closed.subscribe();
    let mut cursor = StreamCursor::default();
    loop {
        tokio::select! {
            _ = closed.changed() => return,
            chunk = stream.next() => if let Some(Ok(chunk)) = chunk {
                let Ok(items) = parser.feed(&chunk) else {
                    return;
                };
                for item in items {
                    route_stream_item(&shared, item, &mut cursor);
                }
            } else {
                if let Ok(items) = parser.finish() {
                    for item in items {
                        route_stream_item(&shared, item, &mut cursor);
                    }
                }
                return;
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::future::Future;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::pin::Pin;
    use std::sync::Mutex as StdMutex;

    use serde_json::json;

    use super::*;

    fn parse_sse_items(chunks: &[&[u8]]) -> Vec<SseItem> {
        let mut parser = SseParser::new(DEFAULT_MAX_MESSAGE_BYTES);
        let mut items = Vec::new();
        for chunk in chunks {
            items.extend(parser.feed(chunk).expect("feed"));
        }
        items.extend(parser.finish().expect("finish"));
        items
    }

    fn event_items(chunks: &[&[u8]]) -> Vec<SseEvent> {
        parse_sse_items(chunks)
            .into_iter()
            .filter_map(|item| match item {
                SseItem::Event(event) => Some(event),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn sse_parses_lf_crlf_and_cr_line_endings() {
        let events = event_items(&[b"data: a\n\ndata: b\r\rdata: c\r\n\ndata: d\n\n"]);
        assert_eq!(
            events,
            vec![
                SseEvent {
                    event: None,
                    data: "a".to_string(),
                    id: None
                },
                SseEvent {
                    event: None,
                    data: "b".to_string(),
                    id: None
                },
                SseEvent {
                    event: None,
                    data: "c".to_string(),
                    id: None
                },
                SseEvent {
                    event: None,
                    data: "d".to_string(),
                    id: None
                },
            ]
        );
    }

    #[test]
    fn sse_joins_data_lines_and_carries_event_and_id() {
        let items = parse_sse_items(&[b"event: message\nid: 42\ndata: one\ndata: two\n\n"]);
        assert_eq!(
            items,
            vec![
                SseItem::Id("42".to_string()),
                SseItem::Event(SseEvent {
                    event: Some("message".to_string()),
                    data: "one\ntwo".to_string(),
                    id: Some("42".to_string()),
                }),
            ]
        );
    }

    #[test]
    fn sse_skips_comments_and_strips_one_leading_space() {
        let events = event_items(&[b": heartbeat\ndata:  spaced\n\n"]);
        assert_eq!(
            events,
            vec![SseEvent {
                event: None,
                data: " spaced".to_string(),
                id: None
            }]
        );
    }

    #[test]
    fn sse_handles_chunks_split_mid_line_and_mid_crlf() {
        // A data line split across three chunks, then a CRLF whose CR and
        // LF land in different chunks (the blank line dispatches the first
        // event; data lines only join until one).
        let events = event_items(&[b"da", b"ta: hel", b"lo\r", b"\n\ndata: x\n\n"]);
        assert_eq!(
            events,
            vec![
                SseEvent {
                    event: None,
                    data: "hello".to_string(),
                    id: None
                },
                SseEvent {
                    event: None,
                    data: "x".to_string(),
                    id: None
                },
            ]
        );
    }

    #[test]
    fn sse_records_retry_fields() {
        let items = parse_sse_items(&[b"retry: 2500\nretry: nope\ndata: x\n\n"]);
        assert_eq!(
            items,
            vec![
                SseItem::Retry(2500),
                SseItem::Event(SseEvent {
                    event: None,
                    data: "x".to_string(),
                    id: None
                }),
            ]
        );
    }

    #[test]
    fn sse_ignores_ids_with_nul() {
        let items = parse_sse_items(&[b"id: bad\0id\ndata: x\n\n"]);
        assert_eq!(
            items,
            vec![SseItem::Event(SseEvent {
                event: None,
                data: "x".to_string(),
                id: None
            })]
        );
    }

    #[test]
    fn sse_caps_pending_event_bytes() {
        let mut parser = SseParser::new(16);
        let error = parser
            .feed(b"data: 0123456789abcdef0123456789abcdef\n\n")
            .expect_err("capped");
        assert_eq!(error, "MCP SSE event exceeds 16 bytes");
    }

    #[test]
    fn sse_caps_events_streamed_as_many_short_lines() {
        // Each `data:` line is short; the joined event crosses the cap.
        let mut parser = SseParser::new(16);
        let error = parser
            .feed(b"data: 0123456789\ndata: 0123456789\n\n")
            .expect_err("capped");
        assert_eq!(error, "MCP SSE event exceeds 16 bytes");
    }

    #[test]
    fn describe_http_failure_truncates_long_bodies() {
        assert_eq!(
            describe_http_failure(500, "  boom  "),
            "MCP HTTP request failed with status 500: boom"
        );
        let long = "x".repeat(ERROR_MESSAGE_BODY_CHARS + 50);
        let message = describe_http_failure(500, &long);
        assert_eq!(
            message,
            format!(
                "MCP HTTP request failed with status 500: {}...",
                "x".repeat(ERROR_MESSAGE_BODY_CHARS - 3)
            )
        );
    }

    #[test]
    fn needs_authorization_matches_the_bearer_challenge() {
        assert!(needs_authorization(401, None));
        assert!(needs_authorization(
            403,
            Some(r#"Bearer realm="mcp", error="insufficient_scope""#)
        ));
        assert!(needs_authorization(
            403,
            Some("Bearer error=insufficient_scope")
        ));
        assert!(needs_authorization(
            403,
            Some("Bearer ERROR=\"INSUFFICIENT_SCOPE\"")
        ));
        // The upstream regex has no boundary after the value, so a
        // superstring like `insufficient_scope2` matches too.
        assert!(needs_authorization(
            403,
            Some("Bearer error=insufficient_scope2")
        ));
        assert!(!needs_authorization(403, Some("Bearer")));
        assert!(!needs_authorization(403, Some("xerror=insufficient_scope")));
        assert!(!needs_authorization(
            500,
            Some("Bearer error=insufficient_scope")
        ));
    }

    #[test]
    fn is_retryable_covers_transient_statuses_and_network_failures() {
        for status in [408u16, 429, 500, 501, 503] {
            assert!(
                is_retryable(&McpError::Http {
                    status,
                    message: String::new(),
                    body: String::new()
                }),
                "{status}"
            );
        }
        assert!(is_retryable(&McpError::Network("reset".to_string())));
        assert!(!is_retryable(&McpError::AuthRequired {
            www_authenticate: None,
            body: String::new()
        }));
        assert!(!is_retryable(&McpError::SessionExpired {
            body: String::new()
        }));
        assert!(!is_retryable(&McpError::Malformed("bad".to_string())));
    }

    /// One scripted HTTP/1.1 request the test server recorded.
    struct RecordedRequest {
        method: String,
        headers: Vec<(String, String)>,
        body: String,
    }

    impl RecordedRequest {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        }
    }

    /// One scripted server response handler.
    type TestHandler = Arc<dyn Fn(&RecordedRequest, &mut TcpStream) + Send + Sync>;

    /// One parsed HTTP/1.1 request: the request-line method, the headers,
    /// and the content-length body.
    type RawRequest = (String, Vec<(String, String)>, String);

    /// A minimal HTTP/1.1 test server answering each request from
    /// `handler`; recorded requests are readable afterwards.
    struct TestServer {
        url: String,
        requests: Arc<StdMutex<Vec<RecordedRequest>>>,
    }

    impl TestServer {
        fn spawn(handler: TestHandler) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
            let url = format!("http://{}/mcp", listener.local_addr().expect("addr"));
            let requests: Arc<StdMutex<Vec<RecordedRequest>>> = Arc::default();
            let recorded = Arc::clone(&requests);
            std::thread::Builder::new()
                .name("pi-mcp-http-test-server".to_string())
                .spawn(move || {
                    for stream in listener.incoming() {
                        let Ok(mut stream) = stream else { continue };
                        let Some((method, headers, body)) = read_request(&mut stream) else {
                            continue;
                        };
                        recorded
                            .lock()
                            .expect("test server request log poisoned")
                            .push(RecordedRequest {
                                method: method.clone(),
                                headers: headers.clone(),
                                body: body.clone(),
                            });
                        let request = RecordedRequest {
                            method,
                            headers,
                            body,
                        };
                        handler(&request, &mut stream);
                    }
                })
                .expect("spawn test server");
            Self { url, requests }
        }

        fn recorded(&self) -> Vec<(String, Vec<String>)> {
            self.requests
                .lock()
                .expect("test server request log poisoned")
                .iter()
                .map(|request| {
                    (
                        request.method.clone(),
                        request
                            .headers
                            .iter()
                            .map(|(key, value)| format!("{key}: {value}"))
                            .collect::<Vec<_>>(),
                    )
                })
                .collect()
        }
    }

    /// Reads one HTTP/1.1 request: request line, headers, and the
    /// content-length body.
    fn read_request(stream: &mut TcpStream) -> Option<RawRequest> {
        let mut buffer = Vec::new();
        let mut byte = [0u8; 1];
        while !buffer.ends_with(b"\r\n\r\n") {
            let read = stream.read(&mut byte).ok()?;
            if read == 0 {
                return None;
            }
            buffer.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&buffer).into_owned();
        let mut lines = head.split("\r\n");
        let request_line = lines.next()?;
        let method = request_line.split(' ').next()?.to_string();
        let mut headers = Vec::new();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
        let content_length = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; content_length];
        if content_length > 0 {
            stream.read_exact(&mut body).ok()?;
        }
        let body = String::from_utf8_lossy(&body).into_owned();
        Some((method, headers, body))
    }

    /// Writes a complete response and closes the stream.
    fn write_response(
        stream: &mut TcpStream,
        status_line: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) {
        let mut head = format!("{status_line}\r\n");
        for (name, value) in headers {
            let _ = write!(head, "{name}: {value}\r\n");
        }
        let _ = write!(head, "content-length: {}\r\n", body.len());
        head.push_str("connection: close\r\n\r\n");
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(body);
    }

    /// Writes an SSE response without content-length (the body ends at
    /// close) and closes the stream.
    fn write_sse_response(stream: &mut TcpStream, status_line: &str, body: &str) {
        let head = format!(
            "{status_line}\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n"
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(body.as_bytes());
    }

    fn options(url: &str) -> StreamableHttpOptions {
        StreamableHttpOptions {
            url: url.to_string(),
            reconnect_initial_delay_ms: 1,
            ..StreamableHttpOptions::default()
        }
    }

    fn initialize_body() -> Value {
        json!({"protocolVersion": crate::PROTOCOL_VERSION, "capabilities": {}, "clientInfo": {"name": "pi-rs", "version": "0.1.0"}})
    }

    fn initialize_reply(id: i64) -> String {
        json!({
            "jsonrpc": "2.0", "id": id,
            "result": {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
                       "serverInfo": {"name": "synth", "version": "1.0.0"}},
        })
        .to_string()
    }

    #[tokio::test]
    async fn handshake_and_calls_flow_over_json_with_session_and_version_headers() {
        let server = TestServer::spawn(Arc::new(|request, stream| match request.method.as_str() {
            "POST" if request.body.contains(r#""method":"initialize""#) => write_response(
                stream,
                "HTTP/1.1 200 OK",
                &[
                    ("content-type", "application/json"),
                    ("mcp-session-id", "sess-1"),
                ],
                format!("{}\n", initialize_reply(1)).as_bytes(),
            ),
            "POST" => write_response(
                stream,
                "HTTP/1.1 200 OK",
                &[("content-type", "application/json")],
                br#"{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}"#.as_slice(),
            ),
            _ => write_response(stream, "HTTP/1.1 405 Method Not Allowed", &[], b""),
        }));
        // The GET stream would race the recorded-request assertions; the
        // dedicated GET-stream test covers it.
        let transport = StreamableHttpTransport::new(StreamableHttpOptions {
            open_get_stream: false,
            ..options(&server.url)
        })
        .expect("transport");
        transport.start().expect("start");
        transport.set_protocol_version("2025-06-18");
        let result = transport
            .request(
                "initialize",
                Some(initialize_body()),
                Duration::from_secs(5),
            )
            .await
            .expect("initialize");
        assert_eq!(result["serverInfo"]["name"], "synth");
        assert_eq!(transport.session_id().as_deref(), Some("sess-1"));
        transport
            .notify("notifications/initialized", None)
            .await
            .expect("initialized");
        let tools = transport
            .request("tools/list", None, Duration::from_secs(5))
            .await
            .expect("tools/list");
        assert!(tools["tools"].is_array());
        // Every POST after the first carries the session id, the
        // negotiated version, and the exact accept pair.
        for (_, headers) in server.recorded().iter().skip(1) {
            assert!(
                headers
                    .iter()
                    .any(|header| header.eq_ignore_ascii_case("mcp-session-id: sess-1")),
                "{headers:?}"
            );
            assert!(
                headers
                    .iter()
                    .any(|header| header.eq_ignore_ascii_case("mcp-protocol-version: 2025-06-18")),
                "{headers:?}"
            );
            assert!(
                headers.iter().any(|header| header
                    .eq_ignore_ascii_case("accept: application/json, text/event-stream")),
                "{headers:?}"
            );
        }
        transport.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn sse_response_stream_answers_and_routes_later_events() {
        let server = TestServer::spawn(Arc::new(|_request, stream| {
            // The reply event carries an id, then a notification arrives
            // before the server closes the stream.
            let reply = format!(
                "id: 1\nevent: message\ndata: {}\n\nevent: message\ndata: {}\n\n",
                json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}}),
                json!({"jsonrpc": "2.0", "method": "notifications/message", "params": {"level": "info"}}),
            );
            write_sse_response(stream, "HTTP/1.1 200 OK", &reply);
        }));
        let mut transport = StreamableHttpTransport::new(options(&server.url)).expect("transport");
        transport.start().expect("start");
        let result = transport
            .request("tools/list", None, Duration::from_secs(5))
            .await
            .expect("answered over SSE");
        assert_eq!(result["ok"], true);
        let inbound = transport.inbound.recv().await.expect("notification routed");
        assert!(
            matches!(inbound, InboundMessage::Notification(ref notification)
                     if notification.method == "notifications/message"),
            "{inbound:?}"
        );
    }

    #[tokio::test]
    async fn a_broken_response_stream_resumes_with_last_event_id() {
        let counter = Arc::new(StdMutex::new(true));
        let server = TestServer::spawn(Arc::new(move |request, stream| {
            if request.method == "GET" {
                // The resumption stream answers the pending request.
                assert!(
                    request.header("last-event-id") == Some("1"),
                    "expected the Last-Event-ID header, got {:?}",
                    request.headers
                );
                let reply = format!(
                    "id: 2\nevent: message\ndata: {}\n\n",
                    json!({"jsonrpc": "2.0", "id": 1, "result": {"resumed": true}})
                );
                write_sse_response(stream, "HTTP/1.1 200 OK", &reply);
                return;
            }
            let is_first_post = {
                let mut flag = counter.lock().expect("test flag poisoned");
                let was = *flag;
                *flag = false;
                was
            };
            if is_first_post {
                // The response stream primes an id and breaks without
                // answering.
                write_sse_response(stream, "HTTP/1.1 200 OK", "id: 1\n\n");
                return;
            }
            unreachable!("only one POST is scripted");
        }));
        let transport = StreamableHttpTransport::new(options(&server.url)).expect("transport");
        transport.start().expect("start");
        let result = transport
            .request("tools/list", None, Duration::from_secs(5))
            .await
            .expect("answered on the resumed stream");
        assert_eq!(result["resumed"], true);
    }

    #[tokio::test]
    async fn a_stream_ending_without_ids_or_answer_fails_the_request() {
        let server = TestServer::spawn(Arc::new(|_request, stream| {
            write_sse_response(stream, "HTTP/1.1 200 OK", "event: message\n\n");
        }));
        let transport = StreamableHttpTransport::new(options(&server.url)).expect("transport");
        transport.start().expect("start");
        let error = transport
            .request("tools/list", None, Duration::from_secs(5))
            .await
            .expect_err("no answer");
        assert!(
            matches!(error, McpError::Rpc { code: -32603, .. }),
            "{error}"
        );
    }

    #[tokio::test]
    async fn batched_json_replies_route_notifications_and_the_answer() {
        let server = TestServer::spawn(Arc::new(|_request, stream| {
            let body = json!([
                {"jsonrpc": "2.0", "method": "notifications/message"},
                {"jsonrpc": "2.0", "id": 1, "result": {"batched": true}},
            ]);
            write_response(
                stream,
                "HTTP/1.1 200 OK",
                &[("content-type", "application/json")],
                serde_json::to_vec(&body).expect("body").as_slice(),
            );
        }));
        let mut transport = StreamableHttpTransport::new(options(&server.url)).expect("transport");
        transport.start().expect("start");
        let result = transport
            .request("tools/list", None, Duration::from_secs(5))
            .await
            .expect("batched reply");
        assert_eq!(result["batched"], true);
        let inbound = transport.inbound.recv().await.expect("notification routed");
        assert!(
            matches!(inbound, InboundMessage::Notification(_)),
            "{inbound:?}"
        );
    }

    #[tokio::test]
    async fn http_failures_surface_the_upstream_error_kinds() {
        // 401 → AuthRequired with the challenge; 404 with a session →
        // SessionExpired; 404 without → generic; 202 on a request →
        // "without a response"; wrong content type → unsupported.
        let server = TestServer::spawn(Arc::new(|request, stream| match request.method.as_str() {
            "POST" if request.body.contains(r#""method":"initialize""#) => write_response(
                stream,
                "HTTP/1.1 200 OK",
                &[
                    ("content-type", "application/json"),
                    ("mcp-session-id", "sess-9"),
                ],
                format!("{}\n", initialize_reply(1)).as_bytes(),
            ),
            "POST" if request.body.contains(r#""method":"tools/call""#) => {
                write_response(stream, "HTTP/1.1 202 Accepted", &[], b"");
            }
            "POST" => write_response(
                stream,
                "HTTP/1.1 404 Not Found",
                &[("content-type", "text/plain")],
                b"gone".as_slice(),
            ),
            _ => write_response(stream, "HTTP/1.1 405 Method Not Allowed", &[], b""),
        }));
        let transport = StreamableHttpTransport::new(options(&server.url)).expect("transport");
        transport.start().expect("start");
        transport
            .request(
                "initialize",
                Some(initialize_body()),
                Duration::from_secs(5),
            )
            .await
            .expect("initialize");
        let error = transport
            .request("tools/list", None, Duration::from_secs(5))
            .await
            .expect_err("404 with a session");
        assert!(matches!(error, McpError::SessionExpired { .. }), "{error}");
        let error = transport
            .request(
                "tools/call",
                Some(json!({"name": "x"})),
                Duration::from_secs(5),
            )
            .await
            .expect_err("202 on a request");
        let McpError::Http { message, .. } = &error else {
            panic!("{error}")
        };
        assert_eq!(
            message,
            "MCP server accepted request tools/call without a response"
        );
        transport.shutdown().await.expect("shutdown");

        // A fresh transport without a session: the 401 carries the
        // challenge, and the 404 is a generic failure.
        let server = TestServer::spawn(Arc::new(|_request, stream| {
            write_response(
                stream,
                "HTTP/1.1 401 Unauthorized",
                &[("www-authenticate", r#"Bearer realm="mcp""#)],
                b"who?",
            );
        }));
        let transport = StreamableHttpTransport::new(options(&server.url)).expect("transport");
        transport.start().expect("start");
        let error = transport
            .request(
                "initialize",
                Some(initialize_body()),
                Duration::from_secs(5),
            )
            .await
            .expect_err("401");
        let McpError::AuthRequired {
            www_authenticate,
            body,
        } = &error
        else {
            panic!("{error}")
        };
        assert_eq!(www_authenticate.as_deref(), Some(r#"Bearer realm="mcp""#));
        assert_eq!(body, "who?");
    }

    #[tokio::test]
    async fn error_bodies_are_capped_and_described() {
        let server = TestServer::spawn(Arc::new(|_request, stream| {
            let body = "y".repeat(MAX_ERROR_BODY_BYTES * 2);
            write_response(
                stream,
                "HTTP/1.1 503 Service Unavailable",
                &[("content-type", "text/plain")],
                body.as_bytes(),
            );
        }));
        let transport = StreamableHttpTransport::new(options(&server.url)).expect("transport");
        transport.start().expect("start");
        let error = transport
            .request("tools/list", None, Duration::from_secs(5))
            .await
            .expect_err("503");
        let McpError::Http {
            status,
            message,
            body,
        } = &error
        else {
            panic!("{error}")
        };
        assert_eq!(*status, 503);
        assert_eq!(body.len(), MAX_ERROR_BODY_BYTES);
        assert!(
            message.starts_with("MCP HTTP request failed with status 503: "),
            "{message}"
        );
    }

    #[tokio::test]
    async fn unsupported_content_type_fails_the_request() {
        let server = TestServer::spawn(Arc::new(|_request, stream| {
            write_response(
                stream,
                "HTTP/1.1 200 OK",
                &[("content-type", "text/html; charset=utf-8")],
                b"<html/>",
            );
        }));
        let transport = StreamableHttpTransport::new(options(&server.url)).expect("transport");
        transport.start().expect("start");
        let error = transport
            .request("tools/list", None, Duration::from_secs(5))
            .await
            .expect_err("html");
        let McpError::Http { message, .. } = &error else {
            panic!("{error}")
        };
        assert_eq!(message, "Unsupported MCP response content type: text/html");
    }

    #[tokio::test]
    async fn shutdown_sends_the_session_delete() {
        let server = TestServer::spawn(Arc::new(|request, stream| match request.method.as_str() {
            "POST" if request.body.contains(r#""method":"initialize""#) => write_response(
                stream,
                "HTTP/1.1 200 OK",
                &[
                    ("content-type", "application/json"),
                    ("mcp-session-id", "sess-2"),
                ],
                format!("{}\n", initialize_reply(1)).as_bytes(),
            ),
            "DELETE" => write_response(stream, "HTTP/1.1 200 OK", &[], b""),
            _ => write_response(stream, "HTTP/1.1 405 Method Not Allowed", &[], b""),
        }));
        let transport = StreamableHttpTransport::new(options(&server.url)).expect("transport");
        transport.start().expect("start");
        transport
            .request(
                "initialize",
                Some(initialize_body()),
                Duration::from_secs(5),
            )
            .await
            .expect("initialize");
        transport.shutdown().await.expect("shutdown");
        let methods: Vec<String> = server
            .recorded()
            .into_iter()
            .map(|(method, _)| method)
            .collect();
        assert!(methods.contains(&"DELETE".to_string()), "{methods:?}");
    }

    #[tokio::test]
    async fn the_get_stream_opens_after_initialized_and_routes_notifications() {
        let server = TestServer::spawn(Arc::new(|request, stream| match request.method.as_str() {
            "POST" if request.body.contains(r#""method":"initialize""#) => write_response(
                stream,
                "HTTP/1.1 200 OK",
                &[("content-type", "application/json")],
                format!("{}\n", initialize_reply(1)).as_bytes(),
            ),
            "POST" => write_response(stream, "HTTP/1.1 202 Accepted", &[], b""),
            "GET" => {
                let event = format!(
                    "event: message\ndata: {}\n\n",
                    json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})
                );
                write_sse_response(stream, "HTTP/1.1 200 OK", &event);
            }
            _ => write_response(stream, "HTTP/1.1 405 Method Not Allowed", &[], b""),
        }));
        let mut transport = StreamableHttpTransport::new(options(&server.url)).expect("transport");
        transport.start().expect("start");
        transport
            .request(
                "initialize",
                Some(initialize_body()),
                Duration::from_secs(5),
            )
            .await
            .expect("initialize");
        transport
            .notify("notifications/initialized", None)
            .await
            .expect("initialized");
        let inbound = tokio::time::timeout(Duration::from_secs(5), transport.inbound.recv())
            .await
            .expect("GET stream notification in time")
            .expect("notification routed");
        assert!(
            matches!(inbound, InboundMessage::Notification(ref notification)
                     if notification.method == "notifications/tools/list_changed"),
            "{inbound:?}"
        );
        transport.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn a_bearer_token_rides_the_authorization_header() {
        struct Fixed;
        impl crate::auth_provider::AuthProvider for Fixed {
            fn token(
                &self,
            ) -> Pin<Box<dyn Future<Output = Result<Option<String>, McpError>> + Send + '_>>
            {
                Box::pin(std::future::ready(Ok(Some("tok-1".to_string()))))
            }

            fn on_unauthorized(
                &self,
                _context: crate::auth_provider::UnauthorizedContext,
            ) -> Pin<Box<dyn Future<Output = Result<(), McpError>> + Send + '_>> {
                Box::pin(std::future::pending())
            }
        }
        let server = TestServer::spawn(Arc::new(|_request, stream| {
            write_response(
                stream,
                "HTTP/1.1 200 OK",
                &[("content-type", "application/json")],
                br#"{"jsonrpc":"2.0","id":1,"result":{}}"#.as_slice(),
            );
        }));
        let transport = StreamableHttpTransport::new(StreamableHttpOptions {
            url: server.url.clone(),
            headers: HashMap::from([("x-custom".to_string(), "yes".to_string())]),
            auth: Some(Arc::new(Fixed)),
            ..StreamableHttpOptions::default()
        })
        .expect("transport");
        transport.start().expect("start");
        transport
            .request("ping", None, Duration::from_secs(5))
            .await
            .expect("ping");
        let recorded = server.recorded();
        assert!(
            recorded[0]
                .1
                .iter()
                .any(|header| header.eq_ignore_ascii_case("authorization: Bearer tok-1")),
            "{:?}",
            recorded[0]
        );
        assert!(
            recorded[0]
                .1
                .iter()
                .any(|header| header.eq_ignore_ascii_case("x-custom: yes")),
            "{:?}",
            recorded[0]
        );
    }

    /// A provider whose token flips after its first recovery, so the
    /// retried request carries the replacement.
    #[tokio::test]
    async fn a_challenged_request_recovers_once_and_retries_with_the_new_token() {
        struct Recovering(Arc<std::sync::atomic::AtomicUsize>);
        impl crate::auth_provider::AuthProvider for Recovering {
            fn token(
                &self,
            ) -> Pin<Box<dyn Future<Output = Result<Option<String>, McpError>> + Send + '_>>
            {
                let generation = self.0.load(std::sync::atomic::Ordering::SeqCst);
                Box::pin(std::future::ready(Ok(Some(format!("tok-{generation}")))))
            }

            fn recovers(&self) -> bool {
                true
            }

            fn on_unauthorized(
                &self,
                context: crate::auth_provider::UnauthorizedContext,
            ) -> Pin<Box<dyn Future<Output = Result<(), McpError>> + Send + '_>> {
                // Consume the response like the OAuth provider would and
                // flip the token, so the retry carries the new one.
                drop(context);
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Box::pin(std::future::ready(Ok(())))
            }
        }
        let generation = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server = TestServer::spawn(Arc::new({
            let generation = Arc::clone(&generation);
            move |request, stream| {
                if generation.load(std::sync::atomic::Ordering::SeqCst) == 0
                    && request.body.contains(r#""method":"initialize""#)
                {
                    // The first initialize is challenged...
                    return write_response(
                        stream,
                        "HTTP/1.1 401 Unauthorized",
                        &[("www-authenticate", r#"Bearer realm="mcp""#)],
                        b"nope",
                    );
                }
                if request.body.contains(r#""method":"initialize""#) {
                    // ...the retried one initializes.
                    return write_response(
                        stream,
                        "HTTP/1.1 200 OK",
                        &[("content-type", "application/json")],
                        format!("{}\n", initialize_reply(1)).as_bytes(),
                    );
                }
                write_response(
                    stream,
                    "HTTP/1.1 200 OK",
                    &[("content-type", "application/json")],
                    br#"{"jsonrpc":"2.0","id":9,"result":{}}"#.as_slice(),
                );
            }
        }));
        let transport = StreamableHttpTransport::new(StreamableHttpOptions {
            url: server.url.clone(),
            auth: Some(Arc::new(Recovering(Arc::clone(&generation)))),
            ..options(&server.url)
        })
        .expect("transport");
        transport.start().expect("start");
        transport
            .request(
                "initialize",
                Some(initialize_body()),
                Duration::from_secs(5),
            )
            .await
            .expect("recovered initialize");
        // Two initialize requests went out, the second with the new token.
        let recorded = server.recorded();
        let initializes: Vec<_> = recorded
            .iter()
            .filter(|(_, headers)| {
                headers
                    .iter()
                    .any(|header| header.to_lowercase().starts_with("authorization:"))
            })
            .collect();
        assert_eq!(initializes.len(), 2, "{recorded:?}");
        assert!(
            initializes[0]
                .1
                .iter()
                .any(|header| header.eq_ignore_ascii_case("authorization: Bearer tok-0")),
            "{:?}",
            initializes[0]
        );
        assert!(
            initializes[1]
                .1
                .iter()
                .any(|header| header.eq_ignore_ascii_case("authorization: Bearer tok-1")),
            "{:?}",
            initializes[1]
        );
        transport.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn a_provider_without_recovery_leaves_the_challenge_to_the_caller() {
        struct FixedNoRecovery;
        impl crate::auth_provider::AuthProvider for FixedNoRecovery {
            fn token(
                &self,
            ) -> Pin<Box<dyn Future<Output = Result<Option<String>, McpError>> + Send + '_>>
            {
                Box::pin(std::future::ready(Ok(Some("tok".to_string()))))
            }

            fn on_unauthorized(
                &self,
                _context: crate::auth_provider::UnauthorizedContext,
            ) -> Pin<Box<dyn Future<Output = Result<(), McpError>> + Send + '_>> {
                Box::pin(std::future::pending())
            }
        }
        let server = TestServer::spawn(Arc::new(|_request, stream| {
            write_response(
                stream,
                "HTTP/1.1 401 Unauthorized",
                &[("www-authenticate", r#"Bearer error="insufficient_scope""#)],
                b"nope",
            );
        }));
        let transport = StreamableHttpTransport::new(StreamableHttpOptions {
            url: server.url.clone(),
            auth: Some(Arc::new(FixedNoRecovery)),
            ..options(&server.url)
        })
        .expect("transport");
        transport.start().expect("start");
        // Even an insufficient_scope 403-class challenge stays a plain 401
        // without a recovery hook, and exactly one request goes out.
        let error = transport
            .request(
                "initialize",
                Some(initialize_body()),
                Duration::from_secs(5),
            )
            .await
            .expect_err("challenged");
        let McpError::AuthRequired { body, .. } = &error else {
            panic!("{error}")
        };
        assert_eq!(body, "nope");
        assert_eq!(server.recorded().len(), 1);
        transport.shutdown().await.expect("shutdown");
    }
}
