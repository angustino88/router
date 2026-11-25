//! Public routing API: which backend serves a user or an agent.
//!
//! * `POST /routing/resolve/user`   `{"id_token": "<Microsoft ID token>"}`
//! * `POST /routing/resolve/agent`  `{"agent_id": "<agent UUID>"}`
//!
//! Success: `200 {"data": {"backend_id": "...", "name": "...", "prefix": "/t/<prefix_key>"}}`.
//! Errors: `400 validation_failed | invalid_token`, `404 not_routed`, `409 ambiguous_route`.
//!
//! A key claimed by more than one active backend is a conflict (backend routing spec CNS-6): it resolves to
//! nothing and is logged, so nobody is silently sent to one of two departments.

pub mod aad;

use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Router,
};

use crate::{
    db::{
        claims::{self, canonical_uuid, Route},
        Kind,
    },
    http::api::{data, json_body, ApiError, ApiResult, Fields},
    metrics,
    state::AppState,
};

pub const RESOLVE_USER_PATH: &str = "/routing/resolve/user";
pub const RESOLVE_AGENT_PATH: &str = "/routing/resolve/agent";

/// One route, or the matching error.
fn single_route(kind: Kind, mut routes: Vec<Route>) -> ApiResult {
    let what = match kind {
        Kind::Users => "user",
        Kind::Agents => "agent",
    };
    match routes.len() {
        0 => Err(ApiError::new(StatusCode::NOT_FOUND, "not_routed", format!("No backend serves this {what}."))),
        1 => Ok(data(StatusCode::OK, routes.remove(0))),
        _ => {
            let prefixes: Vec<&str> = routes.iter().map(|r| r.prefix.as_str()).collect();
            // The key itself is sensitive (spec 10.6): log only where it is claimed.
            tracing::warn!(entity = kind.entity(), backends = ?prefixes, "routing conflict: key claimed by several backends");
            Err(ApiError::new(
                StatusCode::CONFLICT,
                "ambiguous_route",
                format!(
                    "This {what} is assigned to more than one backend. Ask an administrator to fix the assignment."
                ),
            ))
        }
    }
}

/// Counts the outcome (`router_routing_requests_total`).
fn record(kind: Kind, result: ApiResult) -> Response {
    let response = result.unwrap_or_else(IntoResponse::into_response);
    let outcome = match response.status() {
        StatusCode::OK => "ok",
        StatusCode::NOT_FOUND => "not_routed",
        StatusCode::CONFLICT => "ambiguous",
        s if s.is_client_error() => "rejected",
        _ => "error",
    };
    metrics::inc("router_routing_requests_total", &[("entity", kind.entity()), ("outcome", outcome)]);
    response
}

async fn resolve_user(State(s): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let result = async {
        let body = json_body(&headers, &body)?;
        let mut f = Fields::new(&body);
        let id_token = f.required_string("id_token")?;
        f.finish()?;
        let claims = s.aad.decode_id_token(&id_token).await.map_err(|e| {
            tracing::warn!("ID token rejected: {e:#}");
            ApiError::new(StatusCode::BAD_REQUEST, "invalid_token", aad::INVALID_TOKEN)
        })?;
        // The user is identified by the Entra identity (tid + oid), the key the backends publish.
        let key =
            aad::extract_user_key(&claims).map_err(|m| ApiError::new(StatusCode::BAD_REQUEST, "invalid_token", m))?;
        single_route(Kind::Users, claims::routes_for(&s.pool, Kind::Users, &key).await?)
    }
    .await;
    record(Kind::Users, result)
}

async fn resolve_agent(State(s): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let result = async {
        let body = json_body(&headers, &body)?;
        let mut f = Fields::new(&body);
        let raw = f.required_string("agent_id")?;
        f.finish()?;
        let key =
            canonical_uuid(&raw).ok_or_else(|| ApiError::field("agent_id", "invalid_format", "Must be a UUID."))?;
        single_route(Kind::Agents, claims::routes_for(&s.pool, Kind::Agents, &key).await?)
    }
    .await;
    record(Kind::Agents, result)
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route(RESOLVE_USER_PATH, post(resolve_user)).route(RESOLVE_AGENT_PATH, post(resolve_agent))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(p: &str) -> Route {
        Route { backend_id: p.into(), name: p.into(), prefix: format!("/t/{p}") }
    }

    #[test]
    fn one_route_or_an_error() {
        assert!(single_route(Kind::Users, vec![route("a")]).is_ok());
        let e = single_route(Kind::Users, vec![]).unwrap_err();
        assert_eq!((e.status, e.code), (StatusCode::NOT_FOUND, "not_routed"));
        let e = single_route(Kind::Agents, vec![route("a"), route("b")]).unwrap_err();
        assert_eq!((e.status, e.code), (StatusCode::CONFLICT, "ambiguous_route"));
    }
}
