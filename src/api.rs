//! The agent-facing OpenAI-compatible surface plus the routing loop.

use std::sync::Arc;
use std::time::Instant;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use futures::StreamExt;
use serde_json::{json, Value};

use crate::anthropic::{self, StreamTlx};
use crate::config::{Config, ProviderKind};
use crate::router::{self, Candidate};
use crate::sse::{self, SseParser};
use crate::state::AppState;
use crate::time_windows::window_active;
use crate::upstream::{self, Attempt};
use crate::usage::Record;
use crate::VERSION;

/// Cost of one request in USD, using the provider's configured per-model
/// prices and the active price windows at the current time (each window in
/// its own timezone).
pub fn cost_usd(
    cfg: &Config,
    provider_id: &str,
    model: &str,
    pt: Option<u64>,
    ct: Option<u64>,
    tz: &str,
) -> Option<f64> {
    let (Some(pt), Some(ct)) = (pt, ct) else {
        return None;
    };
    let p = cfg.providers.get(provider_id)?;
    let price = p.models.get(model)?;
    let mut mult = 1.0f64;
    for wname in &p.price_windows {
        if let Some(w) = cfg.windows.get(wname) {
            if window_active(w, tz) {
                mult *= w.mult;
            }
        }
    }
    let base = price.input_per_mtok * pt as f64 / 1_000_000.0
        + price.output_per_mtok * ct as f64 / 1_000_000.0;
    Some(base * mult)
}

/// POST /v1/chat/completions (and /chat/completions).
pub async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    state.maybe_reload().await;
    let cfg = state.cfg.read().await.clone();
    let overrides = state.overrides.read().await.clone();

    if let Some(expected) = &cfg.server.api_key {
        let ok = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.trim().eq_ignore_ascii_case(&format!("Bearer {expected}")))
            .unwrap_or(false);
        if !ok {
            return json_err(
                StatusCode::UNAUTHORIZED,
                "invalid api key (server.api_key is set)",
            );
        }
    }

    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return json_err(StatusCode::BAD_REQUEST, &format!("invalid json: {e}")),
    };
    let alias = match req.get("model").and_then(|m| m.as_str()) {
        Some(m) if !m.trim().is_empty() => m.to_string(),
        _ => return json_err(StatusCode::BAD_REQUEST, "missing 'model' field"),
    };
    let stream = req.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    let include_usage = req
        .pointer("/stream_options/include_usage")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let started = Instant::now();

    let tz = cfg.server.window_timezone.clone();
    let candidates = router::resolve(&cfg, &overrides, &alias, &tz);
    if candidates.is_empty() {
        return json_err(
            StatusCode::NOT_FOUND,
            &format!(
                "no route for model '{alias}': no matching policy, and default_provider is \
                 unset, missing its key, or absent"
            ),
        );
    }

    let mut attempts: Vec<String> = vec![];
    for (i, cand) in candidates.iter().enumerate() {
        let Some(pcfg) = cfg.providers.get(&cand.provider) else {
            continue;
        };
        let mut cbody = req.clone();
        cbody["model"] = Value::String(cand.model.clone());
        let body_val: Value = if pcfg.kind == ProviderKind::Anthropic {
            match anthropic::translate_request(&cbody, cfg.server.default_max_tokens) {
                Ok(v) => v,
                Err(e) => {
                    attempts.push(format!("{}: translation failed: {e}", cand.provider));
                    continue;
                }
            }
        } else {
            cbody
        };

        match upstream::send(&state.client, pcfg, &body_val, user_agent.as_deref()).await {
            Attempt::Ok(resp) => {
                tracing::info!(
                    alias = %alias,
                    provider = %cand.provider,
                    model = %cand.model,
                    stream,
                    via_override = cand.via_override,
                    "routed"
                );
                let resp_ct = resp
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());
                if stream {
                    return stream_response(
                        state,
                        cfg,
                        cand.clone(),
                        resp,
                        resp_ct,
                        alias.clone(),
                        include_usage,
                        i + 1,
                        started,
                    );
                }
                return nonstream_response(
                    state,
                    cfg,
                    cand.clone(),
                    resp,
                    resp_ct,
                    alias.clone(),
                    i + 1,
                    started,
                )
                .await;
            }
            Attempt::Retryable(msg) => {
                tracing::warn!(provider = %cand.provider, "candidate failed: {msg}");
                attempts.push(format!("{}: {msg}", cand.provider));
            }
            Attempt::Fatal { status, body } => {
                let rec = Record {
                    alias: alias.clone(),
                    provider: cand.provider.clone(),
                    model: cand.model.clone(),
                    status: status.as_u16(),
                    stream,
                    prompt_tokens: None,
                    completion_tokens: None,
                    cost_usd: None,
                    duration_ms: started.elapsed().as_millis(),
                    attempts: i + 1,
                    error: None,
                };
                state.usage.record(&rec);
                if pcfg.kind == ProviderKind::Anthropic {
                    let (st, v) = upstream::anthropic_error_to_openai(status, &body);
                    return json_err_value(st, v);
                }
                return passthrough_error(status, &body);
            }
        }
    }

    let msg = format!(
        "all {} candidate(s) failed: {}",
        attempts.len(),
        attempts.join(" | ")
    );
    state.usage.record(&Record {
        alias,
        provider: "-".into(),
        model: "-".into(),
        status: 502,
        stream,
        prompt_tokens: None,
        completion_tokens: None,
        cost_usd: None,
        duration_ms: started.elapsed().as_millis(),
        attempts: candidates.len(),
        error: Some(msg.clone()),
    });
    json_err(StatusCode::BAD_GATEWAY, &msg)
}

