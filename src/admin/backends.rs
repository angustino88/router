//! Backend CRUD and the read-only user/agent mapping lists.

use std::{collections::HashMap, sync::Arc};

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

use super::fields::{self, authorized_body, optional_bool, optional_int, optional_string};
use crate::{
    db::{
        backends::{self as db, BackendPatch, BackendView, NewBackend},
        mappings::list_mappings,
        Kind,
    },
    http::drf::{detail, field_error, server_error},
    jobs::render_nginx_map_recorded,
    state::AppState,
};

const NOT_FOUND: &str = "Not found.";

/// Unique index on `backend_id` violated.
fn duplicate_backend_id(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(d) if d.constraint() == Some("tenant_backend_backend_id_uniq"))
}

/// Regenerates the nginx map after a change; the outcome goes into the response as `nginx_map`.
async fn regenerate(s: &AppState) -> Value {
    match render_nginx_map_recorded(&s.pool, &s.maps, "admin").await {
        Ok(v) => v,
        Err(e) => json!({ "error": e }),
    }
}

fn with_map(view: BackendView, map: Value) -> Value {
    let mut v = serde_json::to_value(view).unwrap_or_else(|_| json!({}));
    v["nginx_map"] = map;
    v
}

/// `GET /api/v1/admin/backends`
pub async fn list(State(s): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(r) = super::auth::authorize(&s, &headers, None).await {
        return r;
    }
    match db::list_views(&s.pool).await {
        Ok(b) => Json(json!({ "backends": b })).into_response(),
        Err(e) => server_error(e),
    }
}

/// `GET /api/v1/admin/backends/{id}`
pub async fn get_one(State(s): State<Arc<AppState>>, headers: HeaderMap, Path(id): Path<i64>) -> Response {
    if let Err(r) = super::auth::authorize(&s, &headers, None).await {
        return r;
    }
    match db::get_view(&s.pool, id).await {
        Ok(Some(b)) => Json(b).into_response(),
        Ok(None) => detail(StatusCode::NOT_FOUND, NOT_FOUND),
        Err(e) => server_error(e),
    }
}

/// `POST /api/v1/admin/backends`
pub async fn create(State(s): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let data = match authorized_body(&s, &headers, &body).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let new = (|| -> Result<NewBackend, Response> {
        let name = optional_string(&data, "name")?.ok_or_else(|| field_error("name", "This field is required."))?;
        Ok(NewBackend {
            name: fields::backend_name(&name).map_err(|m| field_error("name", m))?,
            upstream_url: fields::upstream_url(&optional_string(&data, "upstream_url")?.unwrap_or_default())
                .map_err(|m| field_error("upstream_url", m))?,
            backend_id: fields::backend_id(&optional_string(&data, "backend_id")?.unwrap_or_default())
                .map_err(|m| field_error("backend_id", m))?,
            is_active: optional_bool(&data, "is_active")?.unwrap_or(true),
            computer_sync_version: optional_int(&data, "computer_sync_version", 0, i32::MAX as i64)?.unwrap_or(0)
                as i32,
        })
    })();
    let new = match new {
        Ok(n) => n,
        Err(r) => return r,
    };
    match db::create_backend(&s.pool, &new).await {
        Ok(view) => {
            tracing::info!(prefix_key = %view.prefix_key, name = %view.name, "backend created");
            let map = regenerate(&s).await;
            (StatusCode::CREATED, Json(with_map(view, map))).into_response()
        }
        Err(e) if duplicate_backend_id(&e) => {
            field_error("backend_id", "Another backend already uses this backend_id.")
        }
        Err(e) => server_error(e),
    }
}

