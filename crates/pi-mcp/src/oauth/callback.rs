//! The loopback OAuth callback server
//! (`packages/mcp/src/oauth/callback.ts`): a minimal local HTTP server
//! that receives the authorization response the browser redirects to, one
//! pending wait per `state` parameter. Upstream runs node's `http`
//! server; this port runs a tokio `TcpListener` with a hand-rolled
//! one-request-per-connection handler (the browser's redirect is a plain
//! GET), closing every connection after its reply — upstream's
//! `closeAllConnections` therefore has no lingering sockets to sweep, and
//! `close()` does not wait for one.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use url::Url;

use crate::error::McpError;
use crate::oauth::errors::OAuthError;

type Result<T> = std::result::Result<T, McpError>;

fn other(message: impl Into<String>) -> McpError {
    McpError::OAuth(OAuthError::Other(message.into()))
}

/// The authorization response of one sign-in (`OAuthCallback`,
/// `callback.ts:5-9`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthCallback {
    pub code: String,
    pub state: String,
    pub iss: Option<String>,
}

/// Outcome shown on the browser page after the redirect
/// (`OAuthCallbackPage`, `callback.ts:11-13`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OAuthCallbackPage {
    Ok,
    Error {
        message: String,
        details: Option<String>,
    },
}

/// Renders the browser page as HTML (`renderPage`,
/// `callback.ts:30`). Default: a plain-text message.
pub type RenderPage = Arc<dyn Fn(&OAuthCallbackPage) -> String + Send + Sync>;

/// Options of [`OAuthCallbackServer::listen`] (`OAuthCallbackServerOptions`,
/// `callback.ts:15-31`).
#[derive(Default)]
pub struct OAuthCallbackServerOptions<'a> {
    /// Address to listen on. Default: `127.0.0.1`.
    pub host: Option<&'a str>,
    /// Host name in `redirectUrl`, for example `localhost` for a client
    /// registered with it while listening on `127.0.0.1`. Default: host.
    pub redirect_host: Option<&'a str>,
    /// Bind port; 0 (default) picks a free one.
    pub port: Option<u16>,
    /// Request path receiving the callback. Default: `/callback`.
    pub path: Option<&'a str>,
    /// More paths that receive the callback, for example a server-specific
    /// path of a redirect URI.
    pub extra_paths: Vec<String>,
    /// How long a wait may take. Default: 5 minutes.
    pub timeout_ms: Option<u64>,
    /// Render the browser page as HTML. Default: a plain-text message.
    pub render_page: Option<RenderPage>,
}

/// Plain-text page of the default rendering (`plainText`,
/// `callback.ts:33-38`).
fn plain_text(page: &OAuthCallbackPage) -> String {
    match page {
        OAuthCallbackPage::Ok => "Authorization complete. You may close this window.".to_string(),
        OAuthCallbackPage::Error { message, details } => details.as_ref().map_or_else(
            || message.clone(),
            |details| format!("{message}\n\n{details}"),
        ),
    }
}

#[derive(Debug)]
struct Pending {
    sender: oneshot::Sender<Result<OAuthCallback>>,
    /// The exact path the response must arrive on, when the wait asked for
    /// one.
    path: Option<String>,
}

/// One registered wait, resolved by the accept loop or by its own timeout
/// (`waitForCallback`, `callback.ts:108-123`).
#[derive(Debug)]
pub struct WaitForCallback {
    state: String,
    pending: Arc<Mutex<HashMap<String, Pending>>>,
    receiver: oneshot::Receiver<Result<OAuthCallback>>,
    deadline: Pin<Box<tokio::time::Sleep>>,
}

