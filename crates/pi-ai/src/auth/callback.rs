//! Loopback browser callback (`packages/ai/src/auth/oauth/callback-server.ts`).
//!
//! Ground truth: `OAuthCallbackServerOptions` (`:12-31`), the server contract
//! (`:33-43`), route/state/error/code handling and HTTP status codes
//! (`:78-116`), `redirectUri` construction with IPv6 brackets (`:136-138`),
//! `cancel`/`close` (`:139-147`), and `waitForCallbackOrManualInput`
//! (`:155-183`). Browser pages follow `utils/oauth-page.ts:80-99`.
//!
//! Deviations: Node's `http.createServer` is a `std::net::TcpListener` in a
//! blocking task (one request per connection), polled every 20 ms for
//! cancellation; a 15-minute safety timeout was added where upstream has none
//! unless the caller passes `timeoutMs`. `wait()` resolves `None` on cancel
//! (upstream `undefined`) so manual paste can continue.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use tokio::sync::oneshot;

use super::http::parse_query;
use super::{AuthError, AuthPrompt, CancelToken, Interaction};

/// `OAuthCallbackServerOptions` (`callback-server.ts:12-31`).
pub struct CallbackOptions<'a> {
    pub provider_name: &'a str,
    pub host: &'a str,
    pub port: u16,
    pub path: &'a str,
    pub redirect_host: Option<&'a str>,
    pub state: Option<&'a str>,
    pub cancel: CancelToken,
    pub timeout: Option<Duration>,
}

/// The browser callback plus the manual-code alternative
/// (`waitForCallbackOrManualInput`'s return type, `:155-159`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallbackResult {
    Callback(String),
    Manual(String),
}

/// The manual prompt's message/placeholder (`callback-server.ts:158`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManualPrompt {
    pub message: String,
    pub placeholder: String,
}

/// `OAuthCallbackServer` (`callback-server.ts:33-43`).
pub struct CallbackServer {
    redirect_uri: String,
    cancel: CancelToken,
    outcome: Option<oneshot::Receiver<Result<Option<String>, AuthError>>>,
}

impl CallbackServer {
    /// `startOAuthCallbackServer` (`callback-server.ts:50-148`).
    ///
    /// # Errors
    /// When the loopback port cannot be bound.
    pub fn start(options: &CallbackOptions<'_>) -> Result<Self, AuthError> {
        if options.cancel.is_cancelled() {
            return Err(AuthError::Message(super::CANCEL_MESSAGE.to_string()));
        }
        let listener = TcpListener::bind((options.host, options.port))
            .map_err(|error| AuthError::Message(error.to_string()))?;
        let port = listener
            .local_addr()
            .map_err(|error| AuthError::Message(error.to_string()))?
            .port();
        let redirect_host = options.redirect_host.unwrap_or(options.host);
        let host = if redirect_host.contains(':') {
            format!("[{redirect_host}]")
        } else {
            redirect_host.to_string()
        };
        let redirect_uri = format!("http://{host}:{port}{}", options.path);
        let (sender, receiver) = oneshot::channel();
        let provider_name = options.provider_name.to_string();
        let path = options.path.to_string();
        let state = options.state.map(str::to_string);
        let cancel = options.cancel.clone();
        let timeout = options.timeout.unwrap_or(Duration::from_mins(15));
        std::thread::spawn(move || {
            serve(
                &listener,
                &provider_name,
                &path,
                state.as_deref(),
                &cancel,
                timeout,
                sender,
            );
        });
        Ok(Self {
            redirect_uri,
            cancel: options.cancel.clone(),
            outcome: Some(receiver),
        })
    }

    /// The bound redirect URI (`:137-138`).
    #[must_use]
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// `wait()` (`:139`): `None` after `cancel()`.
    ///
    /// # Errors
    /// Provider `error` redirects, `complete` failures, timeouts and listener
    /// errors.
    pub async fn wait(&mut self) -> Result<Option<String>, AuthError> {
        let Some(receiver) = self.outcome.take() else {
            return Ok(None);
        };
        tokio::select! {
            outcome = receiver => outcome.unwrap_or(Ok(None)),
            () = self.cancel.cancelled() => Ok(None),
        }
    }

    /// `cancel()` (`:140-142`).
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// The token `cancel()` flips, so a racing manual prompt can stop the
    /// server without borrowing it across a `tokio::select!` arm.
    #[must_use]
    pub fn cancellation(&self) -> CancelToken {
        self.cancel.clone()
    }

    /// `close()` (`:143-146`).
    pub fn close(&self) {
        self.cancel();
    }
}

impl Drop for CallbackServer {
    fn drop(&mut self) {
        self.cancel();
    }
}

enum CallbackStep {
    Continue,
    Finish(Result<Option<String>, AuthError>),
}