/// `PATCH|PUT /api/v1/admin/backends/{id}` (every field optional).
pub async fn update(State(s): State<Arc<AppState>>, headers: HeaderMap, Path(id): Path<i64>, body: Bytes) -> Response {
    let data = match authorized_body(&s, &headers, &body).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let patch = (|| -> Result<BackendPatch, Response> {
        Ok(BackendPatch {
            name: optional_string(&data, "name")?
                .map(|n| fields::backend_name(&n).map_err(|m| field_error("name", m)))
                .transpose()?,
            upstream_url: optional_string(&data, "upstream_url")?
                .map(|u| fields::upstream_url(&u).map_err(|m| field_error("upstream_url", m)))
                .transpose()?,
            backend_id: optional_string(&data, "backend_id")?
                .map(|b| fields::backend_id(&b).map_err(|m| field_error("backend_id", m)))
                .transpose()?,
            is_active: optional_bool(&data, "is_active")?,
            computer_sync_version: optional_int(&data, "computer_sync_version", 0, i32::MAX as i64)?.map(|v| v as i32),
        })
    })();
    let patch = match patch {
        Ok(p) => p,
        Err(r) => return r,
    };
    // A URL for a backend that the static map already routes would never take effect (duplicate map key).
    if matches!(&patch.upstream_url, Some(u) if !u.is_empty()) {
        match db::get_view(&s.pool, id).await {
            Ok(Some(current)) if s.maps.git_entries().iter().any(|e| e.prefix_key == current.prefix_key) => {
                return field_error(
                    "upstream_url",
                    "This backend is declared in the static backends.map file; remove that line first.",
                );
            }
            Ok(Some(_)) => {}
            Ok(None) => return detail(StatusCode::NOT_FOUND, NOT_FOUND),
            Err(e) => return server_error(e),
        }
    }
    match db::update_backend(&s.pool, id, &patch).await {
        Ok(Some(view)) => {
            tracing::info!(prefix_key = %view.prefix_key, "backend updated");
            let map = regenerate(&s).await;
            Json(with_map(view, map)).into_response()
        }
        Ok(None) => detail(StatusCode::NOT_FOUND, NOT_FOUND),
        Err(e) if duplicate_backend_id(&e) => {
            field_error("backend_id", "Another backend already uses this backend_id.")
        }
        Err(e) => server_error(e),
    }
}

/// `DELETE /api/v1/admin/backends/{id}`: the backend and all its mappings.
pub async fn delete(State(s): State<Arc<AppState>>, headers: HeaderMap, Path(id): Path<i64>) -> Response {
    if let Err(r) = super::auth::authorize(&s, &headers, None).await {
        return r;
    }
    match db::delete_backend(&s.pool, id).await {
        Ok(Some(counts)) => {
            tracing::warn!(id, users = counts.users, agents = counts.agents, "backend deleted");
            let map = regenerate(&s).await;
            Json(json!({ "deleted": counts, "nginx_map": map })).into_response()
        }
        Ok(None) => detail(StatusCode::NOT_FOUND, NOT_FOUND),
        Err(e) => server_error(e),
    }
}

async fn mappings(s: &AppState, headers: &HeaderMap, id: i64, q: &HashMap<String, String>, kind: Kind) -> Response {
    if let Err(r) = super::auth::authorize(s, headers, None).await {
        return r;
    }
    let search = q.get("search").map(|v| v.trim()).filter(|v| !v.is_empty());
    let active = match q.get("active").map(|v| v.trim().to_ascii_lowercase()).as_deref() {
        None | Some("") | Some("all") => None,
        Some("true") | Some("1") => Some(true),
        Some("false") | Some("0") => Some(false),
        Some(_) => return field_error("active", "Use true, false or all."),
    };
    let limit = match q.get("limit").map(|v| v.trim()).filter(|v| !v.is_empty()) {
        None => 500,
        Some(v) => match v.parse::<i64>() {
            Ok(n) if (1..=5000).contains(&n) => n,
            _ => return field_error("limit", "Use a number between 1 and 5000."),
        },
    };
    match db::get_view(&s.pool, id).await {
        Ok(Some(_)) => {}
        Ok(None) => return detail(StatusCode::NOT_FOUND, NOT_FOUND),
        Err(e) => return server_error(e),
    }
    match list_mappings(&s.pool, kind, id, search, active, limit).await {
        Ok((total, items)) => Json(json!({ "total": total, "count": items.len(), "items": items })).into_response(),
        Err(e) => server_error(e),
    }
}

/// `GET /api/v1/admin/backends/{id}/users`
pub async fn users(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    mappings(&s, &headers, id, &q, Kind::Users).await
}

/// `GET /api/v1/admin/backends/{id}/agents`
pub async fn agents(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    mappings(&s, &headers, id, &q, Kind::Agents).await
}
