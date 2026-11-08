//! Server-Sent Events parsing over a reqwest byte stream.
//!
//! Mirrors the semantics TS relies on via `iterateSseMessages`: fields
//! `event`, `data` (multi-line joined with `\n`), `id`, `retry`;
//! `:`-prefixed comments ignored; blank line dispatches the message.

use std::collections::{HashMap, VecDeque};

/// One parsed SSE message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseMessage {
    /// `event:` field, if present.
    pub event: Option<String>,
    /// `data:` field: multi-line values joined with `\n`.
    pub data: String,
    /// All other fields (e.g. `id`), lowercased keys.
    pub fields: HashMap<String, String>,
}

/// Errors from streaming an SSE response body.
#[derive(Debug, thiserror::Error)]
pub enum SseError {
    /// The HTTP layer failed.
    #[error("http error: {0}")]
    Http(String),
    /// The HTTP response was an error status; carries the status code.
    #[error("http status {status}")]
    Status {
        status: u16,
        /// Response body excerpt for diagnostics.
        body: String,
    },
}

/// Incremental SSE parser: feed bytes, get messages on blank lines. Follows
/// the WHATWG SSE algorithm for the line endings and fields the providers pi
/// supports use (`OpenAI`, Anthropic, llama.cpp, vLLM, …).
#[derive(Debug, Default)]
pub struct SseParser {
    event: Option<String>,
    data: Vec<String>,
    fields: HashMap<String, String>,
    buffer: Vec<u8>,
}

impl SseParser {
    /// Feed raw bytes; returns messages completed by this chunk.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseMessage> {
        self.buffer.extend_from_slice(chunk);
        let mut messages = Vec::new();
        let mut consumed = 0usize;
        let mut i = 0usize;
        while i < self.buffer.len() {
            let (line_end, next) = if self.buffer[i] == b'\n' {
                (i, i + 1)
            } else if self.buffer[i] == b'\r' {
                if i + 1 < self.buffer.len() && self.buffer[i + 1] == b'\n' {
                    (i, i + 2)
                } else {
                    (i, i + 1)
                }
            } else {
                i += 1;
                continue;
            };
            let line = self.buffer[consumed..line_end].to_vec();
            if let Some(msg) = self.handle_line(&line) {
                messages.push(msg);
            }
            consumed = next;
            i = next;
        }
        self.buffer.drain(..consumed);
        messages
    }

    /// Flush at end of stream: process a final line without newline, then
    /// dispatch if a message is open.
    pub fn finish(&mut self) -> Vec<SseMessage> {
        let mut messages = Vec::new();
        if !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            if let Some(msg) = self.handle_line(&line) {
                messages.push(msg);
            }
        }
        if let Some(msg) = self.dispatch() {
            messages.push(msg);
        }
        messages
    }

    /// Process one line; returns a message when the line was blank and a
    /// message was open.
    fn handle_line(&mut self, line: &[u8]) -> Option<SseMessage> {
        if line.is_empty() {
            return self.dispatch();
        }
        // Comments start with a colon.
        if line[0] == b':' {
            return None;
        }
        let line_str = String::from_utf8_lossy(line);
        let (field, value) = match line_str.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line_str.as_ref(), ""),
        };
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => self.data.push(value.to_string()),
            _ => {
                self.fields.insert(field.to_string(), value.to_string());
            }
        }
        None
    }

    fn dispatch(&mut self) -> Option<SseMessage> {
        if self.event.is_none() && self.data.is_empty() && self.fields.is_empty() {
            return None;
        }
        let msg = SseMessage {
            event: self.event.take(),
            data: self.data.join("\n"),
            fields: std::mem::take(&mut self.fields),
        };
        self.data.clear();
        Some(msg)
    }
}

/// Read an SSE stream from an HTTP response as an async stream of messages.
/// The status check happens before any event is produced, so callers can emit
/// their `start` event only after setup succeeds. The `[DONE]` sentinel is
/// left to the caller (OpenAI-specific).
pub async fn read_sse_stream(
    response: reqwest::Response,
) -> Result<impl futures::Stream<Item = Result<SseMessage, SseError>> + Send, SseError> {
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        let body: String = body.chars().take(2048).collect();
        return Err(SseError::Status {
            status: status.as_u16(),
            body,
        });
    }

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<SseMessage, SseError>>(64);

    tokio::spawn(async move {
        let mut byte_stream = response.bytes_stream();
        let mut parser = SseParser::default();
        let mut pending: VecDeque<SseMessage> = VecDeque::new();

        loop {
            if let Some(msg) = pending.pop_front() {
                if tx.send(Ok(msg)).await.is_err() {
                    return;
                }
                continue;
            }
            match futures::StreamExt::next(&mut byte_stream).await {
                Some(Ok(chunk)) => {
                    pending.extend(parser.feed(&chunk));
                }
                Some(Err(e)) => {
                    let _ = tx.send(Err(SseError::Http(e.to_string()))).await;
                    return;
                }
                None => {
                    for msg in parser.finish() {
                        if tx.send(Ok(msg)).await.is_err() {
                            return;
                        }
                    }
                    return;
                }
            }
        }
    });

    Ok(tokio_stream::wrappers::ReceiverStream::new(rx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_flow() {
        let mut p = SseParser::default();
        let msgs = p.feed(b"event: message_start\r\ndata: {\"a\":1}\r\n\r\n");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].event.as_deref(), Some("message_start"));
        assert_eq!(msgs[0].data, "{\"a\":1}");

        let msgs = p.feed(b": comment\n\ndata: line1\ndata: line2\n\n");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].data, "line1\nline2");
        assert_eq!(msgs[0].event, None);
    }

    #[test]
    fn handles_chunks_split_mid_line() {
        let mut p = SseParser::default();
        assert_eq!(p.feed(b"data: {\"x\":"), []);
        assert_eq!(p.feed(b"1}\n\n").len(), 1);
        assert_eq!(p.finish(), []);
    }

    #[test]
    fn finish_flushes_final_line() {
        let mut p = SseParser::default();
        assert_eq!(p.feed(b"data: tail"), []);
        let msgs = p.finish();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].data, "tail");
    }

    #[tokio::test]
    async fn reads_stream_response() {
        // Feed the parser directly to emulate a response body.
        let mut p = SseParser::default();
        let mut all = Vec::new();
        all.extend(p.feed(b"event: a\ndata: 1\n\n"));
        all.extend(p.feed(b"data: 2\n"));
        all.extend(p.feed(b"\nevent: b\ndata: 3\n"));
        all.extend(p.finish());
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].event.as_deref(), Some("a"));
        assert_eq!(all[1].data, "2");
        assert_eq!(all[2].event.as_deref(), Some("b"));
    }
}