#[allow(clippy::too_many_arguments)]
async fn nonstream_response(
    state: Arc<AppState>,
    cfg: Config,
    cand: Candidate,
    resp: reqwest::Response,
    resp_ct: Option<String>,
    alias: String,
    attempts: usize,
    started: Instant,
) -> Response {
    let status = resp.status();
    let is_anthropic = cfg
        .providers
        .get(&cand.provider)
        .map(|p| p.kind == ProviderKind::Anthropic)
        .unwrap_or(false);
    let bytes = resp.bytes().await.unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes);
    let (pt, ct) = sse::openai_usage(&text)
        .map(|(a, b)| (Some(a), Some(b)))
        .unwrap_or((None, None));
    let cost = cost_usd(
        &cfg,
        &cand.provider,
        &cand.model,
        pt,
        ct,
        &cfg.server.window_timezone,
    );
    state.usage.record(&Record {
        alias: alias.clone(),
        provider: cand.provider.clone(),
        model: cand.model.clone(),
        status: status.as_u16(),
        stream: false,
        prompt_tokens: pt,
        completion_tokens: ct,
        cost_usd: cost,
        duration_ms: started.elapsed().as_millis(),
        attempts,
        error: None,
    });

    let mut b = Response::builder().status(status);
    b = b.header(
        header::CONTENT_TYPE,
        if is_anthropic {
            "application/json".to_string()
        } else {
            resp_ct.unwrap_or_else(|| "application/json".to_string())
        },
    );
    b = b.header("x-modelmux-provider", cand.provider.as_str());
    b = b.header("x-modelmux-alias", alias.as_str());

    if is_anthropic {
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let out = anthropic::translate_response(&v, &alias);
        b.body(Body::from(out.to_string()))
    } else {
        b.body(Body::from(bytes))
    }
    .unwrap_or_else(|_| Response::new(Body::empty()))
}