impl Future for WaitForCallback {
    type Output = Result<OAuthCallback>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if let Poll::Ready(outcome) = Pin::new(&mut this.receiver).poll(cx) {
            return Poll::Ready(outcome.unwrap_or_else(|_| {
                // The sender is gone: the wait was removed (its timeout or
                // `close`).
                Err(other("OAuth callback server closed"))
            }));
        }
        if this.deadline.as_mut().poll(cx).is_ready() {
            this.pending
                .lock()
                .expect("callback pending mutex poisoned")
                .remove(&this.state);
            return Poll::Ready(Err(other("OAuth callback timed out")));
        }
        Poll::Pending
    }
}

/// The loopback callback server (`OAuthCallbackServer`,
/// `callback.ts:40-176`).
pub struct OAuthCallbackServer {
    /// The redirect URI the server answers (`redirectUrl`): the listening
    /// port filled in.
    redirect_url: String,
    timeout_ms: u64,
    pending: Arc<Mutex<HashMap<String, Pending>>>,
    accept_task: JoinHandle<()>,
}

impl OAuthCallbackServer {
    /// Binds the server and starts accepting
    /// (`OAuthCallbackServer.listen`, `callback.ts:56-89`).
    ///
    /// # Errors
    /// When the listener cannot bind or report its address.
    pub async fn listen(options: OAuthCallbackServerOptions<'_>) -> Result<Self> {
        let host = options.host.unwrap_or("127.0.0.1");
        let redirect_host = options.redirect_host.unwrap_or(host);
        let path = options.path.unwrap_or("/callback").to_string();
        let listener = TcpListener::bind((host, options.port.unwrap_or(0)))
            .await
            .map_err(|error| other(error.to_string()))?;
        let port = listener
            .local_addr()
            .map_err(|error| other(error.to_string()))?
            .port();
        let redirect_url = format!(
            "http://{}:{port}{path}",
            if redirect_host.contains(':') {
                format!("[{redirect_host}]")
            } else {
                redirect_host.to_string()
            }
        );
        let paths = Arc::new(
            [path]
                .into_iter()
                .chain(options.extra_paths)
                .collect::<Vec<_>>(),
        );
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let render_page = options.render_page;
        let accept_task = tokio::spawn(accept_loop(
            listener,
            redirect_url.clone(),
            Arc::clone(&paths),
            Arc::clone(&pending),
            render_page,
        ));
        Ok(Self {
            redirect_url,
            timeout_ms: options.timeout_ms.unwrap_or(5 * 60_000),
            pending,
            accept_task,
        })
    }

    /// The redirect URI the server serves.
    pub fn redirect_url(&self) -> &str {
        &self.redirect_url
    }

    /// Wait for the authorization response with `state`. With `path`, a
    /// response on another path fails, so a server-specific redirect URI
    /// can tell authorization servers apart (RFC 9700 section 4.4.2.2)
    /// (`waitForCallback`, `callback.ts:99-123`).
    ///
    /// # Errors
    /// When `state` is already pending.
    pub fn wait_for_callback(&self, state: &str, path: Option<String>) -> Result<WaitForCallback> {
        let mut pending = self
            .pending
            .lock()
            .expect("callback pending mutex poisoned");
        if pending.contains_key(state) {
            return Err(other("OAuth state is already pending"));
        }
        let (sender, receiver) = oneshot::channel();
        pending.insert(state.to_string(), Pending { sender, path });
        drop(pending);
        Ok(WaitForCallback {
            state: state.to_string(),
            pending: Arc::clone(&self.pending),
            receiver,
            deadline: Box::pin(tokio::time::sleep(Duration::from_millis(self.timeout_ms))),
        })
    }

    /// Rejects every pending wait and stops accepting (`close`,
    /// `callback.ts:125-143`).
    pub fn close(self) {
        let mut pending = self
            .pending
            .lock()
            .expect("callback pending mutex poisoned");
        for (_, entry) in pending.drain() {
            let _ = entry
                .sender
                .send(Err(other("OAuth callback server closed")));
        }
        drop(pending);
        self.accept_task.abort();
    }
}

