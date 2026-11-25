//! Routing sync status: stream cursors, leadership and claim conflicts.

use std::sync::Arc;

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Response,
};
use serde_json::json;

use super::auth::guarded;
use crate::{
    db::{claims, Kind},
    http::api::data,
    state::AppState,
};

const CONFLICT_LIST_LIMIT: i64 = 200;

/// `GET /admin/api/sync`
pub async fn status(State(s): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    guarded(&s, &headers, || async {
        Ok(data(
            StatusCode::OK,
            json!({
                "sync_enabled": s.sync_enabled,
                "leader": s.leadership.is_leader(),
                "leader_election": s.leadership.enabled(),
                "streams": claims::cursors(&s.pool).await?,
            }),
        ))
    })
    .await
}

/// `GET /admin/api/sync/conflicts`: keys claimed by more than one active backend (at most 200 listed per stream).
pub async fn conflicts(State(s): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    guarded(&s, &headers, || async {
        let mut out = serde_json::Map::new();
        for kind in Kind::ALL {
            let (total, items) = claims::conflicts(&s.pool, kind, CONFLICT_LIST_LIMIT).await?;
            out.insert(kind.entity().into(), json!({ "total": total, "items": items }));
        }
        Ok(data(StatusCode::OK, out))
    })
    .await
}
