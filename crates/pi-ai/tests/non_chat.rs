//! End-to-end wire tests for the non-chat APIs (classifier + image
//! generation). A local mock server exercises the real reqwest path without
//! contacting a paid endpoint, mirroring `tests/streaming.rs`.
//!
//! Upstream evidence: `packages/ai/test/openai-decisions.test.ts`,
//! `packages/ai/test/openrouter-images.test.ts`, and the request sequence in
//! `packages/ai/src/api/llama-cpp-classify.ts:216-371`.

use pi_ai::providers::classifier::{
    ClassifierBoolCriteria, ClassifierContext, ClassifierOptions, ClassifierQuestion,
    ClassifierResult, ClassifierStopReason, OrderedMap, classify,
};
use pi_ai::providers::images::{ImagesContext, ImagesOptions, ImagesStopReason, generate_images};
use pi_ai::types::{
    ClassifierModel, ImageContent, ImageModel, InputContent, ModelCost, ModelInputModality,
    ModelOutputModality, TextContent,
};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::sync::mpsc::{Receiver, Sender, channel};

/// One captured request: path, JSON body, Authorization header.
type CapturedRequest = (String, String, Option<String>);

/// Serve canned JSON on an ephemeral port, handing each request's path, body
/// and Authorization header to `handler`. Runs until the process exits.
fn serve(handler: impl Fn(&str, &str, Option<&str>) -> (u16, String) + Send + 'static) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut sock) = stream else { continue };
            let mut received = Vec::new();
            let mut buf = [0u8; 8192];
            let head_end = loop {
                match sock.read(&mut buf) {
                    Ok(0) | Err(_) => break None,
                    Ok(n) => {
                        received.extend_from_slice(&buf[..n]);
                        if let Some(pos) = received.windows(4).position(|w| w == b"\r\n\r\n") {
                            break Some(pos + 4);
                        }
                    }
                }
            };
            let Some(head_end) = head_end else { continue };
            let head = String::from_utf8_lossy(&received[..head_end - 4]).to_string();
            let content_length = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    if name.eq_ignore_ascii_case("content-length") {
                        value.trim().parse::<usize>().ok()
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            while received.len() < head_end + content_length {
                match sock.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => received.extend_from_slice(&buf[..n]),
                }
            }
            let body =
                String::from_utf8_lossy(&received[head_end..head_end + content_length]).to_string();
            let path = head
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("/")
                .to_string();
            let authorization = head.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("authorization")
                    .then(|| value.trim().to_string())
            });
            let (status, response_body) = handler(&path, &body, authorization.as_deref());
            let response = format!(
                "HTTP/1.1 {status} OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            let _ = sock.write_all(response.as_bytes());
            let _ = sock.flush();
        }
    });
    format!("http://{addr}")
}

fn decisions_model(base_url: String) -> ClassifierModel {
    ClassifierModel {
        id: "gpt-6-luna".into(),
        name: "GPT-6 Luna".into(),
        api: "openai-decisions".into(),
        provider: "openai".into(),
        base_url,
        input: vec![ModelInputModality::Text, ModelInputModality::Image],
        cost: ModelCost {
            input: 0.1,
            ..Default::default()
        },
        r#type: "classifier".into(),
        context_window: 922_000,
        ..Default::default()
    }
}

