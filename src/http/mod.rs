//! HTTP server assembly: feature routers, health probes, metrics, the OpenAPI document and the middleware stack
//! (request id, access log, error envelope, timeout, body limit, `nosniff`, `no-store`).

pub mod api;
pub mod middleware;

use std::{sync::Arc, time::Duration};

use axum::{
    extract::{DefaultBodyLimit, State},
    http::{header, HeaderMap, HeaderValue, Request, StatusCode},
    middleware::from_fn,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use tower_http::{set_header::SetResponseHeaderLayer, timeout::TimeoutLayer, trace::TraceLayer};

use crate::{
    admin,
    db::{claims, Kind},
    metrics::{self, Gauge},
    openapi, routing,
    state::AppState,
};

/// Largest accepted request body (routing and admin bodies are a few kilobytes).
const MAX_BODY_BYTES: usize = 1024 * 1024;
/// Upper bound of any request; nothing served here legitimately takes longer.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Liveness: the process answers.
async fn healthz() -> &'static str {
    "ok"
}

/// Readiness: the database answers too.
async fn readyz(State(s): State<Arc<AppState>>) -> Response {
    match sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(&s.pool).await {
        Ok(_) => (StatusCode::OK, "ok").into_response(),
        Err(e) => {
            tracing::warn!("readiness: database unavailable: {e}");
            (StatusCode::SERVICE_UNAVAILABLE, "database unavailable").into_response()
        }
    }
}

/// `GET /metrics` (Prometheus). Not exposed by the public nginx listener.
async fn metrics_text(State(s): State<Arc<AppState>>) -> Response {
    let mut gauges = vec![
        Gauge::single(
            "router_sync_leader",
            "1 when this replica runs the stream sync.",
            s.leadership.is_leader() as u8 as f64,
        ),
        Gauge::single("router_sync_enabled", "1 when SYNC_ENABLED is on.", s.sync_enabled as u8 as f64),
    ];
    let counts: Result<(i64, i64), _> = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE is_active), count(*) FILTER (WHERE is_active AND upstream_url <> '')
           FROM backend",
    )
    .fetch_one(&s.pool)
    .await;
    if let Ok((active, routed)) = counts {
        gauges.push(Gauge::single("router_backends_active", "Active backends in the registry.", active as f64));
        gauges.push(Gauge::single("router_backends_routed", "Active backends with an upstream URL.", routed as f64));
    }
    let mut conflicts =
        Gauge { name: "router_conflicting_keys", help: "Keys claimed by several backends.", samples: vec![] };
    for kind in [Kind::Users, Kind::Agents] {
        if let Ok((n, _)) = claims::conflicts(&s.pool, kind, 0).await {
            conflicts.samples.push((metrics::labels(&[("entity", kind.entity())]), n as f64));
        }
    }
    gauges.push(conflicts);
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], metrics::render(&gauges)).into_response()
}

/// `GET /openapi.json`: the OpenAPI document itself, not wrapped in the envelope (admin credentials required).
async fn openapi_json(State(s): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    admin::auth::guarded(&s, &headers, || async {
        Ok(Json(openapi::document().map_err(api::ApiError::internal)?).into_response())
    })
    .await
}

async fn not_found() -> api::ApiError {
    api::ApiError::not_found()
}

pub fn router(state: Arc<AppState>) -> Router {
    // Logs method, path (never the query string or body: they can carry tokens) and the request id.
    let trace = TraceLayer::new_for_http().make_span_with(|req: &Request<_>| {
        let id = req.headers().get(middleware::REQUEST_ID).and_then(|v| v.to_str().ok()).unwrap_or("-");
        tracing::info_span!("http", method = %req.method(), path = %req.uri().path(), request_id = %id)
    });
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics_text))
        .route("/openapi.json", get(openapi_json))
        .merge(routing::router())
        .merge(admin::router())
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(TimeoutLayer::with_status_code(StatusCode::REQUEST_TIMEOUT, REQUEST_TIMEOUT))
        .layer(from_fn(middleware::envelope_errors))
        .layer(trace)
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::overriding(header::CACHE_CONTROL, HeaderValue::from_static("no-store")))
        .layer(from_fn(middleware::request_id))
        .with_state(state)
}
