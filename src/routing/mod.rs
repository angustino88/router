//! Public routing API (served under the same paths by the service and by nginx):
//!
//! * `POST /routing/resolve/user`   body `{"id_token": "..."}` (Microsoft ID token)
//! * `POST /routing/resolve/agent`  body `{"agent_id": "..."}` (agent UUID)
//!
//! The paths of the original service, `/routing/auth/backend-info/by-id-token/` and `.../by-agent-id/` (also
//! under `/api/v1/`), remain as deprecated aliases: same behaviour, plus `Deprecation: true` and a `Link` to
//! the successor.
//! Success: `200 {"mappings": [{"backend_name": "...", "prefix": "/t/<prefix_key>"}]}`.
//!
//! A key claimed by more than one active backend is a conflict (backend routing spec CNS-6). With the default
//! `ROUTING_CONFLICT_POLICY=reject` it resolves to nothing: `409 {"detail": "..."}` and a warning in the log, so
//! a user is never silently sent to one of two departments. `ROUTING_CONFLICT_POLICY=all` keeps the old
//! behaviour (every claiming backend is listed and the client chooses).

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
    http::api::{char_field, detail, request_data, server_error},
    metrics,
    settings::ConflictPolicy,
    state::AppState,
};

const USER_CONFLICT: &str =
    "This user is assigned to more than one backend. Ask an administrator to remove the duplicate assignment.";
const AGENT_CONFLICT: &str =
    "This agent is assigned to more than one backend. Ask an administrator to remove the duplicate assignment.";

pub const RESOLVE_USER_PATH: &str = "/routing/resolve/user";
pub const RESOLVE_AGENT_PATH: &str = "/routing/resolve/agent";

/// Deprecated aliases of the user endpoint (original service paths, with and without the trailing slash).
pub const LEGACY_USER_PATHS: [&str; 4] = [
    "/routing/auth/backend-info/by-id-token/",
    "/routing/auth/backend-info/by-id-token",
    "/api/v1/routing/auth/backend-info/by-id-token/",
    "/api/v1/routing/auth/backend-info/by-id-token",
];
/// Deprecated aliases of the agent endpoint.
pub const LEGACY_AGENT_PATHS: [&str; 4] = [
    "/routing/auth/backend-info/by-agent-id/",
    "/routing/auth/backend-info/by-agent-id",
    "/api/v1/routing/auth/backend-info/by-agent-id/",
    "/api/v1/routing/auth/backend-info/by-agent-id",
];

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

/// Counts the outcome of a routing request (`router_routing_requests_total`).
fn record(entity: &str, r: Response) -> Response {
    let outcome = match r.status() {
        StatusCode::OK => "ok",
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::CONFLICT => "conflict",
        s if s.is_client_error() => "rejected",
        _ => "error",
    };
    metrics::inc("router_routing_requests_total", &[("entity", entity), ("outcome", outcome)]);
    r
}

async fn by_id_token(State(s): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    record("users", resolve_user(&s, &headers, &body).await)
}

async fn by_agent_id(State(s): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    record("agents", resolve_agent(&s, &headers, &body).await)
}

async fn resolve_user(s: &AppState, headers: &HeaderMap, body: &Bytes) -> Response {
    let data = match request_data(headers, body) {
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
    // The user is identified by the Entra identity (tid + oid), the key the backends publish.
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

async fn resolve_agent(s: &AppState, headers: &HeaderMap, body: &Bytes) -> Response {
    let data = match request_data(headers, body) {
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

/// OPTIONS answers the view metadata, any other method 405.
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

/// Marks a response of a deprecated alias (RFC 9745 `Deprecation`, RFC 8288 `Link` to the successor).
fn deprecated(mut r: Response, successor: &'static str) -> Response {
    let h = r.headers_mut();
    h.insert("deprecation", HeaderValue::from_static("true"));
    if let Ok(link) = HeaderValue::from_str(&format!("<{successor}>; rel=\"successor-version\"")) {
        h.insert(header::LINK, link);
    }
    r
}

async fn legacy_by_id_token(state: State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    deprecated(by_id_token(state, headers, body).await, RESOLVE_USER_PATH)
}

async fn legacy_by_agent_id(state: State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    deprecated(by_agent_id(state, headers, body).await, RESOLVE_AGENT_PATH)
}

pub fn router() -> Router<Arc<AppState>> {
    let endpoint = |m: MethodRouter<Arc<AppState>>| m.fallback(not_allowed);
    let mut r = Router::new()
        .route(RESOLVE_USER_PATH, endpoint(post(by_id_token)))
        .route(RESOLVE_AGENT_PATH, endpoint(post(by_agent_id)));
    for path in LEGACY_USER_PATHS {
        r = r.route(path, endpoint(post(legacy_by_id_token)));
    }
    for path in LEGACY_AGENT_PATHS {
        r = r.route(path, endpoint(post(legacy_by_agent_id)));
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use serde_json::Value;

    #[tokio::test]
    async fn not_allowed_has_detail_body() {
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
