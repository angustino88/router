//! nginx routing table and reload.

use std::sync::Arc;

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Response,
};
use serde_json::{json, Value};

use super::auth::guarded;
use crate::{
    db::backends::all_backends,
    http::api::{data, ApiError},
    jobs::write_nginx_map_recorded,
    state::AppState,
};

/// `GET /admin/api/nginx`: the map nginx uses, problems found, and the last reload result.
pub async fn status(State(s): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    guarded(&s, &headers, || async {
        let backends = all_backends(&s.pool).await?;
        let entries = s.maps.entries();
        let mut problems: Vec<String> = backends
            .iter()
            .filter(|b| b.is_active && b.upstream_url.is_empty())
            .map(|b| {
                format!(
                    "Backend \"{}\" ({}) is active but has no URL: /t/{}/ answers 404.",
                    b.name, b.prefix_key, b.prefix_key
                )
            })
            .collect();
        let mut expected: Vec<(String, String)> = backends
            .iter()
            .filter(|b| b.is_active && !b.upstream_url.is_empty())
            .map(|b| (b.prefix_key.clone(), b.upstream_url.clone()))
            .collect();
        expected.sort();
        let on_disk: Vec<(String, String)> =
            entries.iter().map(|e| (e.prefix_key.clone(), e.upstream_url.clone())).collect();
        if expected != on_disk {
            problems.push(
                "The map on disk differs from the registry; it is rewritten by the next write_nginx_map run.".into(),
            );
        }
        let nginx = s.maps.nginx_status();
        if let Some(st) = &nginx {
            if st.get("ok") == Some(&Value::Bool(false)) {
                problems.push(format!(
                    "nginx reported an error ({}); it keeps the last working configuration.",
                    st.get("action").and_then(Value::as_str).unwrap_or("unknown")
                ));
            }
        }
        Ok(data(
            StatusCode::OK,
            json!({
                "map_file": s.maps.map_file().display().to_string(),
                "entries": entries,
                "problems": problems,
                "reload_pending": s.maps.reload_pending(),
                "nginx": nginx,
            }),
        ))
    })
    .await
}

/// `POST /admin/api/nginx/reload`: rewrite the map and make nginx reload even if nothing changed.
pub async fn reload(State(s): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    guarded(&s, &headers, || async {
        let map = write_nginx_map_recorded(&s.pool, &s.maps, "manual").await.unwrap_or_else(|e| json!({ "error": e }));
        s.maps.request_reload().map_err(ApiError::internal)?;
        tracing::info!("nginx reload requested from the admin page");
        Ok(data(StatusCode::ACCEPTED, json!({ "reload_requested": true, "nginx_map": map })))
    })
    .await
}
