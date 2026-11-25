//! HTTP server assembly: every feature router, health probes, the OpenAPI document and the fallback.

pub mod drf;

use std::sync::Arc;

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use tower_http::trace::TraceLayer;

use crate::{admin, openapi, routing, state::AppState};

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

/// `GET /openapi.json` (admin credentials required).
async fn openapi_json(State(s): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(r) = admin::auth::authorize(&s, &headers, None).await {
        return r;
    }
    match openapi::document() {
        Ok(doc) => Json(doc).into_response(),
        Err(e) => drf::server_error(e),
    }
}

async fn not_found() -> Response {
    drf::detail(StatusCode::NOT_FOUND, "Not found.")
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/openapi.json", get(openapi_json))
        .merge(routing::router())
        .merge(admin::router())
        .fallback(not_found)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
