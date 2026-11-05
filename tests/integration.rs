use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::post;
use axum::{Json, Router};
use reqwest::Client;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Mock upstream providers
// ---------------------------------------------------------------------------

struct MockState {
    seen_models: std::sync::Mutex<Vec<String>>,
}

async fn echo_handler(State(state): State<Arc<MockState>>, Json(body): Json<Value>) -> Json<Value> {
    let model = body["model"].as_str().unwrap_or("").to_string();
    state.seen_models.lock().unwrap().push(model.clone());
    Json(json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion",
        "created": 0,
        "model": model,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "hello from mock" },
            "finish_reason": "stop",
        }],
        "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 },
    }))
}

const OPENAI_SSE: &str = r#"data: {"id":"c1","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":"Hi"},"finish_reason":null}]}

data: {"id":"c1","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{"content":" there"},"finish_reason":null}]}

data: {"id":"c1","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}

data: [DONE]

"#;

async fn openai_sse_handler() -> Response {
    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .body(Body::from(OPENAI_SSE.to_string()))
        .unwrap()
}

async fn fail_handler() -> (StatusCode, Json<Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": { "message": "boom", "type": "mock_error" } })),
    )
}

const ANTHROPIC_JSON: &str = r#"{
  "id": "msg_1",
  "type": "message",
  "role": "assistant",
  "model": "kimi-for-coding",
  "content": [
    { "type": "text", "text": "Checking the weather." },
    { "type": "tool_use", "id": "toolu_1", "name": "get_weather",
      "input": { "city": "Brussels" } }
  ],
  "stop_reason": "tool_use",
  "stop_sequence": null,
  "usage": { "input_tokens": 12, "output_tokens": 7 }
}
"#;

async fn anthropic_json_handler() -> (StatusCode, Json<Value>) {
    (
        StatusCode::OK,
        Json(serde_json::from_str(ANTHROPIC_JSON).unwrap()),
    )
}

const ANTHROPIC_SSE: &str = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"usage":{"input_tokens":11,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: ping
data: {"type":"ping"}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_9","name":"search","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"q\":"}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"nick\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":9}}

event: message_stop
data: {"type":"message_stop"}
"#;

async fn anthropic_sse_handler() -> Response {
    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .body(Body::from(ANTHROPIC_SSE.to_string()))
        .unwrap()
}

async fn spawn_mock(route: axum::Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, route).await.unwrap();
    });
    (format!("http://{addr}"), handle)
}

// ---------------------------------------------------------------------------
// Proxy under test
// ---------------------------------------------------------------------------