fn decisions_context() -> ClassifierContext {
    let mut questions = OrderedMap::new();
    questions.insert(
        "category".into(),
        ClassifierQuestion::Choice {
            instructions: "Classify the message".into(),
            criteria: OrderedMap::from_vec(vec![
                ("success".into(), "Successful".into()),
                ("failure".into(), String::new()),
            ]),
        },
    );
    questions.insert(
        "approved".into(),
        ClassifierQuestion::Bool {
            instructions: "Does the user approve?".into(),
            criteria: ClassifierBoolCriteria {
                true_meaning: "Approval".into(),
                false_meaning: "No approval".into(),
            },
        },
    );
    ClassifierContext {
        state: serde_json::from_str(r#"{"text":"The deployment succeeded, thank you."}"#).unwrap(),
        images: vec![ImageContent {
            data: "aW1hZ2U=".into(),
            mime_type: "image/png".into(),
        }],
        questions,
    }
}

fn decisions_response() -> String {
    json!({
        "answers": [
            {"type":"predicate","name":"approved","probability":0.95},
            {"type":"choice","name":"category","choice":"success",
             "probabilities":[{"value":"success","probability":0.9},{"value":"failure","probability":0.1}],
             "confidence":0.8}
        ],
        "usage": {"input_tokens":164,"output_tokens":0}
    })
    .to_string()
}

#[tokio::test]
async fn decisions_full_wire_path() {
    let (tx, rx): (Sender<CapturedRequest>, Receiver<CapturedRequest>) = channel();
    let base_url = serve(move |path, body, auth| {
        tx.send((path.to_string(), body.to_string(), auth.map(str::to_string)))
            .unwrap();
        (200, decisions_response())
    });
    let model = decisions_model(base_url);
    let result = classify(
        &model,
        &decisions_context(),
        &ClassifierOptions {
            api_key: Some("secret".into()),
            ..Default::default()
        },
    )
    .await;

    assert_eq!(result.stop_reason, ClassifierStopReason::Stop);
    assert_eq!(result.error_message, None);
    assert_eq!(
        result.answers.get("approved").unwrap(),
        &pi_ai::providers::classifier::ClassifierAnswer::Bool { probability: 0.95 }
    );
    let usage = result.usage.unwrap();
    assert_eq!(usage.input, 164);
    assert!((usage.cost.total - 0.000_016_4).abs() < 1e-12);

    let (path, body, auth) = rx.recv().unwrap();
    assert_eq!(path, "/decisions");
    assert_eq!(auth.as_deref(), Some("Bearer secret"));
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body.pointer("/model").unwrap().as_str(), Some("gpt-6-luna"));
    assert_eq!(
        body.pointer("/input/0/content/1/type").unwrap().as_str(),
        Some("input_image")
    );
    assert_eq!(
        body.pointer("/input/0/content/1/image_url")
            .unwrap()
            .as_str(),
        Some("data:image/png;base64,aW1hZ2U=")
    );
    assert_eq!(
        body.pointer("/questions/0/type").unwrap().as_str(),
        Some("choice")
    );
    assert_eq!(
        body.pointer("/questions/1/type").unwrap().as_str(),
        Some("predicate")
    );
}

