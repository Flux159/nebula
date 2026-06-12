//! `mock-model` — a deterministic OpenAI-compatible chat-completions server
//! (hyper 1.x) for testing model-driven features with zero keys/tokens.
//!
//! - `POST /v1/chat/completions` (stream + non-stream), `GET /v1/models`
//! - reply: `MOCK-REPLY: <first line of the last user message>`
//! - scripting: `[[reply:XYZ]]` anywhere in the last user message makes the
//!   reply exactly `XYZ` — e2e tests drive exact outputs
//! - bind: 127.0.0.1, or 0.0.0.0 with MOCK_BIND_ALL=1 (containers reach it
//!   at host.docker.internal on plain docker / 192.168.64.1 under nebula
//!   on macOS — see COMPONENT.md #4)
//!
//! Wire as a subcommand: `your-app mock-model --port 9123`.

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};

pub async fn serve(port: u16) -> std::io::Result<()> {
    let host = if std::env::var("MOCK_BIND_ALL").is_ok() { [0, 0, 0, 0] } else { [127, 0, 0, 1] };
    let addr = std::net::SocketAddr::from((host, port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("mock-model listening on http://{addr}/v1");
    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(async move {
            let _ = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service_fn(route))
                .await;
        });
    }
}

async fn route(req: Request<hyper::body::Incoming>) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let path = req.uri().path().to_string();
    let method = req.method().clone();
    let body = req.into_body().collect().await?.to_bytes();

    let resp = match (method.as_str(), path.as_str()) {
        ("GET", "/v1/models") => plain_json(
            StatusCode::OK,
            json!({ "data": [{ "id": "mock", "object": "model" }] }),
        ),
        ("POST", "/v1/chat/completions") => completions(&body),
        _ => plain_json(StatusCode::NOT_FOUND, json!({ "error": "not found" })),
    };
    Ok(resp)
}

fn plain_json(status: StatusCode, v: Value) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(v.to_string())))
        .unwrap()
}

fn reply_for(body: &Value) -> String {
    let last_user = body["messages"]
        .as_array()
        .and_then(|m| {
            m.iter().rev().find(|msg| msg["role"] == "user").map(|msg| match &msg["content"] {
                Value::String(s) => s.clone(),
                Value::Array(blocks) => blocks
                    .iter()
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
                _ => String::new(),
            })
        })
        .unwrap_or_default();
    if let Some(start) = last_user.find("[[reply:") {
        if let Some(end) = last_user[start..].find("]]") {
            return last_user[start + 8..start + end].to_string();
        }
    }
    let line: String = last_user.lines().next().unwrap_or("").chars().take(120).collect();
    format!("MOCK-REPLY: {line}")
}

fn completions(raw: &[u8]) -> Response<Full<Bytes>> {
    let body: Value = serde_json::from_slice(raw).unwrap_or_else(|_| json!({}));
    let text = reply_for(&body);
    let model = body["model"].as_str().unwrap_or("mock").to_string();

    if !body["stream"].as_bool().unwrap_or(false) {
        return plain_json(
            StatusCode::OK,
            json!({
                "id": "mock-1", "object": "chat.completion", "model": model,
                "choices": [{ "index": 0, "finish_reason": "stop",
                    "message": { "role": "assistant", "content": text } }],
                "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
            }),
        );
    }

    // SSE chunks then [DONE] — the llama.cpp/OpenAI streaming shape.
    let mk = |delta: Value, finish: Option<&str>| {
        json!({
            "id": "mock-1", "object": "chat.completion.chunk", "model": model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }]
        })
        .to_string()
    };
    let mut sse = format!("data: {}\n\n", mk(json!({ "role": "assistant" }), None));
    for chunk in text.as_bytes().chunks(24) {
        sse.push_str(&format!("data: {}\n\n", mk(json!({ "content": String::from_utf8_lossy(chunk) }), None)));
    }
    sse.push_str(&format!("data: {}\n\n", mk(json!({}), Some("stop"))));
    sse.push_str("data: [DONE]\n\n");

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Full::new(Bytes::from(sse)))
        .unwrap()
}