async fn start_proxy(dir: &std::path::Path) -> String {
    let cfg_path = dir.join("config.toml");
    let state = Arc::new(modelmux::state::AppState::new(cfg_path).await.unwrap());
    let app = modelmux::app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

async fn temp_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("modelmux-test-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn config_template(good: &str, bad: &str, anth: &str, body: &str) -> String {
    format!(
        r#"
[server]
listen = "127.0.0.1:1"
usage_log = "usage.jsonl"
default_provider = "good"
window_timezone = "utc"

[windows.always]
# start == end is the full-day convention (all 1440 minutes).
start = "00:00"
end = "00:00"
mult = 2.0

[providers.good]
kind = "openai"
base_url = "{good}"

[providers.bad]
kind = "openai"
base_url = "{bad}"

[providers.anth]
kind = "anthropic"
base_url = "{anth}"

{body}
"#
    )
}

const POLICIES: &str = r#"
[policies."alias"]
candidates = [
  { provider = "bad", model = "bad-model" },
  { provider = "good", model = "good-model" },
]

[policies."avoided"]
candidates = [
  { provider = "bad", model = "m1", avoid_windows = ["always"] },
  { provider = "good", model = "good-model" },
]

[policies."anth-alias"]
candidates = [
  { provider = "anth", model = "kimi-for-coding" },
]

[policies."openai-sse"]
candidates = [
  { provider = "good", model = "good-model" },
]
"#;

fn client() -> Client {
    Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap()
}

fn chat_req(alias: &str, extra: Value) -> Value {
    let mut body = json!({
        "model": alias,
        "messages": [{ "role": "user", "content": "hi" }],
    });
    if let Some(obj) = extra.as_object() {
        for (k, v) in obj {
            body[k] = v.clone();
        }
    }
    body
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn failover_on_500_and_model_rewrite() {
    let dir = temp_dir().await;
    let (good_url, good_h) = spawn_mock(
        Router::new()
            .route("/chat/completions", post(echo_handler))
            .with_state(Arc::new(MockState {
                seen_models: std::sync::Mutex::new(vec![]),
            })),
    )
    .await;
    let (bad_url, bad_h) =
        spawn_mock(Router::new().route("/chat/completions", post(fail_handler))).await;
    let (anth_url, anth_h) =
        spawn_mock(Router::new().route("/v1/messages", post(anthropic_json_handler))).await;
    let cfg = config_template(&good_url, &bad_url, &anth_url, POLICIES);
    std::fs::write(dir.join("config.toml"), &cfg).unwrap();
    let base = start_proxy(&dir).await;

    let resp = client()
        .post(format!("{base}/v1/chat/completions"))
        .json(&chat_req("alias", json!({})))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-modelmux-provider"], "good");
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["model"], "good-model");
    assert_eq!(v["choices"][0]["message"]["content"], "hello from mock");
    assert_eq!(v["usage"]["prompt_tokens"], 10);

    drop(good_h);
    drop(bad_h);
    drop(anth_h);
}

#[tokio::test]
async fn avoid_window_skips_candidate() {
    let dir = temp_dir().await;
    let (good_url, good_h) = spawn_mock(
        Router::new()
            .route("/chat/completions", post(echo_handler))
            .with_state(Arc::new(MockState {
                seen_models: std::sync::Mutex::new(vec![]),
            })),
    )
    .await;
    let (bad_url, bad_h) =
        spawn_mock(Router::new().route("/chat/completions", post(fail_handler))).await;
    let (anth_url, anth_h) =
        spawn_mock(Router::new().route("/v1/messages", post(anthropic_json_handler))).await;
    let cfg = config_template(&good_url, &bad_url, &anth_url, POLICIES);
    std::fs::write(dir.join("config.toml"), &cfg).unwrap();
    let base = start_proxy(&dir).await;

    // "bad" is avoided by the always-active window, so "good" serves directly
    // without a 500 in between.
    let resp = client()
        .post(format!("{base}/v1/chat/completions"))
        .json(&chat_req("avoided", json!({})))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-modelmux-provider"], "good");

    drop(good_h);
    drop(bad_h);
    drop(anth_h);
}

#[tokio::test]
async fn anthropic_nonstream_translation() {
    let dir = temp_dir().await;
    let (good_url, good_h) = spawn_mock(
        Router::new()
            .route("/chat/completions", post(echo_handler))
            .with_state(Arc::new(MockState {
                seen_models: std::sync::Mutex::new(vec![]),
            })),
    )
    .await;
    let (bad_url, bad_h) =
        spawn_mock(Router::new().route("/chat/completions", post(fail_handler))).await;
    let (anth_url, anth_h) =
        spawn_mock(Router::new().route("/v1/messages", post(anthropic_json_handler))).await;
    let cfg = config_template(&good_url, &bad_url, &anth_url, POLICIES);
    std::fs::write(dir.join("config.toml"), &cfg).unwrap();
    let base = start_proxy(&dir).await;

    let resp = client()
        .post(format!("{base}/v1/chat/completions"))
        .json(&chat_req("anth-alias", json!({})))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-modelmux-provider"], "anth");
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["object"], "chat.completion");
    assert_eq!(v["model"], "anth-alias");
    assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(
        v["choices"][0]["message"]["content"],
        "Checking the weather."
    );
    let tc = &v["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(tc["id"], "toolu_1");
    assert_eq!(tc["function"]["name"], "get_weather");
    assert!(tc["function"]["arguments"]
        .as_str()
        .unwrap()
        .contains("Brussels"));
    assert_eq!(v["usage"]["prompt_tokens"], 12);
    assert_eq!(v["usage"]["completion_tokens"], 7);

    drop(good_h);
    drop(bad_h);
    drop(anth_h);
}

#[tokio::test]
async fn anthropic_stream_translation() {
    let dir = temp_dir().await;
    let (good_url, good_h) = spawn_mock(
        Router::new()
            .route("/chat/completions", post(echo_handler))
            .with_state(Arc::new(MockState {
                seen_models: std::sync::Mutex::new(vec![]),
            })),
    )
    .await;
    let (bad_url, bad_h) =
        spawn_mock(Router::new().route("/chat/completions", post(fail_handler))).await;
    let (anth_url, anth_h) =
        spawn_mock(Router::new().route("/v1/messages", post(anthropic_sse_handler))).await;
    let cfg = config_template(&good_url, &bad_url, &anth_url, POLICIES);
    std::fs::write(dir.join("config.toml"), &cfg).unwrap();
    let base = start_proxy(&dir).await;

    let resp = client()
        .post(format!("{base}/v1/chat/completions"))
        .json(&chat_req(
            "anth-alias",
            json!({
                "stream": true,
                "stream_options": { "include_usage": true },
            }),
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert!(text.contains("\"content\":\"Hel\""));
    assert!(text.contains("\"content\":\"lo\""));
    assert!(text.contains("\"id\":\"toolu_9\""));
    assert!(text.contains("\"name\":\"search\""));
    assert!(text.contains("data: [DONE]"));
    // usage chunk
    assert!(text.contains("\"prompt_tokens\":11"));
    assert!(text.contains("\"completion_tokens\":9"));

    // Reassemble tool call arguments from the delta frames.
    let mut args = String::new();
    for line in text.lines() {
        let Some(payload) = line.strip_prefix("data: ") else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(payload.trim()) else {
            continue;
        };
        if let Some(tc) = v.pointer("/choices/0/delta/tool_calls/0") {
            if let Some(a) = tc.pointer("/function/arguments").and_then(|x| x.as_str()) {
                args.push_str(a);
            }
        }
    }
    assert_eq!(args, "{\"q\":\"nick\"}");

    drop(good_h);
    drop(bad_h);
    drop(anth_h);
}

#[tokio::test]
async fn openai_stream_passthrough_is_byte_identical() {
    let dir = temp_dir().await;
    // "good" returns the SSE stream for openai-sse alias (first candidate).
    let good_app = Router::new().route("/chat/completions", post(openai_sse_handler));
    let (good_url, good_h) = spawn_mock(good_app).await;
    let (bad_url, bad_h) =
        spawn_mock(Router::new().route("/chat/completions", post(fail_handler))).await;
    let (anth_url, anth_h) =
        spawn_mock(Router::new().route("/v1/messages", post(anthropic_json_handler))).await;
    let cfg = config_template(&good_url, &bad_url, &anth_url, POLICIES);
    std::fs::write(dir.join("config.toml"), &cfg).unwrap();
    let base = start_proxy(&dir).await;

    let resp = client()
        .post(format!("{base}/v1/chat/completions"))
        .json(&chat_req("openai-sse", json!({ "stream": true })))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-modelmux-provider"], "good");
    let text = resp.text().await.unwrap();
    assert_eq!(text, OPENAI_SSE);

    drop(good_h);
    drop(bad_h);
    drop(anth_h);
}

#[tokio::test]
async fn admin_override_pins_provider() {
    let dir = temp_dir().await;
    let (good_url, good_h) = spawn_mock(
        Router::new()
            .route("/chat/completions", post(echo_handler))
            .with_state(Arc::new(MockState {
                seen_models: std::sync::Mutex::new(vec![]),
            })),
    )
    .await;
    let (bad_url, bad_h) =
        spawn_mock(Router::new().route("/chat/completions", post(fail_handler))).await;
    let (anth_url, anth_h) =
        spawn_mock(Router::new().route("/v1/messages", post(anthropic_json_handler))).await;
    let cfg = config_template(&good_url, &bad_url, &anth_url, POLICIES);
    std::fs::write(dir.join("config.toml"), &cfg).unwrap();
    let base = start_proxy(&dir).await;

    // Pin "alias" to the good provider with an explicit model id.
    let resp = client()
        .post(format!("{base}/admin/override"))
        .json(&json!({
            "model": "alias",
            "provider": "good",
            "model_id": "pinned-model",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // The override is persisted (file exists) and used.
    let overrides_path = dir.join("overrides.json");
    assert!(overrides_path.exists());

    let resp = client()
        .post(format!("{base}/v1/chat/completions"))
        .json(&chat_req("alias", json!({})))
        .send()
        .await
        .unwrap();
    let v: Value = resp.json().await.unwrap();
    // echo mock reports the model it received.
    assert_eq!(v["model"], "pinned-model");

    // /admin/routes shows the override first.
    let resp = client()
        .get(format!("{base}/admin/routes"))
        .send()
        .await
        .unwrap();
    let v: Value = resp.json().await.unwrap();
    let alias_row = v["policies"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["alias"] == "alias")
        .unwrap();
    assert_eq!(alias_row["candidates"][0]["via_override"], true);

    // Clear the override; routing falls back to the policy (bad -> good).
    let resp = client()
        .delete(format!("{base}/admin/override/alias"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let resp = client()
        .post(format!("{base}/v1/chat/completions"))
        .json(&chat_req("alias", json!({})))
        .send()
        .await
        .unwrap();
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["model"], "good-model");

    drop(good_h);
    drop(bad_h);
    drop(anth_h);
}

#[tokio::test]
async fn unknown_model_uses_default_provider() {
    let dir = temp_dir().await;
    let (good_url, good_h) = spawn_mock(
        Router::new()
            .route("/chat/completions", post(echo_handler))
            .with_state(Arc::new(MockState {
                seen_models: std::sync::Mutex::new(vec![]),
            })),
    )
    .await;
    let (bad_url, bad_h) =
        spawn_mock(Router::new().route("/chat/completions", post(fail_handler))).await;
    let (anth_url, anth_h) =
        spawn_mock(Router::new().route("/v1/messages", post(anthropic_json_handler))).await;
    let cfg = config_template(&good_url, &bad_url, &anth_url, POLICIES);
    std::fs::write(dir.join("config.toml"), &cfg).unwrap();
    let base = start_proxy(&dir).await;

    let resp = client()
        .post(format!("{base}/v1/chat/completions"))
        .json(&chat_req("totally-unknown-model", json!({})))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-modelmux-provider"], "good");
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["model"], "totally-unknown-model");

    drop(good_h);
    drop(bad_h);
    drop(anth_h);
}

#[tokio::test]
async fn responses_carry_request_id() {
    // No mock upstreams: /health, /v1/models and /admin/stats never leave
    // the proxy, which is all this test needs.
    let dir = temp_dir().await;
    let cfg = config_template(
        "http://127.0.0.1:1",
        "http://127.0.0.1:1",
        "http://127.0.0.1:1",
        POLICIES,
    );
    std::fs::write(dir.join("config.toml"), &cfg).unwrap();
    let base = start_proxy(&dir).await;

    // The proxy assigns an id when the client sends none.
    let resp = client().get(format!("{base}/health")).send().await.unwrap();
    let id = resp
        .headers()
        .get("x-request-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(!id.is_empty());

    // A client-supplied id is echoed back unchanged.
    let resp = client()
        .get(format!("{base}/v1/models"))
        .header("x-request-id", "agent-side-id-123")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.headers()
            .get("x-request-id")
            .unwrap()
            .to_str()
            .unwrap(),
        "agent-side-id-123"
    );

    // The admin surface is covered by the chain as well.
    let resp = client()
        .get(format!("{base}/admin/stats"))
        .send()
        .await
        .unwrap();
    assert!(resp.headers().get("x-request-id").is_some());
}

#[tokio::test]
async fn models_listing_and_health() {
    let dir = temp_dir().await;
    let (good_url, good_h) = spawn_mock(
        Router::new()
            .route("/chat/completions", post(echo_handler))
            .with_state(Arc::new(MockState {
                seen_models: std::sync::Mutex::new(vec![]),
            })),
    )
    .await;
    let (bad_url, bad_h) =
        spawn_mock(Router::new().route("/chat/completions", post(fail_handler))).await;
    let (anth_url, anth_h) =
        spawn_mock(Router::new().route("/v1/messages", post(anthropic_json_handler))).await;
    let cfg = config_template(&good_url, &bad_url, &anth_url, POLICIES);
    std::fs::write(dir.join("config.toml"), &cfg).unwrap();
    let base = start_proxy(&dir).await;

    let resp = client()
        .get(format!("{base}/v1/models"))
        .send()
        .await
        .unwrap();
    let v: Value = resp.json().await.unwrap();
    let ids: Vec<&str> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"alias"));
    assert!(ids.contains(&"anth-alias"));
    assert!(ids.contains(&"openai-sse"));

    let resp = client().get(format!("{base}/health")).send().await.unwrap();
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["status"], "ok");

    drop(good_h);
    drop(bad_h);
    drop(anth_h);
}
