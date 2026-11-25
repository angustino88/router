//! Backend registry and the read-only claim lists.

use std::{collections::HashMap, sync::Arc};

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::Response,
};
use serde_json::{json, Value};

use super::{auth::guarded, fields, API};
use crate::{
    db::{
        backends::{self as db, BackendPatch, NewBackend},
        claims, Kind,
    },
    http::api::{data, data_with_meta, json_body, no_content, ApiError, ApiResult, Fields},
    jobs::write_nginx_map_recorded,
    state::AppState,
};

fn validated(field: &str, r: Result<String, String>) -> Result<String, ApiError> {
    r.map_err(|m| ApiError::field(field, "invalid", m))
}

fn duplicate(e: sqlx::Error) -> ApiError {
    if db::is_duplicate_backend_id(&e) {
        ApiError::field("backend_id", "duplicate", "Another backend already uses this backend_id.")
    } else {
        ApiError::internal(e)
    }
}

/// Rewrites the nginx map after a change; the outcome is returned as `meta.nginx_map`.
async fn regenerate(s: &AppState) -> Value {
    match write_nginx_map_recorded(&s.pool, &s.maps, "admin").await {
        Ok(v) => v,
        Err(e) => json!({ "error": e }),
    }
}

/// `GET /admin/api/backends`
pub async fn list(State(s): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    guarded(&s, &headers, || async { Ok(data(StatusCode::OK, db::list_views(&s.pool).await?)) }).await
}

/// `GET /admin/api/backends/{id}`
pub async fn get_one(State(s): State<Arc<AppState>>, headers: HeaderMap, Path(id): Path<i64>) -> Response {
    guarded(&s, &headers, || async {
        Ok(data(StatusCode::OK, db::get_view(&s.pool, id).await?.ok_or_else(ApiError::not_found)?))
    })
    .await
}

/// `POST /admin/api/backends`: `name` (required), `backend_id`, `upstream_url`, `is_active`.
pub async fn create(State(s): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    guarded(&s, &headers, || async {
        let body = json_body(&headers, &body)?;
        let mut f = Fields::new(&body);
        let new = NewBackend {
            name: validated("name", fields::backend_name(&f.required_string("name")?))?,
            backend_id: validated("backend_id", fields::backend_id(&f.string("backend_id")?.unwrap_or_default()))?,
            upstream_url: validated(
                "upstream_url",
                fields::upstream_url(&f.string("upstream_url")?.unwrap_or_default()),
            )?,
            is_active: f.boolean("is_active")?.unwrap_or(true),
        };
        f.finish()?;
        let id = db::create_backend(&s.pool, &new, None).await.map_err(duplicate)?;
        let view = db::get_view(&s.pool, id).await?.ok_or_else(ApiError::not_found)?;
        tracing::info!(prefix_key = %view.prefix_key, name = %view.name, "backend created");
        let mut r = data_with_meta(StatusCode::CREATED, view, json!({ "nginx_map": regenerate(&s).await }));
        if let Ok(location) = HeaderValue::from_str(&format!("{API}/backends/{id}")) {
            r.headers_mut().insert(header::LOCATION, location);
        }
        Ok(r)
    })
    .await
}

/// `PATCH /admin/api/backends/{id}`: any of `name`, `backend_id`, `upstream_url` (`""` = not routed), `is_active`.
pub async fn update(State(s): State<Arc<AppState>>, headers: HeaderMap, Path(id): Path<i64>, body: Bytes) -> Response {
    guarded(&s, &headers, || async {
        let body = json_body(&headers, &body)?;
        let mut f = Fields::new(&body);
        let patch = BackendPatch {
            name: f.string("name")?.map(|n| validated("name", fields::backend_name(&n))).transpose()?,
            backend_id: f.string("backend_id")?.map(|b| validated("backend_id", fields::backend_id(&b))).transpose()?,
            upstream_url: f
                .string("upstream_url")?
                .map(|u| validated("upstream_url", fields::upstream_url(&u)))
                .transpose()?,
            is_active: f.boolean("is_active")?,
        };
        f.finish()?;
        if !db::update_backend(&s.pool, id, &patch).await.map_err(duplicate)? {
            return Err(ApiError::not_found());
        }
        let view = db::get_view(&s.pool, id).await?.ok_or_else(ApiError::not_found)?;
        tracing::info!(prefix_key = %view.prefix_key, "backend updated");
        Ok(data_with_meta(StatusCode::OK, view, json!({ "nginx_map": regenerate(&s).await })))
    })
    .await
}

/// `DELETE /admin/api/backends/{id}`: the backend, its claims and cursors.
pub async fn delete(State(s): State<Arc<AppState>>, headers: HeaderMap, Path(id): Path<i64>) -> Response {
    guarded(&s, &headers, || async {
        if !db::delete_backend(&s.pool, id).await? {
            return Err(ApiError::not_found());
        }
        tracing::warn!(id, "backend deleted");
        regenerate(&s).await;
        Ok(no_content())
    })
    .await
}

async fn claims_of(s: &AppState, id: i64, q: &HashMap<String, String>, kind: Kind) -> ApiResult {
    if let Some(unknown) = q.keys().find(|k| !matches!(k.as_str(), "search" | "limit")) {
        return Err(ApiError::field(unknown, "unknown_parameter", "Unknown query parameter."));
    }
    let search = q.get("search").map(|v| v.trim()).filter(|v| !v.is_empty());
    let limit = match q.get("limit") {
        None => 500,
        Some(v) => match v.trim().parse::<i64>() {
            Ok(n) if (1..=5000).contains(&n) => n,
            _ => return Err(ApiError::field("limit", "out_of_range", "Must be between 1 and 5000.")),
        },
    };
    db::get_view(&s.pool, id).await?.ok_or_else(ApiError::not_found)?;
    let (total, items) = claims::list(&s.pool, kind, id, search, limit).await?;
    Ok(data_with_meta(StatusCode::OK, items, json!({ "total": total })))
}

/// `GET /admin/api/backends/{id}/users?search=&limit=`
pub async fn users(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    guarded(&s, &headers, || claims_of(&s, id, &q, Kind::Users)).await
}

/// `GET /admin/api/backends/{id}/agents?search=&limit=`
pub async fn agents(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    guarded(&s, &headers, || claims_of(&s, id, &q, Kind::Agents)).await
}