/// Accepts connections until the task is aborted (`createServer` +
/// `server.listen`, `callback.ts:61-77`).
async fn accept_loop(
    listener: TcpListener,
    redirect_url: String,
    paths: Arc<Vec<String>>,
    pending: Arc<Mutex<HashMap<String, Pending>>>,
    render_page: Option<RenderPage>,
) {
    loop {
        let Ok((stream, _address)) = listener.accept().await else {
            continue;
        };
        tokio::spawn(handle_connection(
            stream,
            redirect_url.clone(),
            Arc::clone(&paths),
            Arc::clone(&pending),
            render_page.clone(),
        ));
    }
}

/// Reads one request, answers it, and closes the connection (the
/// `instance?.handle(request.url, response)` callback plus node's
/// close-after-reply).
async fn handle_connection(
    mut stream: TcpStream,
    redirect_url: String,
    paths: Arc<Vec<String>>,
    pending: Arc<Mutex<HashMap<String, Pending>>>,
    render_page: Option<RenderPage>,
) {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    let request_head = loop {
        match stream.read(&mut chunk).await {
            Ok(read) if read > 0 => {
                buffer.extend_from_slice(&chunk[..read]);
                if let Some(end) = find_header_end(&buffer) {
                    break end;
                }
                if buffer.len() > 64 * 1024 {
                    return;
                }
            }
            _ => return,
        }
    };
    // First line: `GET /path?query HTTP/1.1` — the target is the second
    // token (node hands `request.url` to `handle`).
    let head = String::from_utf8_lossy(&buffer[..request_head]);
    let target = head
        .lines()
        .next()
        .unwrap_or_default()
        .split(' ')
        .nth(1)
        .unwrap_or("/")
        .to_string();
    let (status, headers, body) = handle_request(
        &target,
        &redirect_url,
        &paths,
        &pending,
        render_page.as_ref(),
    );
    let reason = match status {
        400 => "Bad Request",
        404 => "Not Found",
        _ => "OK",
    };
    let mut response = format!("HTTP/1.1 {status} {reason}\r\n");
    for (name, value) in &headers {
        let _ = write!(response, "{name}: {value}\r\n");
    }
    let _ = write!(
        response,
        "connection: close\r\ncontent-length: {}\r\n\r\n",
        body.len()
    );
    if stream.write_all(response.as_bytes()).await.is_ok()
        && stream.write_all(body.as_bytes()).await.is_ok()
    {
        let _ = stream.flush().await;
    }
    let _ = stream.shutdown().await;
}

/// The `\r\n\r\n` the request head ends at, if it arrived.
fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

/// The first value of one query parameter (`searchParams.get`).
fn query_parameter(url: &Url, name: &str) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}

