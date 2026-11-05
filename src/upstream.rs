//! Upstream request dispatch for both provider kinds.

use axum::http::{header, HeaderValue, StatusCode};
use reqwest::Client;
use serde_json::Value;

use crate::config::{AuthStyle, ProviderCfg, ProviderKind};

/// Outcome of one candidate attempt.
pub enum Attempt {
    /// 2xx response; body not yet consumed.
    Ok(reqwest::Response),
    /// Connect/timeout/408/429/5xx: try the next candidate.
    Retryable(String),
    /// Non-retryable 4xx: surface to the client as-is.
    Fatal { status: StatusCode, body: String },
}

pub fn retryable_status(s: StatusCode) -> bool {
    matches!(s.as_u16(), 408 | 425 | 429 | 500 | 502 | 503 | 504)
}

pub fn build_url(p: &ProviderCfg) -> String {
    let base = p.base_url.trim_end_matches('/');
    match p.kind {
        ProviderKind::Openai => format!("{base}/chat/completions"),
        ProviderKind::Anthropic => format!("{base}/v1/messages"),
    }
}

pub fn provider_key(p: &ProviderCfg) -> Option<String> {
    let var = p.api_key_env.as_deref()?;
    let v = std::env::var(var).ok()?.trim().to_string();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

fn set_auth(req: reqwest::RequestBuilder, p: &ProviderCfg) -> reqwest::RequestBuilder {
    let Some(k) = provider_key(p) else {
        return req;
    };
    match (p.kind, p.auth_style) {
        (ProviderKind::Openai, _) => req.bearer_auth(k),
        (ProviderKind::Anthropic, AuthStyle::ApiKey) => req.header("x-api-key", k),
        (ProviderKind::Anthropic, AuthStyle::Bearer) => req.bearer_auth(k),
    }
}

/// Send the request to the provider. The caller has already translated the
/// body to the provider's format and set `model`.
pub async fn send(
    client: &Client,
    p: &ProviderCfg,
    body: &Value,
    user_agent: Option<&str>,
) -> Attempt {
    let url = build_url(p);
    let mut req = set_auth(client.post(&url).json(body), p);
    if p.kind == ProviderKind::Anthropic {
        req = req.header("anthropic-version", p.anthropic_version.clone());
    }
    if let Some(ua) = user_agent {
        if let Ok(hv) = HeaderValue::from_str(ua) {
            // Kimi's terms require keeping the real client identity.
            req = req.header(header::USER_AGENT, hv);
        }
    }
    match req.send().await {
        Err(e) => Attempt::Retryable(format!("transport error: {e}")),
        Ok(resp) => {
            let status = resp.status();
            if status.is_success() {
                Attempt::Ok(resp)
            } else {
                let text = resp.text().await.unwrap_or_default();
                if retryable_status(status) {
                    Attempt::Retryable(format!("status {status}: {}", truncate(&text, 500)))
                } else {
                    Attempt::Fatal { status, body: text }
                }
            }
        }
    }
}

/// Map an Anthropic-format error body to an OpenAI-format error object.
pub fn anthropic_error_to_openai(status: StatusCode, body: &str) -> (StatusCode, Value) {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let msg = v
        .pointer("/error/message")
        .and_then(|x| x.as_str())
        .unwrap_or(body);
    let etype = v
        .pointer("/error/type")
        .and_then(|x| x.as_str())
        .unwrap_or("upstream_error");
    (
        status,
        json!({ "error": { "message": msg, "type": etype, "code": status.as_u16() } }),
    )
}

pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let cut: String = s.chars().take(n).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_building() {
        let p = ProviderCfg {
            kind: ProviderKind::Openai,
            base_url: "https://api.example.com/v1/".into(),
            api_key_env: None,
            price_windows: vec![],
            models: Default::default(),
            auth_style: AuthStyle::Bearer,
            anthropic_version: "2023-06-01".into(),
        };
        assert_eq!(build_url(&p), "https://api.example.com/v1/chat/completions");
        let p2 = ProviderCfg {
            kind: ProviderKind::Anthropic,
            base_url: "https://api.kimi.com/coding/".into(),
            api_key_env: None,
            price_windows: vec![],
            models: Default::default(),
            auth_style: AuthStyle::Bearer,
            anthropic_version: "2023-06-01".into(),
        };
        assert_eq!(build_url(&p2), "https://api.kimi.com/coding/v1/messages");
    }

    #[test]
    fn status_classification() {
        assert!(retryable_status(StatusCode::from_u16(429).unwrap()));
        assert!(retryable_status(StatusCode::from_u16(503).unwrap()));
        assert!(!retryable_status(StatusCode::from_u16(400).unwrap()));
        assert!(!retryable_status(StatusCode::from_u16(404).unwrap()));
    }
}