#[allow(clippy::too_many_arguments)]
fn stream_response(
    state: Arc<AppState>,
    cfg: Config,
    cand: Candidate,
    resp: reqwest::Response,
    resp_ct: Option<String>,
    alias: String,
    include_usage: bool,
    attempts: usize,
    started: Instant,
) -> Response {
    let is_anthropic = cfg
        .providers
        .get(&cand.provider)
        .map(|p| p.kind == ProviderKind::Anthropic)
        .unwrap_or(false);
    let provider = cand.provider.clone();
    let model = cand.model.clone();
    let tz = cfg.server.window_timezone.clone();
    let provider_hdr = provider.clone();
    let alias_hdr = alias.clone();

    let stream = async_stream::stream! {
        let mut parser = SseParser::new();
        let mut outbuf: Vec<String> = vec![];
        let mut tlx = is_anthropic.then(|| StreamTlx::new(&alias, include_usage));
        let mut usage: Option<(u64, u64)> = None;
        let mut up = resp.bytes_stream();
        while let Some(item) = up.next().await {
            match item {
                Ok(chunk) => {
                    parser.feed(&chunk);
                    while let Some(ev) = parser.next_event() {
                        if let Some(t) = tlx.as_mut() {
                            t.on_event(&ev, &mut outbuf);
                        } else if let Some(u) = sse::openai_usage(&ev.data) {
                            usage = Some(u);
                        }
                    }
                    if is_anthropic {
                        for s in outbuf.drain(..) {
                            yield Ok::<Bytes, std::io::Error>(Bytes::from(s));
                        }
                    } else {
                        yield Ok::<Bytes, std::io::Error>(chunk);
                    }
                }
                Err(e) => {
                    tracing::warn!(provider = %provider, "upstream stream error: {e}");
                    break;
                }
            }
        }
        if let Some(t) = tlx.as_mut() {
            if !t.finished {
                t.end(&mut outbuf);
            }
            usage = Some((t.input_tokens, t.output_tokens));
            for s in outbuf.drain(..) {
                yield Ok::<Bytes, std::io::Error>(Bytes::from(s));
            }
        }
        let (pt, ct) = usage
            .map(|(a, b)| (Some(a), Some(b)))
            .unwrap_or((None, None));
        let cost = cost_usd(&cfg, &provider, &model, pt, ct, &tz);
        state.usage.record(&Record {
            alias,
            provider,
            model,
            status: 200,
            stream: true,
            prompt_tokens: pt,
            completion_tokens: ct,
            cost_usd: cost,
            duration_ms: started.elapsed().as_millis(),
            attempts,
            error: None,
        });
    };

    let mut b = Response::builder().status(200);
    b = b.header(
        header::CONTENT_TYPE,
        resp_ct.unwrap_or_else(|| "text/event-stream".to_string()),
    );
    b = b.header("cache-control", "no-cache");
    b = b.header("x-modelmux-provider", provider_hdr.as_str());
    b = b.header("x-modelmux-alias", alias_hdr.as_str());
    b.body(Body::from_stream(stream))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

/// GET /v1/models — the aliases agents can request.
pub async fn list_models(State(state): State<Arc<AppState>>) -> Response {
    state.maybe_reload().await;
    let cfg = state.cfg.read().await.clone();
    let mut data: Vec<Value> = vec![];
    let mut aliases: Vec<&String> = cfg.policies.keys().collect();
    aliases.sort();
    for alias in aliases {
        data.push(json!({
            "id": alias,
            "object": "model",
            "created": 0,
            "owned_by": "modelmux",
        }));
    }
    json_ok(json!({ "object": "list", "data": data }))
}

pub async fn health() -> Response {
    json_ok(json!({ "status": "ok", "version": VERSION }))
}

pub fn json_ok(v: Value) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(v.to_string()))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

pub fn json_err(status: StatusCode, msg: &str) -> Response {
    json_err_value(
        status,
        json!({ "error": { "message": msg, "type": "modelmux_error" } }),
    )
}

pub fn json_err_value(status: StatusCode, v: Value) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(v.to_string()))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

fn passthrough_error(status: StatusCode, body: &str) -> Response {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        return json_err_value(status, v);
    }
    json_err(status, body)
}
