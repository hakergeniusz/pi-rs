//! P1 exit criterion: stream a completion through both API shapes.
//!
//! Mock SSE servers exercise the full HTTP + SSE + protocol path offline.
//! The live test (ignored by default) runs against the local llama-swap
//! proxy (OpenAI-compatible) with a local model — never a paid endpoint.

use pi_ai::types::{
    AssistantMessageEvent, InputContent, Message, Model, ModelCost, ModelInputModality, StopReason,
    StreamOptions, StringOrTextBlocks, SystemMessage, TranscriptContext, UserContent, UserMessage,
};
use std::io::{Read, Write};

/// Serve one canned HTTP response on an ephemeral port, then stop.
fn mock_http_server(response: &'static [u8]) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        for stream in listener.incoming().take(1) {
            let Ok(mut sock) = stream else { return };
            // Read the request head (and body if any) before responding.
            let mut buf = [0u8; 8192];
            let mut received = Vec::new();
            loop {
                match sock.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        received.extend_from_slice(&buf[..n]);
                        let head_end = received.windows(4).any(|w| w == b"\r\n\r\n");
                        if head_end {
                            // Minimal: no content-length parsing needed for tests.
                            break;
                        }
                    }
                }
            }
            let _ = sock.write_all(response);
            let _ = sock.flush();
            // Keep the socket open briefly so the client can read the body.
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    });
    format!("http://{addr}")
}

fn sse_response(body: &'static str) -> &'static [u8] {
    // Leaked to keep &'static lifetime for the helper; test-only.
    Box::leak(
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}"
        )
        .into_boxed_str()
        .into_boxed_bytes(),
    )
}

fn error_response(status: &'static str, body: &'static str) -> &'static [u8] {
    Box::leak(
        format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{body}"
        )
        .into_boxed_str()
        .into_boxed_bytes(),
    )
}

fn model(api: &str, base_url: String) -> Model {
    Model {
        id: "test-model".into(),
        name: "Test Model".into(),
        api: api.into(),
        provider: "test".into(),
        base_url,
        input: vec![ModelInputModality::Text],
        input_limits: None,
        cost: ModelCost {
            input: 1.0,
            output: 2.0,
            cache_read: 0.1,
            cache_write: 0.0,
            ..Default::default()
        },
        headers: None,
        r#type: None,
        reasoning: false,
        thinking_level_map: None,
        prompt_cache: None,
        context_window: 100_000,
        max_tokens: 4096,
        sampling_params: None,
        sampling_params_by_thinking_level: None,
        compat: None,
    }
}

fn context(prompt: &str) -> TranscriptContext {
    TranscriptContext {
        messages: vec![
            Message::System(SystemMessage {
                content: StringOrTextBlocks::Text("You are a test.".into()),
                sections: None,
                tools_added: None,
                tools_removed: None,
                timestamp: 1,
            }),
            Message::User(UserMessage {
                content: UserContent::Text(prompt.into()),
                timestamp: 2,
            }),
        ],
    }
}

async fn collect(mut stream: pi_ai::AssistantMessageEventStream) -> Vec<AssistantMessageEvent> {
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    events
}

const ANTHROPIC_SSE: &str = concat!(
    "event: message_start\n",
    r#"data: {"type":"message_start","message":{"id":"msg_1","model":"test-model","usage":{"input_tokens":12,"output_tokens":0,"cache_read_input_tokens":3,"cache_creation_input_tokens":2}}}"#,
    "\n\n",
    "event: content_block_start\n",
    r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}"#,
    "\n\n",
    "event: content_block_stop\n",
    r#"data: {"type":"content_block_stop","index":0}"#,
    "\n\n",
    "event: content_block_start\n",
    r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"bash","input":{}}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":"}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"ls\""}}"#,
    "\n\n",
    "event: content_block_stop\n",
    r#"data: {"type":"content_block_stop","index":1}"#,
    "\n\n",
    "event: message_delta\n",
    r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":23}}"#,
    "\n\n",
    "event: message_stop\n",
    r#"data: {"type":"message_stop"}"#,
    "\n\n",
);

