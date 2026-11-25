//! Public routing API, request/response compatible with the Django views in `tenant/api.py`:
//!
//! * `POST /api/v1/routing/auth/backend-info/by-id-token/`  body `{"id_token": "..."}`
//! * `POST /api/v1/routing/auth/backend-info/by-agent-id/`  body `{"agent_id": "..."}`
//!
//! Success: `200 {"mappings": [{"backend_name": "...", "prefix": "/t/<prefix_key>"}]}`.
//!
//! A key claimed by more than one active backend is a conflict (backend routing spec CNS-6). With the default
//! `ROUTING_CONFLICT_POLICY=reject` it resolves to nothing: `409 {"detail": "..."}` and a warning in the log, so
//! a user is never silently sent to one of two departments. `ROUTING_CONFLICT_POLICY=all` keeps the old
//! Django behaviour (every claiming backend is listed and the client chooses).

pub mod aad;

use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::State,
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{post, MethodRouter},
    Json, Router,
};
use serde_json::json;

use crate::{
    db::routing as lookups,
    http::drf::{char_field, detail, request_data, server_error},
    settings::ConflictPolicy,
    state::AppState,
};

const USER_CONFLICT: &str =
    "This user is assigned to more than one backend. Ask an administrator to remove the duplicate assignment.";
const AGENT_CONFLICT: &str =
    "This agent is assigned to more than one backend. Ask an administrator to remove the duplicate assignment.";

pub const ID_TOKEN_PATH: &str = "/api/v1/routing/auth/backend-info/by-id-token/";

pub const AGENT_ID_PATH: &str = "/api/v1/routing/auth/backend-info/by-agent-id/";

fn mappings_response(
    policy: ConflictPolicy,
    entity: &str,
    mappings: Vec<lookups::Mapping>,
    not_found: &str,
    conflict: &str,
) -> Response {
    if mappings.is_empty() {
        return detail(StatusCode::NOT_FOUND, not_found);
    }
    if mappings.len() > 1 && policy == ConflictPolicy::Reject {
        let prefixes: Vec<&str> = mappings.iter().map(|m| m.prefix.as_str()).collect();
        // The key itself is sensitive (spec 10.6): log only where it is claimed.
        tracing::warn!(entity, backends = ?prefixes, "routing conflict: key claimed by several backends, refused");
        return detail(StatusCode::CONFLICT, conflict);
    }
    (StatusCode::OK, Json(json!({ "mappings": mappings }))).into_response()
}

/// `GetBackendInfoByUserIdTokenAPIView.post`
async fn by_id_token(State(s): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let data = match request_data(&headers, &body) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let id_token = match char_field(&data, "id_token") {
        Ok(t) => t,
        Err(r) => return r,
    };

    let payload = match s.aad.decode_id_token(&id_token).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("AAD token validation failed: {e:#}");
            return detail(StatusCode::BAD_REQUEST, aad::INVALID_TOKEN);
        }
    };
    // The user is identified by the Entra identity (tid + oid), the key the core backends publish.
    let key = match aad::extract_user_key(&payload) {
        Ok(k) => k,
        Err(msg) => return detail(StatusCode::BAD_REQUEST, msg),
    };

    match lookups::active_backends_by_user_key(&s.pool, &key).await {
        Ok(m) => {
            mappings_response(s.conflict_policy, "users", m, "No backend access found for this user.", USER_CONFLICT)
        }
        Err(e) => server_error(e),
    }
}

/// `GetBackendInfoByAgentIdAPIView.post`
async fn by_agent_id(State(s): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let data = match request_data(&headers, &body) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let agent_id = match char_field(&data, "agent_id") {
        Ok(a) => a,
        Err(r) => return r,
    };
    match lookups::active_backends_by_agent_id(&s.pool, &agent_id).await {
        Ok(m) => {
            mappings_response(s.conflict_policy, "agents", m, "No backend access found for this agent.", AGENT_CONFLICT)
        }
        Err(e) => server_error(e),
    }
}

/// Other methods: DRF answers OPTIONS with view metadata and anything else with 405.
async fn not_allowed(method: Method) -> Response {
    let allow = [(header::ALLOW, HeaderValue::from_static("POST, OPTIONS"))];
    if method == Method::OPTIONS {
        let meta = json!({
            "renders": ["application/json"],
            "parses": ["application/json", "application/x-www-form-urlencoded"],
        });
        return (StatusCode::OK, allow, Json(meta)).into_response();
    }
    let mut r = detail(StatusCode::METHOD_NOT_ALLOWED, format!("Method \"{method}\" not allowed."));
    r.headers_mut().insert(header::ALLOW, HeaderValue::from_static("POST, OPTIONS"));
    r
}

pub fn router() -> Router<Arc<AppState>> {
    let id_token = || -> MethodRouter<Arc<AppState>> { post(by_id_token).fallback(not_allowed) };
    let agent_id = || -> MethodRouter<Arc<AppState>> { post(by_agent_id).fallback(not_allowed) };
    Router::new()
        .route(ID_TOKEN_PATH, id_token())
        .route(ID_TOKEN_PATH.trim_end_matches('/'), id_token())
        .route(AGENT_ID_PATH, agent_id())
        .route(AGENT_ID_PATH.trim_end_matches('/'), agent_id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use serde_json::Value;

    #[tokio::test]
    async fn not_allowed_is_drf_shaped() {
        let r = not_allowed(Method::GET).await;
        assert_eq!(r.status(), StatusCode::METHOD_NOT_ALLOWED);
        let body: Value = serde_json::from_slice(&to_bytes(r.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(body, json!({"detail": "Method \"GET\" not allowed."}));
    }

    #[tokio::test]
    async fn conflicts_resolve_to_nothing() {
        let m = |p: &str| lookups::Mapping { backend_name: p.into(), prefix: format!("/t/{p}") };
        let two = || vec![m("a"), m("b")];
        assert_eq!(mappings_response(ConflictPolicy::Reject, "users", two(), "nf", "c").status(), StatusCode::CONFLICT);
        assert_eq!(mappings_response(ConflictPolicy::All, "users", two(), "nf", "c").status(), StatusCode::OK);
        assert_eq!(
            mappings_response(ConflictPolicy::Reject, "users", vec![m("a")], "nf", "c").status(),
            StatusCode::OK
        );
        assert_eq!(
            mappings_response(ConflictPolicy::Reject, "users", vec![], "nf", "c").status(),
            StatusCode::NOT_FOUND
        );
    }
}