#[tokio::test]
async fn decisions_does_not_retry_gateway_timeout() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let calls = std::sync::Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let base_url = serve(move |_path, _body, _auth| {
        counter.fetch_add(1, Ordering::SeqCst);
        (
            504,
            "<!DOCTYPE html><html>Gateway time-out</html>".to_string(),
        )
    });
    let result: ClassifierResult = classify(
        &decisions_model(base_url),
        &decisions_context(),
        &ClassifierOptions {
            api_key: Some("secret".into()),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Error);
    let message = result.error_message.unwrap();
    assert!(message.contains("OpenAI Decisions error (504): the request timed out at the gateway"));
    assert!(!message.contains("<html>"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

fn image_model(base_url: String, output: Vec<ModelOutputModality>) -> ImageModel {
    ImageModel {
        id: "google/gemini-2.5-flash-image".into(),
        name: "Gemini 2.5 Flash Image".into(),
        api: "openrouter-images".into(),
        provider: "openrouter".into(),
        base_url,
        input: vec![ModelInputModality::Text, ModelInputModality::Image],
        input_limits: None,
        cost: ModelCost {
            input: 0.015,
            output: 0.03,
            ..Default::default()
        },
        headers: None,
        r#type: "image".into(),
        output,
    }
}

#[tokio::test]
async fn images_full_wire_path() {
    let (tx, rx): (Sender<CapturedRequest>, Receiver<CapturedRequest>) = channel();
    let base_url = serve(move |path, body, auth| {
        tx.send((path.to_string(), body.to_string(), auth.map(str::to_string)))
            .unwrap();
        (
            200,
            json!({
                "id": "img-1",
                "usage": {"prompt_tokens":12,"completion_tokens":34,
                          "prompt_tokens_details":{"cached_tokens":0}},
                "choices": [{"message":{
                    "content": "Here is your image.",
                    "images": [{"image_url":"data:image/png;base64,ZmFrZS1wbmc="}]
                }}]
            })
            .to_string(),
        )
    });
    let context = ImagesContext {
        input: vec![InputContent::Text(TextContent {
            text: "Generate a dog".into(),
            text_signature: None,
        })],
    };
    let result = generate_images(
        &image_model(
            format!("{base_url}/api/v1"),
            vec![ModelOutputModality::Image, ModelOutputModality::Text],
        ),
        &context,
        &ImagesOptions {
            api_key: Some("test".into()),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(result.stop_reason, ImagesStopReason::Stop);
    assert_eq!(result.response_id.as_deref(), Some("img-1"));
    assert_eq!(
        result.output,
        vec![
            InputContent::Text(TextContent {
                text: "Here is your image.".into(),
                text_signature: None,
            }),
            InputContent::Image(ImageContent {
                data: "ZmFrZS1wbmc=".into(),
                mime_type: "image/png".into(),
            }),
        ]
    );
    assert_eq!(result.usage.unwrap().input, 12);

    let (path, body, auth) = rx.recv().unwrap();
    assert_eq!(path, "/api/v1/chat/completions");
    assert_eq!(auth.as_deref(), Some("Bearer test"));
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body.pointer("/stream").unwrap().as_bool(), Some(false));
    assert_eq!(
        body.pointer("/modalities/0").unwrap().as_str(),
        Some("image")
    );
    assert_eq!(
        body.pointer("/modalities/1").unwrap().as_str(),
        Some("text")
    );
    assert_eq!(
        body.pointer("/messages/0/content/0/text").unwrap().as_str(),
        Some("Generate a dog")
    );
}

#[tokio::test]
async fn llama_cpp_full_wire_path() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let completions = std::sync::Arc::new(AtomicUsize::new(0));
    let counter = completions.clone();
    let base_url = serve(move |path, body, _auth| {
        let body: Value = serde_json::from_str(body).unwrap();
        let response = match path {
            "/tokenize" => {
                let content = body
                    .pointer("/content")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let tokens = match content {
                    "\n" => vec![1],
                    "\nYes" => vec![1, 2],
                    "Yes" => vec![2],
                    "\nNo" => vec![1, 3],
                    "No" => vec![3],
                    _ => vec![],
                };
                json!({"tokens": tokens})
            }
            "/apply-template" => json!({"prompt": "rendered-prompt"}),
            "/completion" => {
                counter.fetch_add(1, Ordering::SeqCst);
                assert_eq!(body.pointer("/n_probs").unwrap().as_u64(), Some(256));
                json!({"completion_probabilities":[{"top_logprobs":[
                    {"id":2,"logprob":0.0},{"id":3,"logprob":-2.0}
                ]}]})
            }
            other => panic!("unexpected path {other}"),
        };
        (200, response.to_string())
    });
    let model = ClassifierModel {
        id: "local-chat".into(),
        name: "Local chat".into(),
        api: "llama-cpp-classify".into(),
        provider: "llama-cpp".into(),
        base_url: format!("{base_url}/v1"),
        input: vec![ModelInputModality::Text],
        r#type: "classifier".into(),
        context_window: 8192,
        ..Default::default()
    };
    let mut questions = OrderedMap::new();
    questions.insert(
        "approved".into(),
        ClassifierQuestion::Bool {
            instructions: "Does the user approve?".into(),
            criteria: ClassifierBoolCriteria {
                true_meaning: String::new(),
                false_meaning: String::new(),
            },
        },
    );
    let context = ClassifierContext {
        state: serde_json::from_str(r#"{"text":"ok"}"#).unwrap(),
        images: Vec::new(),
        questions,
    };
    let result = classify(&model, &context, &ClassifierOptions::default()).await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Stop, "{result:?}");
    let answer = result.answers.get("approved").unwrap();
    let probability = match answer {
        pi_ai::providers::classifier::ClassifierAnswer::Bool { probability } => *probability,
        other => panic!("expected bool answer, got {other:?}"),
    };
    assert!((probability - 0.880_797_077_977_882_3).abs() < 1e-9);
    assert_eq!(completions.load(Ordering::SeqCst), 1);
}