#[tokio::test]
async fn anthropic_streams_text_and_tool_use() {
    let base_url = mock_http_server(sse_response(ANTHROPIC_SSE));
    let model = model("anthropic-messages", base_url);
    let options = StreamOptions {
        api_key: Some("test-key".into()),
        ..Default::default()
    };

    let events = collect(pi_ai::providers::anthropic::stream(
        &model,
        &context("say hi"),
        &options,
    ))
    .await;

    let mut kinds = Vec::new();
    let mut deltas = Vec::new();
    for event in &events {
        kinds.push(match event {
            AssistantMessageEvent::Start { .. } => "start",
            AssistantMessageEvent::TextStart { .. } => "text_start",
            AssistantMessageEvent::TextDelta { .. } => "text_delta",
            AssistantMessageEvent::TextEnd { .. } => "text_end",
            AssistantMessageEvent::ThinkingStart { .. } => "thinking_start",
            AssistantMessageEvent::ThinkingDelta { .. } => "thinking_delta",
            AssistantMessageEvent::ThinkingEnd { .. } => "thinking_end",
            AssistantMessageEvent::ToolcallStart { .. } => "toolcall_start",
            AssistantMessageEvent::ToolcallDelta { .. } => "toolcall_delta",
            AssistantMessageEvent::ToolcallEnd { .. } => "toolcall_end",
            AssistantMessageEvent::Done { .. } => "done",
            AssistantMessageEvent::Error { .. } => "error",
        });
        if let AssistantMessageEvent::TextDelta { delta, .. } = event {
            deltas.push(delta.clone());
        }
    }
    assert_eq!(
        kinds,
        [
            "start",
            "text_start",
            "text_delta",
            "text_delta",
            "text_end",
            "toolcall_start",
            "toolcall_delta",
            "toolcall_delta",
            "toolcall_end",
            "done"
        ],
        "unexpected event sequence: {kinds:?}; last={:?}",
        events.last()
    );
    assert_eq!(deltas.join(""), "Hello");

    let Some(AssistantMessageEvent::Done { reason, message }) = events.last() else {
        panic!("expected done");
    };
    assert_eq!(*reason, StopReason::ToolUse);
    assert_eq!(message.response_id.as_deref(), Some("msg_1"));
    assert_eq!(message.usage.input, 12);
    assert_eq!(message.usage.output, 23);
    assert_eq!(message.usage.cache_read, 3);
    assert_eq!(message.usage.cache_write, 2);
    assert_eq!(message.usage.total_tokens, 12 + 23 + 3 + 2);
    // Catalog-priced cost: input 12 @ 1.0/M + output 23 @ 2.0/M + caches.
    assert!((message.usage.cost.total - (12.0 * 1.0 + 23.0 * 2.0 + 3.0 * 0.1) / 1e6).abs() < 1e-12);
    assert_eq!(message.content.len(), 2);
    assert_eq!(message.raw_stop_reason.as_deref(), Some("tool_use"));
    // Tool call arguments were assembled from two JSON deltas.
    if let pi_ai::types::AssistantContent::ToolCall(call) = &message.content[1] {
        assert_eq!(call.id, "toolu_1");
        assert_eq!(call.name, "bash");
        assert_eq!(
            call.arguments.get("cmd").and_then(|v| v.as_str()),
            Some("ls")
        );
    } else {
        panic!("expected tool call, got {:?}", message.content[1]);
    }
}

#[tokio::test]
async fn anthropic_error_status_becomes_error_event() {
    let base_url = mock_http_server(error_response(
        "500 Internal Server Error",
        r#"{"error":{"message":"boom"}}"#,
    ));
    let model = model("anthropic-messages", base_url);
    let options = StreamOptions {
        api_key: Some("test-key".into()),
        ..Default::default()
    };

    let events = collect(pi_ai::providers::anthropic::stream(
        &model,
        &context("hi"),
        &options,
    ))
    .await;
    assert_eq!(events.len(), 1);
    match &events[0] {
        AssistantMessageEvent::Error { reason, error } => {
            assert_eq!(*reason, StopReason::Error);
            assert!(
                error
                    .error_message
                    .as_deref()
                    .unwrap_or_default()
                    .contains("500")
            );
        }
        other => panic!("expected error event, got {other:?}"),
    }
}

