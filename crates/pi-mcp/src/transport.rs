//! Stdio transport: one MCP server subprocess, newline-delimited JSON-RPC on
//! its stdin/stdout, request ids correlated through a pending map.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::error::McpError;
use crate::types::{
    InboundMessage, JsonRpcId, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
    JsonRpcVersion, classify_line,
};

/// Awaiting responses: id -> one-shot reply channel. Keyed by [`JsonRpcId`] so
/// string-id replies correlate too (upstream `JsonRpcId`, `protocol/jsonrpc.ts:1`).
type PendingMap = Arc<Mutex<HashMap<JsonRpcId, oneshot::Sender<crate::types::JsonRpcResponse>>>>;

/// Live connection to one server process.
pub struct StdioTransport {
    /// Requests for the writer task, serialized on the server's stdin.
    outgoing: mpsc::Sender<Outgoing>,
    pending: PendingMap,
    /// Server-initiated notifications (and malformed lines, for diagnostics).
    /// Guarded because the host may drain it by reference
    /// ([`crate::client::McpClient::try_recv_inbound`]).
    pub inbound: std::sync::Mutex<mpsc::Receiver<InboundMessage>>,
    next_id: Arc<Mutex<i64>>,
    child: Arc<Mutex<Child>>,
    reader_task: JoinHandle<()>,
    writer_task: JoinHandle<()>,
}

enum Outgoing {
    Request(JsonRpcRequest),
    Notification(JsonRpcNotification),
    /// A reply to a server-initiated request (`roots/list`, `ping`).
    Response(JsonRpcResponse),
    /// Tear the transport down: kill the child.
    Close,
}

/// Handles for a spawned transport.
pub struct SpawnOptions {
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub working_dir: Option<std::path::PathBuf>,
}