fn serve(
    listener: &TcpListener,
    provider_name: &str,
    path: &str,
    state: Option<&str>,
    cancel: &CancelToken,
    timeout: Duration,
    sender: oneshot::Sender<Result<Option<String>, AuthError>>,
) {
    if let Err(error) = listener.set_nonblocking(true) {
        let _ = sender.send(Err(AuthError::Message(error.to_string())));
        return;
    }
    let deadline = Instant::now() + timeout;
    loop {
        if cancel.is_cancelled() {
            let _ = sender.send(Ok(None));
            return;
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let (status, page, step) =
                    handle_connection(&mut stream, provider_name, path, state);
                write_response(&mut stream, status, &page);
                if let CallbackStep::Finish(result) = step {
                    let _ = sender.send(result);
                    return;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                let _ = sender.send(Err(AuthError::Message(error.to_string())));
                return;
            }
        }
        if Instant::now() >= deadline {
            let _ = sender.send(Err(AuthError::Message(format!(
                "{provider_name} sign-in timed out"
            ))));
            return;
        }
    }
}

fn handle_connection(
    stream: &mut std::net::TcpStream,
    provider_name: &str,
    path: &str,
    expected_state: Option<&str>,
) -> (u16, String, CallbackStep) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => {
                return (
                    400,
                    oauth_error_html("Malformed callback request.", None),
                    CallbackStep::Continue,
                );
            }
            Ok(read) => {
                buffer.extend_from_slice(&chunk[..read]);
                if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
                if buffer.len() > 64 * 1024 {
                    return (
                        400,
                        oauth_error_html("Malformed callback request.", None),
                        CallbackStep::Continue,
                    );
                }
            }
        }
    }
    let head = String::from_utf8_lossy(&buffer);
    let request_line = head.lines().next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or("/");
    handle_request(method, target, provider_name, path, expected_state)
}

fn handle_request(
    method: &str,
    target: &str,
    provider_name: &str,
    path: &str,
    expected_state: Option<&str>,
) -> (u16, String, CallbackStep) {
    if method != "GET" || target.split('?').next().is_none_or(|value| value != path) {
        return (
            404,
            oauth_error_html("Callback route not found.", None),
            CallbackStep::Continue,
        );
    }
    let query = parse_query(target.split_once('?').map_or("", |(_, query)| query));
    if expected_state.is_some_and(|state| query.get("state").map(String::as_str) != Some(state)) {
        return (
            400,
            oauth_error_html("State mismatch.", None),
            CallbackStep::Continue,
        );
    }
    if let Some(error) = query.get("error") {
        let description = query
            .get("error_description")
            .map_or(error.clone(), Clone::clone);
        return (
            400,
            oauth_error_html(
                &format!("{provider_name} authorization failed."),
                Some(&description),
            ),
            CallbackStep::Finish(Err(AuthError::Message(format!(
                "{provider_name} authorization failed: {description}"
            )))),
        );
    }
    let Some(code) = query.get("code") else {
        return (
            400,
            oauth_error_html("Missing authorization code.", None),
            CallbackStep::Continue,
        );
    };
    (
        200,
        oauth_success_html(&format!(
            "Signed in to {provider_name}. You may now close this page."
        )),
        CallbackStep::Finish(Ok(Some(code.clone()))),
    )
}