const OPENAI_SSE: &str = concat!(
    r#"data: {"id":"chatcmpl_1","model":"test-model","choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":"thi"}}]}"#,
    "\n\n",
    r#"data: {"choices":[{"index":0,"delta":{"reasoning_content":"nking"}}]}"#,
    "\n\n",
    r#"data: {"choices":[{"index":0,"delta":{"content":"He"}}]}"#,
    "\n\n",
    r#"data: {"choices":[{"index":0,"delta":{"content":"llo"}}]}"#,
    "\n\n",
    r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read","arguments":"{\"pa"}}]}}]}"#,
    "\n\n",
    r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a.txt\"}"}}]}}]}"#,
    "\n\n",
    r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
    "\n\n",
    r#"data: {"id":"chatcmpl_1","model":"test-model","choices":[],"usage":{"prompt_tokens":9,"completion_tokens":7,"total_tokens":16,"prompt_tokens_details":{"cached_tokens":4},"completion_tokens_details":{"reasoning_tokens":5}}}"#,
    "\n\n",
    "data: [DONE]\n\n",
);

#[tokio::test]
async fn openai_streams_reasoning_text_and_tools() {
    let base_url = mock_http_server(sse_response(OPENAI_SSE));
    let model = model("openai-completions", base_url);
    let options = StreamOptions {
        api_key: Some("test-key".into()),
        ..Default::default()
    };

    let events = collect(pi_ai::providers::openai_completions::stream(
        &model,
        &context("hi"),
        &options,
    ))
    .await;

    let kinds: Vec<&str> = events
        .iter()
        .map(|event| match event {
            AssistantMessageEvent::Start { .. } => "start",
            AssistantMessageEvent::ThinkingStart { .. } => "thinking_start",
            AssistantMessageEvent::ThinkingDelta { .. } => "thinking_delta",
            AssistantMessageEvent::ThinkingEnd { .. } => "thinking_end",
            AssistantMessageEvent::TextStart { .. } => "text_start",
            AssistantMessageEvent::TextDelta { .. } => "text_delta",
            AssistantMessageEvent::TextEnd { .. } => "text_end",
            AssistantMessageEvent::ToolcallStart { .. } => "toolcall_start",
            AssistantMessageEvent::ToolcallDelta { .. } => "toolcall_delta",
            AssistantMessageEvent::ToolcallEnd { .. } => "toolcall_end",
            AssistantMessageEvent::Done { .. } => "done",
            AssistantMessageEvent::Error { .. } => "error",
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "start",
            "thinking_start",
            "thinking_delta",
            "thinking_delta",
            "text_start",
            "text_delta",
            "text_delta",
            "toolcall_start",
            "toolcall_delta",
            "toolcall_delta",
            "thinking_end",
            "text_end",
            "toolcall_end",
            "done"
        ],
        "unexpected sequence: {kinds:?}; last={:?}",
        events.last()
    );

    let Some(AssistantMessageEvent::Done { reason, message }) = events.last() else {
        panic!("expected done");
    };
    assert_eq!(*reason, StopReason::ToolUse);
    assert_eq!(message.response_id.as_deref(), Some("chatcmpl_1"));
    assert_eq!(message.usage.input, 9);
    assert_eq!(message.usage.output, 7);
    assert_eq!(message.usage.cache_read, 4);
    assert_eq!(message.usage.reasoning, Some(5));
    assert_eq!(message.content.len(), 3);
    assert!(
        matches!(&message.content[0], pi_ai::types::AssistantContent::Thinking(t) if t.thinking == "thinking")
    );
    assert!(
        matches!(&message.content[1], pi_ai::types::AssistantContent::Text(t) if t.text == "Hello")
    );
    if let pi_ai::types::AssistantContent::ToolCall(call) = &message.content[2] {
        assert_eq!(call.name, "read");
        assert_eq!(
            call.arguments.get("path").and_then(|v| v.as_str()),
            Some("a.txt")
        );
    } else {
        panic!("expected tool call");
    }
}