/// One request against the pending waits (`handle`,
/// `callback.ts:154-176`).
fn handle_request(
    target: &str,
    redirect_url: &str,
    paths: &[String],
    pending: &Mutex<HashMap<String, Pending>>,
    render_page: Option<&RenderPage>,
) -> (u16, Vec<(&'static str, String)>, String) {
    let reply =
        |status: u16, page: OAuthCallbackPage| -> (u16, Vec<(&'static str, String)>, String) {
            match render_page {
                Some(render) => (
                    status,
                    vec![
                        ("content-type", "text/html; charset=utf-8".to_string()),
                        ("cache-control", "no-store".to_string()),
                    ],
                    render(&page),
                ),
                None => (
                    status,
                    vec![("content-type", "text/plain; charset=utf-8".to_string())],
                    plain_text(&page),
                ),
            }
        };
    let Ok(url) = Url::parse(redirect_url).and_then(|base| base.join(target)) else {
        return reply(
            404,
            OAuthCallbackPage::Error {
                message: "Not found".to_string(),
                details: None,
            },
        );
    };
    if !paths.iter().any(|path| path == url.path()) {
        return reply(
            404,
            OAuthCallbackPage::Error {
                message: "Not found".to_string(),
                details: None,
            },
        );
    }
    let state = query_parameter(&url, "state").filter(|state| !state.is_empty());
    let entry = {
        let mut pending = pending.lock().expect("callback pending mutex poisoned");
        state.as_deref().and_then(|state| pending.remove(state))
    };
    let Some(entry) = entry else {
        return reply(
            400,
            OAuthCallbackPage::Error {
                message: "Invalid or expired OAuth state".to_string(),
                details: None,
            },
        );
    };
    if let Some(expected) = &entry.path
        && url.path() != expected.as_str()
    {
        let _ = entry.sender.send(Err(other(
            "The authorization response arrived on another redirect URI",
        )));
        return reply(
            400,
            OAuthCallbackPage::Error {
                message: "Unexpected redirect URI".to_string(),
                details: None,
            },
        );
    }
    if let Some(error) = query_parameter(&url, "error") {
        // `??`: an empty description stays empty (`callback.ts:166`).
        let description = query_parameter(&url, "error_description").unwrap_or(error);
        let _ = entry.sender.send(Err(other(description.clone())));
        return reply(
            200,
            OAuthCallbackPage::Error {
                message: "Authorization failed. You may close this window.".to_string(),
                details: Some(description),
            },
        );
    }
    let Some(code) = query_parameter(&url, "code").filter(|code| !code.is_empty()) else {
        let _ = entry.sender.send(Err(other(
            "OAuth callback did not include an authorization code",
        )));
        return reply(
            400,
            OAuthCallbackPage::Error {
                message: "Missing authorization code".to_string(),
                details: None,
            },
        );
    };
    let iss = query_parameter(&url, "iss").filter(|iss| !iss.is_empty());
    let _ = entry.sender.send(Ok(OAuthCallback {
        code,
        state: state.expect("matched a pending wait above"),
        iss,
    }));
    reply(200, OAuthCallbackPage::Ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One raw GET against a full redirect URL, returning the raw reply.
    async fn get(url: &str) -> String {
        let rest = url.trim_start_matches("http://");
        let (authority, path) = match rest.find('/') {
            Some(at) => (&rest[..at], &rest[at..]),
            None => (rest, "/"),
        };
        let mut stream = TcpStream::connect(authority).await.expect("connect");
        stream
            .write_all(format!("GET {path} HTTP/1.1\r\nhost: local\r\n\r\n").as_bytes())
            .await
            .expect("write");
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply).await.expect("read to end");
        String::from_utf8_lossy(&reply).into_owned()
    }

    #[tokio::test]
    async fn the_callback_resolves_the_wait_and_answers_the_browser() {
        let server = OAuthCallbackServer::listen(OAuthCallbackServerOptions::default())
            .await
            .expect("listen");
        let wait = server
            .wait_for_callback("state-1", Some("/callback".to_string()))
            .expect("wait");
        let reply = get(&format!(
            "{}?code=the-code&state=state-1&iss=https%3A%2F%2Fauth.example",
            server.redirect_url()
        ))
        .await;
        assert!(reply.starts_with("HTTP/1.1 200 OK"), "{reply}");
        assert!(reply.contains("content-type: text/plain; charset=utf-8"));
        assert!(reply.contains("Authorization complete. You may close this window."));
        let callback = wait.await.expect("callback");
        assert_eq!(callback.code, "the-code");
        assert_eq!(callback.state, "state-1");
        assert_eq!(callback.iss.as_deref(), Some("https://auth.example"));
        server.close();
    }

    #[tokio::test]
    async fn the_browser_error_is_reported_and_the_page_says_what_happened() {
        let server = OAuthCallbackServer::listen(OAuthCallbackServerOptions::default())
            .await
            .expect("listen");
        let wait = server.wait_for_callback("state-1", None).expect("wait");
        let reply = get(&format!(
            "{}?error=access_denied&error_description=The%20user%20said%20no&state=state-1",
            server.redirect_url()
        ))
        .await;
        assert!(reply.starts_with("HTTP/1.1 200 OK"), "{reply}");
        assert!(
            reply.contains("Authorization failed. You may close this window.\n\nThe user said no"),
            "{reply}"
        );
        let error = wait.await.expect_err("denied");
        assert_eq!(error.to_string(), "The user said no");
        server.close();
    }

    #[tokio::test]
    async fn wrong_paths_and_unknown_states_fail_with_the_upstream_messages() {
        let server = OAuthCallbackServer::listen(OAuthCallbackServerOptions::default())
            .await
            .expect("listen");
        let wait = server
            .wait_for_callback("state-1", Some("/callback".to_string()))
            .expect("wait");
        // Another path → 404 before any state handling.
        let reply = get(&format!(
            "{}/elsewhere?state=state-1",
            server.redirect_url()
        ))
        .await;
        assert!(reply.starts_with("HTTP/1.1 404 Not Found"), "{reply}");
        // On the right path but without a code → the missing-code message.
        let reply = get(&format!("{}?state=state-1", server.redirect_url())).await;
        assert!(reply.starts_with("HTTP/1.1 400 Bad Request"), "{reply}");
        assert!(reply.contains("Missing authorization code"));
        let error = wait.await.expect_err("missing code");
        assert_eq!(
            error.to_string(),
            "OAuth callback did not include an authorization code"
        );
        // Unknown state → invalid state, and the wait stays pending.
        let reply = get(&format!("{}?code=c&state=unknown", server.redirect_url())).await;
        assert!(reply.starts_with("HTTP/1.1 400 Bad Request"), "{reply}");
        assert!(reply.contains("Invalid or expired OAuth state"));
        let wait = server.wait_for_callback("state-2", None).expect("wait");
        let reply = get(&format!("{}?code=c", server.redirect_url())).await;
        assert!(reply.contains("Invalid or expired OAuth state"));
        drop(wait);
        server.close();
    }

    #[tokio::test]
    async fn duplicate_waits_the_timeout_and_close_reject_pending() {
        let server = OAuthCallbackServer::listen(OAuthCallbackServerOptions {
            timeout_ms: Some(50),
            ..OAuthCallbackServerOptions::default()
        })
        .await
        .expect("listen");
        let first = server.wait_for_callback("state-1", None).expect("wait");
        let error = server
            .wait_for_callback("state-1", None)
            .expect_err("duplicate state");
        assert_eq!(error.to_string(), "OAuth state is already pending");
        let error = tokio::time::timeout(Duration::from_secs(2), first)
            .await
            .expect("no hang")
            .expect_err("timed out");
        assert_eq!(error.to_string(), "OAuth callback timed out");
        // After the timeout the state may be waited on again.
        let second = server
            .wait_for_callback("state-1", None)
            .expect("wait again");
        server.close();
        let error = second.await.expect_err("closed");
        assert_eq!(error.to_string(), "OAuth callback server closed");
    }

    #[tokio::test]
    async fn a_rendered_page_rides_html_with_no_store() {
        let server = OAuthCallbackServer::listen(OAuthCallbackServerOptions {
            render_page: Some(Arc::new(|page: &OAuthCallbackPage| match page {
                OAuthCallbackPage::Ok => "<html>ok</html>".to_string(),
                OAuthCallbackPage::Error { message, .. } => format!("<html>{message}</html>"),
            })),
            ..OAuthCallbackServerOptions::default()
        })
        .await
        .expect("listen");
        let wait = server.wait_for_callback("state-1", None).expect("wait");
        let reply = get(&format!("{}?code=c&state=state-1", server.redirect_url())).await;
        assert!(
            reply.contains("content-type: text/html; charset=utf-8"),
            "{reply}"
        );
        assert!(reply.contains("cache-control: no-store"));
        assert!(reply.contains("<html>ok</html>"));
        wait.await.expect("callback");
        server.close();
    }
}