fn write_response(stream: &mut std::net::TcpStream, status: u16, page: &str) {
    let reason = match status {
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        502 => "Bad Gateway",
        _ => "OK",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: text/html; charset=utf-8\r\ncache-control: no-store\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        page.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(page.as_bytes());
    let _ = stream.flush();
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// `oauthSuccessHtml` (`utils/oauth-page.ts:83-89`).
#[must_use]
pub fn oauth_success_html(message: &str) -> String {
    render_page(
        "Authentication successful",
        "Authentication successful",
        message,
        None,
    )
}

/// `oauthErrorHtml` (`utils/oauth-page.ts:91-99`).
#[must_use]
pub fn oauth_error_html(message: &str, details: Option<&str>) -> String {
    render_page(
        "Authentication failed",
        "Authentication failed",
        message,
        details,
    )
}

fn render_page(title: &str, heading: &str, message: &str, details: Option<&str>) -> String {
    let details_line = details.map_or_else(String::new, |details| {
        format!("<div class=\"details\">{}</div>", escape_html(details))
    });
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\" /><title>{}</title></head><body><main><h1>{}</h1><p>{}</p>{}</main></body></html>",
        escape_html(title),
        escape_html(heading),
        escape_html(message),
        details_line
    )
}

/// `waitForCallbackOrManualInput` (`callback-server.ts:155-183`): the browser
/// callback wins over a pasted code, and a cancelled callback falls through
/// to the manual prompt.
///
/// # Errors
/// A provider redirect error, a timeout, or a cancelled/interrupted prompt.
pub async fn wait_for_callback_or_manual_input(
    interaction: &Interaction,
    callback: &mut Option<CallbackServer>,
    prompt: &ManualPrompt,
) -> Result<CallbackResult, AuthError> {
    let manual_cancel = CancelToken::new();
    let callback_cancel = callback.as_ref().map(CallbackServer::cancellation);
    let manual_prompt = AuthPrompt::ManualCode {
        message: prompt.message.clone(),
        placeholder: Some(prompt.placeholder.clone()),
    };
    // The manual prompt must stay mounted while the callback is awaited:
    // a user answer during the wait cancels the callback and falls through
    // to the manual result, and a callback that yields none must still be
    // able to await the same prompt (`callback-server.ts:160-183` — the
    // manual promise is created before the callback wait and only aborted
    // in the `finally`). The pinned future is polled by reference, so no
    // select arm can take it away from the other.
    let manual = interaction.prompt_with(&manual_prompt, &manual_cancel);
    tokio::pin!(manual);
    let callback_wait = async {
        match callback.as_mut() {
            Some(server) => server.wait().await,
            None => Ok(None),
        }
    };
    tokio::select! {
        biased;
        outcome = callback_wait => {
            match outcome? {
                Some(code) => {
                    manual_cancel.cancel();
                    Ok(CallbackResult::Callback(code))
                }
                None => Ok(CallbackResult::Manual(manual.await?)),
            }
        }
        answer = &mut manual => {
            if let Some(token) = callback_cancel {
                token.cancel();
            }
            Ok(CallbackResult::Manual(answer?))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::InteractionMessage;
    use std::net::TcpStream;
    use std::sync::Arc;

    fn callback_get(uri: &str, query: &str) -> String {
        let url = uri.strip_prefix("http://").expect("http uri");
        let (authority, path) = url.split_once('/').map_or((url, ""), |(a, p)| (a, p));
        let mut stream = TcpStream::connect(authority).expect("connect callback");
        let request = format!("GET /{path}?{query} HTTP/1.1\r\nHost: {authority}\r\n\r\n");
        stream.write_all(request.as_bytes()).expect("send callback");
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        response
    }

    fn server(state: Option<&str>, timeout: Duration) -> CallbackServer {
        CallbackServer::start(&CallbackOptions {
            provider_name: "Test",
            host: "127.0.0.1",
            port: 0,
            path: "/callback",
            redirect_host: Some("localhost"),
            state,
            cancel: CancelToken::new(),
            timeout: Some(timeout),
        })
        .expect("callback server")
    }

    #[tokio::test]
    async fn callback_with_matching_state_returns_the_code() {
        let mut server = server(Some("verifier"), Duration::from_secs(5));
        assert!(server.redirect_uri().contains("/callback"));
        let response = callback_get(server.redirect_uri(), "code=abc&state=verifier");
        assert!(response.contains("Signed in to Test"), "{response}");
        assert_eq!(server.wait().await.expect("wait"), Some("abc".to_string()));
    }

    #[tokio::test]
    async fn state_mismatch_is_rejected_and_wait_continues() {
        let mut server = server(Some("verifier"), Duration::from_secs(5));
        let response = callback_get(server.redirect_uri(), "code=abc&state=wrong");
        assert!(response.contains("State mismatch."), "{response}");
        callback_get(server.redirect_uri(), "code=def&state=verifier");
        assert_eq!(server.wait().await.expect("wait"), Some("def".to_string()));
    }

    #[tokio::test]
    async fn authorization_error_is_reported_verbatim() {
        let mut server = server(None, Duration::from_secs(5));
        callback_get(
            server.redirect_uri(),
            "error=access_denied&error_description=nope",
        );
        assert_eq!(
            server.wait().await.unwrap_err().to_string(),
            "Test authorization failed: nope"
        );
    }

    #[tokio::test]
    async fn cancel_resolves_wait_with_none() {
        let mut server = server(None, Duration::from_secs(5));
        server.cancel();
        assert_eq!(server.wait().await.expect("wait"), None);
    }

    #[tokio::test]
    async fn timeout_uses_the_provider_name() {
        let mut server = server(None, Duration::from_millis(50));
        assert_eq!(
            server.wait().await.unwrap_err().to_string(),
            "Test sign-in timed out"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn manual_input_wins_when_the_callback_is_cancelled() {
        let (interaction, receiver) = Interaction::new();
        let interaction = Arc::new(interaction);
        let mut callback = Some(server(None, Duration::from_secs(5)));
        let prompt = ManualPrompt {
            message: "paste code".to_string(),
            placeholder: "http://localhost/callback".to_string(),
        };
        let task = {
            let interaction = Arc::clone(&interaction);
            tokio::spawn(async move {
                wait_for_callback_or_manual_input(&interaction, &mut callback, &prompt).await
            })
        };
        let InteractionMessage::Prompt { id, .. } = receiver.recv().expect("prompt") else {
            panic!("manual prompt expected");
        };
        interaction.answer(id, Ok("pasted".to_string()));
        assert_eq!(
            task.await.expect("flow"),
            Ok(CallbackResult::Manual("pasted".to_string()))
        );
    }

    #[test]
    fn pages_escape_details() {
        assert!(oauth_error_html("failed", Some("<script>")).contains("&lt;script&gt;"));
    }
}