#[tokio::test]
async fn openai_content_filter_is_error() {
    let body: &'static str = Box::leak(
        concat!(
            r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"content_filter"}]}"#,
            "\n\n",
            "data: [DONE]\n\n"
        )
        .to_string()
        .into_boxed_str(),
    );
    let base_url = mock_http_server(sse_response(body));
    let model = model("openai-completions", base_url);
    let options = StreamOptions {
        api_key: Some("k".into()),
        ..Default::default()
    };

    let events = collect(pi_ai::providers::openai_completions::stream(
        &model,
        &context("hi"),
        &options,
    ))
    .await;
    match events.last() {
        Some(AssistantMessageEvent::Error { reason, error }) => {
            assert_eq!(*reason, StopReason::Error);
            assert!(
                error
                    .error_message
                    .as_deref()
                    .unwrap_or_default()
                    .contains("content_filter")
            );
        }
        other => panic!("expected error, got {other:?}"),
    }
}

/// Live streaming through the local llama-swap proxy (OpenAI-compatible).
/// Ignored by default: run with `cargo test -p pi-ai -- --ignored`.
/// Uses only local models (see AGENTS rule: no paid endpoints for tests).
#[tokio::test]
#[ignore = "requires local llama-swap proxy on 127.0.0.1:11111"]
async fn live_openai_completions_via_llama_swap() {
    let base_url = "http://127.0.0.1:11111/v1".to_string();
    // Skip silently if the proxy is down.
    if reqwest::get(format!("{base_url}/models")).await.is_err() {
        eprintln!("llama-swap not reachable; skipping live test");
        return;
    }
    let mut model = model("openai-completions", base_url);
    model.id = "qwen3.8-4b".into();
    model.max_tokens = 256;
    let ctx = context("Reply with exactly the word: pong");
    let options = StreamOptions {
        api_key: Some("local".into()),
        temperature: Some(0.0),
        ..Default::default()
    };

    let events = collect(pi_ai::providers::openai_completions::stream(
        &model, &ctx, &options,
    ))
    .await;
    let Some(AssistantMessageEvent::Done { reason, message }) = events.last() else {
        panic!("expected done, got {events:?}");
    };
    assert!(
        !matches!(*reason, StopReason::Error | StopReason::Aborted),
        "{message:?}"
    );
    let text = message
        .content
        .iter()
        .filter_map(|c| match c {
            pi_ai::types::AssistantContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect::<String>();
    assert!(!text.is_empty(), "empty completion from live endpoint");
    eprintln!(
        "live completion: {text:?} (stop={reason:?}, usage={:?})",
        message.usage.total_tokens
    );
}

/// The user-content image path through the `OpenAI` wire format (no server):
/// request building only.
#[test]
fn openai_request_includes_data_uri_images() {
    let model = model("openai-completions", "http://127.0.0.1:1/v1".into());
    let context = TranscriptContext {
        messages: vec![Message::User(UserMessage {
            content: UserContent::Blocks(vec![
                InputContent::Text(pi_ai::types::TextContent {
                    text: "look".into(),
                    text_signature: None,
                }),
                InputContent::Image(pi_ai::types::ImageContent {
                    data: "aGk=".into(),
                    mime_type: "image/png".into(),
                }),
            ]),
            timestamp: 1,
        })],
    };
    let body = pi_ai::providers::openai_completions::build_request_body(
        &model,
        &context,
        &StreamOptions::default(),
    )
    .unwrap();
    let blocks = body["messages"][0]["content"].as_array().unwrap();
    assert_eq!(blocks[1]["type"], "image_url");
    assert_eq!(blocks[1]["image_url"]["url"], "data:image/png;base64,aGk=");
}
