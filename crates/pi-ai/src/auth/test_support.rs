//! Synthetic HTTP endpoints for the OAuth flow tests (no external network).
//!
//! The tests in this module drive the same loopback request/response path the
//! provider flows use; every token and device-code response is canned here.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

type Handler = dyn Fn(&str) -> (u16, String) + Send + Sync;
type HeaderHandler = dyn Fn(&str) -> (u16, Vec<(String, String)>, String) + Send + Sync;

/// The handler receives the full request text (request line, headers and,
/// when present in the first read, the body).
///
/// A tiny HTTP server: one canned response per request line.
pub struct FakeServer {
    base: String,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<String>>>,
    thread: Option<JoinHandle<()>>,
}

impl FakeServer {
    /// Bind a loopback listener and serve until the server is dropped.
    #[must_use]
    pub fn start(handler: impl Fn(&str) -> (u16, String) + Send + Sync + 'static) -> Self {
        Self::start_with_headers(move |request| {
            let (status, body) = handler(request);
            (status, Vec::new(), body)
        })
    }

    /// [`FakeServer::start`] with explicit response headers, so a test can
    /// drive `Retry-After`-sensitive flows (e.g. Copilot's 429 back-off).
    #[must_use]
    pub fn start_with_headers(
        handler: impl Fn(&str) -> (u16, Vec<(String, String)>, String) + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake server");
        let address = listener.local_addr().expect("fake server address");
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let handler: Arc<HeaderHandler> = Arc::new(handler);
        let thread_stop = Arc::clone(&stop);
        let thread_requests = Arc::clone(&requests);
        let thread = std::thread::spawn(move || {
            let _ = listener.set_nonblocking(true);
            while !thread_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                        let mut buffer = Vec::new();
                        let mut chunk = [0_u8; 4096];
                        let request = loop {
                            match stream.read(&mut chunk) {
                                Ok(0) | Err(_) => break None,
                                Ok(read) => {
                                    buffer.extend_from_slice(&chunk[..read]);
                                    if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                                        break Some(String::from_utf8_lossy(&buffer).into_owned());
                                    }
                                }
                            }
                        };
                        let Some(request) = request else {
                            continue;
                        };
                        let line = request.lines().next().unwrap_or_default().to_string();
                        thread_requests
                            .lock()
                            .expect("fake server requests")
                            .push(line.clone());
                        let (status, headers, body) = handler(&request);
                        let reason = match status {
                            200 => "OK",
                            429 => "Too Many Requests",
                            _ => "Bad Request",
                        };
                        let extra = headers
                            .iter()
                            .map(|(name, value)| format!("{name}: {value}\r\n"))
                            .collect::<String>();
                        let head = format!(
                            "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(head.as_bytes());
                        let _ = stream.write_all(body.as_bytes());
                        let _ = stream.flush();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            base: format!("http://{address}"),
            stop,
            requests,
            thread: Some(thread),
        }
    }

    /// Absolute URL for a path on the fake server.
    #[must_use]
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// Request lines observed so far, in order.
    #[must_use]
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("fake server requests").clone()
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
