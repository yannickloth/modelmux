pub mod admin;
pub mod anthropic;
pub mod api;
pub mod config;
pub mod filters;
pub mod router;
pub mod sse;
pub mod state;
pub mod time_windows;
pub mod upstream;
pub mod usage;

use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::middleware;
use axum::routing::{delete, get, post};
use axum::Router;

use crate::state::AppState;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Build the full HTTP router for a given application state.
pub fn app(state: Arc<AppState>) -> Router {
    Router::new()
        // OpenAI-compatible surface (the only one agents ever see).
        .route("/v1/chat/completions", post(api::chat_completions))
        .route("/chat/completions", post(api::chat_completions))
        .route("/v1/models", get(api::list_models))
        .route("/models", get(api::list_models))
        .route("/health", get(api::health))
        // Admin surface (local only).
        .route("/admin/routes", get(admin::routes))
        .route("/admin/stats", get(admin::stats))
        .route("/admin/reload", post(admin::reload))
        .route("/admin/override", post(admin::set_override))
        .route("/admin/overrides", get(admin::list_overrides))
        .route("/admin/override/{model}", delete(admin::del_override))
        //
        // Request filter chain (servlet-Filter equivalent): every request
        // passes through these layers on the way in and out. The LAST
        // .layer(...) below is the OUTERMOST (see the ordering test in
        // filters.rs). To add a filter: write an async fn in
        // src/filters.rs and insert one .layer(...) line here.
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        .layer(middleware::from_fn(filters::request_id))
        .layer(middleware::from_fn(filters::access_log))
        .with_state(state)
}
