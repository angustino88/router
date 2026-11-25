//! Routing sync status: stream cursors, leadership and claim conflicts.

use std::sync::Arc;

use axum::{
    extract::State,
    http::HeaderMap,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

use crate::{
    db::{mappings, Kind},
    http::api::server_error,
    settings::ConflictPolicy,
    state::AppState,
};

const CONFLICT_LIST_LIMIT: i64 = 200;

/// `GET /api/v1/admin/sync`
pub async fn status(State(s): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(r) = super::auth::authorize(&s, &headers, None).await {
        return r;
    }
    match mappings::stream_cursors(&s.pool).await {
        Ok(streams) => Json(json!({
            "sync_enabled": s.scheduler_enabled,
            "leader": s.leadership.is_leader(),
            "leader_election": s.leadership.enabled(),
            "streams": streams,
        }))
        .into_response(),
        Err(e) => server_error(e),
    }
}

/// `GET /api/v1/admin/conflicts`: keys claimed by more than one active backend.
pub async fn conflicts(State(s): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(r) = super::auth::authorize(&s, &headers, None).await {
        return r;
    }
    let users = match mappings::conflicts(&s.pool, Kind::Users, CONFLICT_LIST_LIMIT).await {
        Ok(v) => v,
        Err(e) => return server_error(e),
    };
    let agents = match mappings::conflicts(&s.pool, Kind::Agents, CONFLICT_LIST_LIMIT).await {
        Ok(v) => v,
        Err(e) => return server_error(e),
    };
    Json(json!({
        "policy": match s.conflict_policy { ConflictPolicy::Reject => "reject", ConflictPolicy::All => "all" },
        "users": { "total": users.0, "items": users.1 },
        "agents": { "total": agents.0, "items": agents.1 },
    }))
    .into_response()
}
