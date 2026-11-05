//! Local admin surface: routing inspection, stats, reload, overrides.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::{json_err, json_ok};
use crate::config::OverrideEntry;
use crate::router;
use crate::state::{save_overrides, AppState};
use crate::time_windows::now_minutes;

/// GET /admin/routes — what each alias resolves to right now.
pub async fn routes(State(state): State<Arc<AppState>>) -> Response {
    state.maybe_reload().await;
    let cfg = state.cfg.read().await.clone();
    let overrides = state.overrides.read().await.clone();
    let tz = cfg.server.window_timezone.clone();
    let now = now_minutes(&tz);

    let active_windows: Vec<Value> = cfg
        .windows
        .iter()
        .map(|(name, w)| {
            json!({
                "name": name,
                "start": w.start,
                "end": w.end,
                "mult": w.mult,
                "days": w.days,
                "active_now": crate::time_windows::window_active_at(w, &tz, now),
            })
        })
        .collect();

    let mut rows: Vec<Value> = vec![];
    let mut aliases: Vec<&String> = cfg.policies.keys().collect();
    aliases.sort();
    for alias in aliases {
        let mut cands: Vec<Value> = vec![];
        if let Some(ov) = overrides.get(alias) {
            cands.push(json!({
                "provider": ov.provider,
                "model": ov.model.clone().unwrap_or_else(|| alias.clone()),
                "via_override": true,
            }));
        }
        for c in &cfg.policies[alias].candidates {
            cands.push(json!({
                "provider": c.provider,
                "model": c.model,
                "active": router::candidate_active(&cfg, c, &tz, now),
                "avoid_windows": c.avoid_windows,
                "only_windows": c.only_windows,
            }));
        }
        rows.push(json!({ "alias": alias, "candidates": cands }));
    }
    json_ok(json!({
        "now_minutes": now,
        "timezone": tz,
        "windows": active_windows,
        "policies": rows,
    }))
}

/// GET /admin/stats — token and cost counters since start.
pub async fn stats(State(state): State<Arc<AppState>>) -> Response {
    json_ok(state.usage.snapshot())
}

/// POST /admin/reload — force reload of config and overrides.
pub async fn reload(State(state): State<Arc<AppState>>) -> Response {
    match state.force_reload().await {
        Ok(msg) => json_ok(json!({ "ok": true, "message": msg })),
        Err(e) => json_err(StatusCode::BAD_REQUEST, &e),
    }
}

#[derive(Deserialize)]
struct OverrideBody {
    /// Inbound model alias to pin.
    model: String,
    /// Provider to route it to.
    provider: String,
    /// Model id to send to that provider (defaults to the alias).
    model_id: Option<String>,
}

/// PUT /admin/override — pin an alias to a provider (persisted).
/// Example: `{"model": "deepseek-chat", "provider": "strata", "model_id": "qwen3-32b"}`
/// switches one workload to a local model without touching agent files.
pub async fn set_override(
    State(state): State<Arc<AppState>>,
    Json(body): Json<OverrideBody>,
) -> Response {
    let cfg = state.cfg.read().await.clone();
    if !cfg.providers.contains_key(&body.provider) {
        return json_err(
            StatusCode::BAD_REQUEST,
            &format!("unknown provider '{}'", body.provider),
        );
    }
    let mut map = state.overrides.write().await;
    map.insert(
        body.model.clone(),
        OverrideEntry {
            provider: body.provider.clone(),
            model: body.model_id.clone(),
        },
    );
    let snapshot = map.clone();
    drop(map);
    match save_overrides(&state.overrides_path, &snapshot) {
        Ok(()) => {
            if let Ok(m) = std::fs::metadata(&state.overrides_path).and_then(|m| m.modified()) {
                *state.ov_mtime.lock().unwrap() = Some(m);
            }
            json_ok(json!({
                "ok": true,
                "model": body.model,
                "provider": body.provider,
                "model_id": body.model_id,
            }))
        }
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

/// DELETE /admin/override/:model — clear an override.
pub async fn del_override(
    State(state): State<Arc<AppState>>,
    Path(model): Path<String>,
) -> Response {
    let mut map = state.overrides.write().await;
    if map.remove(&model).is_none() {
        return json_err(
            StatusCode::NOT_FOUND,
            &format!("no override for model '{model}'"),
        );
    }
    let snapshot = map.clone();
    drop(map);
    match save_overrides(&state.overrides_path, &snapshot) {
        Ok(()) => {
            if let Ok(m) = std::fs::metadata(&state.overrides_path).and_then(|m| m.modified()) {
                *state.ov_mtime.lock().unwrap() = Some(m);
            }
            json_ok(json!({ "ok": true, "model": model }))
        }
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

/// GET /admin/overrides — current persisted overrides.
pub async fn list_overrides(State(state): State<Arc<AppState>>) -> Response {
    let map = state.overrides.read().await;
    let obj = serde_json::to_value(&*map).unwrap_or(Value::Null);
    json_ok(obj)
}