impl StdioTransport {
    /// Spawns the server process and starts the reader/writer tasks.
    ///
    /// # Errors
    /// When the server binary cannot be spawned.
    pub fn spawn(options: &SpawnOptions) -> Result<Self, McpError> {
        let mut command = Command::new(&options.command);
        command
            .args(&options.args)
            .envs(&options.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if let Some(dir) = &options.working_dir {
            command.current_dir(dir);
        }
        let mut child = command.spawn().map_err(|source| McpError::Spawn {
            command: options.command.clone(),
            source,
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::ConnectionClosed("server stdin unavailable".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::ConnectionClosed("server stdout unavailable".to_string()))?;

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<Outgoing>(64);
        let (inbound_tx, inbound_rx) = mpsc::channel::<InboundMessage>(256);

        let reader_task = tokio::spawn(reader_loop(
            BufReader::new(stdout),
            pending.clone(),
            inbound_tx,
        ));
        let writer_task = tokio::spawn(writer_loop(stdin, outgoing_rx));

        Ok(Self {
            outgoing: outgoing_tx,
            pending,
            inbound: std::sync::Mutex::new(inbound_rx),
            next_id: Arc::new(Mutex::new(1)),
            child: Arc::new(Mutex::new(child)),
            reader_task,
            writer_task,
        })
    }

    /// Sends a request and waits for its response.
    ///
    /// # Errors
    /// On send failure, malformed replies, RPC errors, or timeout.
    pub async fn request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
        timeout: Duration,
    ) -> Result<serde_json::Value, McpError> {
        let id = {
            let mut next = self.next_id.lock().await;
            let id = *next;
            *next += 1;
            id
        };
        let rpc_id = JsonRpcId::Number(id);
        let request = JsonRpcRequest {
            jsonrpc: JsonRpcVersion,
            id: rpc_id.clone(),
            method: method.to_string(),
            params,
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        self.pending.lock().await.insert(rpc_id.clone(), reply_tx);
        let sent = self
            .outgoing
            .send(Outgoing::Request(request))
            .await
            .map_err(|_| McpError::ConnectionClosed("writer task gone".to_string()));
        if let Err(error) = sent {
            self.pending.lock().await.remove(&rpc_id);
            return Err(error);
        }

        let response =
            tokio::time::timeout(timeout, reply_rx)
                .await
                .map_err(|_| McpError::Timeout {
                    method: method.to_string(),
                    timeout_secs: timeout.as_secs(),
                })?;
        let response = response.map_err(|_| {
            McpError::ConnectionClosed(format!("request {id} dropped before reply"))
        })?;
        if let Some(error) = response.error {
            return Err(McpError::Rpc {
                code: error.code,
                message: error.message,
            });
        }
        response.result.ok_or_else(|| {
            McpError::Malformed(format!("response {id} carried neither result nor error"))
        })
    }

    /// Sends a notification (no reply expected).
    ///
    /// # Errors
    /// When the writer task is gone.
    pub async fn notify(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<(), McpError> {
        let notification = JsonRpcNotification {
            jsonrpc: JsonRpcVersion,
            method: method.to_string(),
            params,
        };
        self.outgoing
            .send(Outgoing::Notification(notification))
            .await
            .map_err(|_| McpError::ConnectionClosed("writer task gone".to_string()))
    }

    /// Replies to a request the server sent (upstream's `requestHandlers`
    /// in `client.ts:176-182`, `:510-520`).
    ///
    /// # Errors
    /// When the writer task is gone.
    pub async fn respond(&self, response: JsonRpcResponse) -> Result<(), McpError> {
        self.outgoing
            .send(Outgoing::Response(response))
            .await
            .map_err(|_| McpError::ConnectionClosed("writer task gone".to_string()))
    }

    /// Kills the server and joins the tasks. Draining `inbound` afterwards
    /// still yields any lines the reader saw before shutdown.
    ///
    /// # Errors
    /// Propagates join failures of the transport tasks.
    pub async fn shutdown(self) -> Result<(), McpError> {
        let _ = self.outgoing.send(Outgoing::Close).await;
        let mut child = self.child.lock().await;
        let _ = child.kill().await;
        drop(child);
        self.reader_task.abort();
        self.writer_task.abort();
        Ok(())
    }
}

async fn reader_loop(
    mut reader: BufReader<tokio::process::ChildStdout>,
    pending: PendingMap,
    inbound: mpsc::Sender<InboundMessage>,
) {
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => {
                // EOF: the server is gone. Fail every pending request.
                for (_, reply) in pending.lock().await.drain() {
                    let _ = reply.send(crate::types::JsonRpcResponse {
                        jsonrpc: JsonRpcVersion,
                        id: JsonRpcId::Number(0),
                        result: None,
                        error: Some(crate::types::JsonRpcError {
                            code: -32000,
                            message: "server closed the connection".to_string(),
                            data: None,
                        }),
                    });
                }
                let _ = inbound
                    .send(InboundMessage::Malformed("<<eof>>".to_string()))
                    .await;
                break;
            }
            Ok(_) => {
                let trimmed = line.trim_end();
                if trimmed.is_empty() {
                    continue;
                }
                match classify_line(trimmed) {
                    InboundMessage::Response(response) => {
                        let reply = pending.lock().await.remove(&response.id);
                        if let Some(reply) = reply {
                            let _ = reply.send(response);
                        }
                    }
                    InboundMessage::Malformed(_) => {
                        // Only surface malformed data that is not an empty
                        // heartbeat; everything else goes to diagnostics.
                    }
                    classified => {
                        let _ = inbound.send(classified).await;
                    }
                }
            }
            Err(_) => break,
        }
    }
}

async fn writer_loop(mut stdin: ChildStdin, mut outgoing: mpsc::Receiver<Outgoing>) {
    while let Some(outgoing) = outgoing.recv().await {
        let line = match &outgoing {
            Outgoing::Request(request) => serde_json::to_string(request),
            Outgoing::Notification(notification) => serde_json::to_string(notification),
            Outgoing::Response(response) => serde_json::to_string(response),
            Outgoing::Close => break,
        };
        let Ok(line) = line else { break };
        if stdin
            .write_all(format!("{line}\n").as_bytes())
            .await
            .is_err()
        {
            break;
        }
        if stdin.flush().await.is_err() {
            break;
        }
    }
}
