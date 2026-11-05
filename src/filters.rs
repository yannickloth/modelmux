//! Request filters: the proxy's interception point.
//!
//! Every request — OpenAI surface and admin surface — passes through the
//! layer chain built in `app()` (see `lib.rs`), and back out through it.
//! This is the servlet-`Filter` equivalent, built on axum layers.
//!
//! A filter is an async function over the request and the rest of the
//! chain:
//!
//! ```ignore
//! use axum::extract::Request;
//! use axum::middleware::Next;
//! use axum::response::Response;
//!
//! async fn my_filter(req: Request, next: Next) -> Response {
//!     // request direction: read or mutate headers, extensions, ...
//!     let mut resp = next.run(req).await;
//!     // response direction: read or mutate the response
//!     resp
//! }
//! ```
//!
//! Install it in `app()` with one line:
//!
//! ```ignore
//! Router::new()
//!     // ...routes...
//!     .layer(axum::middleware::from_fn(my_filter))
//!     .with_state(state)
//! ```
//!
//! Ordering: the LAST `.layer(...)` declared in `app()` is the OUTERMOST
//! (runs first in the request direction, last in the response direction).
//! The test `layer_chain_order_contract` below pins this against the
//! axum version in use.
//!
//! Notes:
//! - Filters run for every endpoint. Scope a filter to a subset of routes
//!   with `.route_layer(...)` instead of `.layer(...)`.
//! - Filters needing shared state use
//!   `middleware::from_fn_with_state(state.clone(), f)`, where `f` takes
//!   `State<Arc<AppState>>` as its first parameter.
//! - Do not buffer the body in a filter: pass `req` straight to
//!   `next.run(req)` so streaming stays intact.

use std::time::Instant;

use axum::extract::Request;
use axum::http::header::HeaderValue;
use axum::middleware::Next;
use axum::response::Response;

/// Header carrying the per-request id.
pub const HEADER_REQUEST_ID: &str = "x-request-id";

/// The request id, stashed in the request extensions by the `request_id`
/// filter so handlers can read it without parsing headers again.
#[derive(Clone, Debug)]
pub struct RequestId(pub String);

/// Propagate a client-supplied `x-request-id`, or generate a UUID v4 when
/// absent. The id is echoed in the response and stashed in the request
/// extensions.
///
/// The id is the join key between an agent-side log line and modelmux's
/// access log / `usage.jsonl` for the same request. A client id that is
/// not a valid header value is replaced by a generated one.
pub async fn request_id(mut req: Request, next: Next) -> Response {
    let id = req
        .headers()
        .get(HEADER_REQUEST_ID)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| HeaderValue::from_str(s).ok().map(|_| s.to_string()))
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    req.extensions_mut().insert(RequestId(id.clone()));
    let mut resp = next.run(req).await;
    if let Ok(hv) = HeaderValue::from_str(&id) {
        resp.headers_mut().insert(HEADER_REQUEST_ID, hv);
    }
    resp
}

/// One access-log line per request: method, path, status, duration.
///
/// Emits under `target: "modelmux.access"` so `RUST_LOG` can tune it
/// independently of the routing lines (e.g.
/// `RUST_LOG=modelmux.access=warn` silences it).
pub async fn access_log(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let started = Instant::now();
    let resp = next.run(req).await;
    tracing::info!(
        target: "modelmux.access",
        %method,
        %path,
        status = resp.status().as_u16(),
        duration_ms = started.elapsed().as_millis(),
        "request"
    );
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    /// Stamps `x-stamp` in the request direction, appends "stamp" to the
    /// response header `x-order` in the response direction.
    async fn stamp(mut req: Request, next: Next) -> Response {
        req.headers_mut()
            .insert("x-stamp", HeaderValue::from_static("1"));
        let mut resp = next.run(req).await;
        resp.headers_mut()
            .append("x-order", HeaderValue::from_static("stamp"));
        resp
    }

    /// Records whether it saw `x-stamp` (request direction), appends
    /// "observe" to `x-order` (response direction).
    async fn observe(req: Request, next: Next) -> Response {
        let saw_stamp = req.headers().get("x-stamp").is_some();
        let mut resp = next.run(req).await;
        if saw_stamp {
            resp.headers_mut()
                .insert("x-saw-stamp", HeaderValue::from_static("1"));
        }
        resp.headers_mut()
            .append("x-order", HeaderValue::from_static("observe"));
        resp
    }

    #[tokio::test]
    async fn layer_chain_order_contract() {
        // Pins axum 0.8's layer semantics against the axum version in use,
        // so reordering the chain in app() is always a conscious decision:
        // the LAST declared .layer(...) is the OUTERMOST.
        let app = Router::new()
            .route("/x", get(|| async {}))
            .layer(axum::middleware::from_fn(stamp))
            .layer(axum::middleware::from_fn(observe));
        let req = Request::builder()
            .uri("/x")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        // observe (last declared, outermost) runs first in the request
        // direction, so it does not see stamp's x-stamp yet.
        assert!(!resp.headers().get("x-saw-stamp").is_some());

        // In the response direction the innermost layer runs first, so
        // "stamp" is appended before "observe". Both markers reach the
        // client (multi-value header, read with get_all).
        let order: Vec<&str> = resp
            .headers()
            .get_all("x-order")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect();
        assert_eq!(order, vec!["stamp", "observe"]);
    }
}
