//! Admin page `/admin/` and admin API `/admin/api/*`.
//!
//! One account from the environment (ADMIN_USERNAME / ADMIN_PASSWORD), sent on every request as HTTP Basic
//! credentials. Responses use the API envelope (`{"data": ...}` / `{"error": ...}`).

pub mod auth;
mod backends;
mod fields;
mod jobs;
mod nginx;
mod sync;

use std::sync::Arc;

use axum::{
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
    Router,
};

use crate::state::AppState;

pub use auth::AdminAccount;
pub use fields::{
    backend_id as validate_backend_id, backend_name as validate_name, upstream_url as validate_upstream_url,
};

const PAGE: &str = include_str!("page.html");

/// Prefix of the admin API.
pub const API: &str = "/admin/api";

async fn page() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8")),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
            (header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY")),
            (header::REFERRER_POLICY, HeaderValue::from_static("no-referrer")),
        ],
        PAGE,
    )
        .into_response()
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/admin", get(|| async { Redirect::permanent("/admin/") }))
        .route("/admin/", get(page))
        .route(&format!("{API}/session"), get(auth::session))
        .route(&format!("{API}/backends"), get(backends::list).post(backends::create))
        .route(&format!("{API}/backends/:id"), get(backends::get_one).patch(backends::update).delete(backends::delete))
        .route(&format!("{API}/backends/:id/users"), get(backends::users))
        .route(&format!("{API}/backends/:id/agents"), get(backends::agents))
        .route(&format!("{API}/nginx"), get(nginx::status))
        .route(&format!("{API}/nginx/reload"), post(nginx::reload))
        .route(&format!("{API}/jobs"), get(jobs::list))
        .route(&format!("{API}/jobs/:name"), get(jobs::get_one).patch(jobs::update))
        .route(&format!("{API}/jobs/:name/run"), post(jobs::run))
        .route(&format!("{API}/runs"), get(jobs::runs))
        .route(&format!("{API}/runs/:id"), get(jobs::run_one))
        .route(&format!("{API}/sync"), get(sync::status))
        .route(&format!("{API}/sync/conflicts"), get(sync::conflicts))
}
