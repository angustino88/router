//! nginx routing table, problems and forced reload.

use std::{collections::HashSet, sync::Arc};

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

use crate::{
    db::backends::all_backends,
    http::api::{detail, server_error},
    jobs::render_nginx_map_recorded,
    state::AppState,
};

/// `GET /api/v1/admin/nginx`
pub async fn status(State(s): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(r) = super::auth::authorize(&s, &headers, None).await {
        return r;
    }
    let backends = match all_backends(&s.pool).await {
        Ok(b) => b,
        Err(e) => return server_error(e),
    };
    let git = s.maps.git_entries();
    let generated = s.maps.generated_entries();
    let git_keys: HashSet<&str> = git.iter().map(|e| e.prefix_key.as_str()).collect();
    let known: HashSet<&str> = backends.iter().map(|b| b.prefix_key.as_str()).collect();

    let mut problems: Vec<String> = Vec::new();
    for b in backends.iter().filter(|b| b.is_active) {
        if b.upstream_url.is_empty() && !git_keys.contains(b.prefix_key.as_str()) {
            problems.push(format!(
                "Backend \"{}\" ({}) is active but has no URL: nginx answers 404 for /t/{}/.",
                b.name, b.prefix_key, b.prefix_key
            ));
        }
    }
    for e in &git {
        if !known.contains(e.prefix_key.as_str()) {
            problems.push(format!(
                "The static backends.map routes {}, which is not a backend in the database.",
                e.prefix_key
            ));
        }
    }
    let mut expected: Vec<(String, String)> = backends
        .iter()
        .filter(|b| b.is_active && !b.upstream_url.is_empty() && !git_keys.contains(b.prefix_key.as_str()))
        .map(|b| (b.prefix_key.clone(), b.upstream_url.clone()))
        .collect();
    expected.sort();
    let mut on_disk: Vec<(String, String)> =
        generated.iter().map(|e| (e.prefix_key.clone(), e.target.clone())).collect();
    on_disk.sort();
    if expected != on_disk {
        problems.push(
            "The generated map differs from the database; it is rewritten by the next render_nginx_map run or by Force reload."
                .into(),
        );
    }
    let nginx_status = s.maps.nginx_status();
    if let Some(st) = &nginx_status {
        if st.get("ok") == Some(&Value::Bool(false)) {
            problems.push(format!(
                "nginx reported an error ({}); it keeps the last working configuration.",
                st.get("action").and_then(Value::as_str).unwrap_or("unknown")
            ));
        }
    }

    Json(json!({
        "git_file": s.maps.git_file().display().to_string(),
        "generated_file": s.maps.generated_file().display().to_string(),
        "entries": s.maps.routing_table(),
        "problems": problems,
        "reload_pending": s.maps.reload_pending(),
        "nginx_status": nginx_status,
    }))
    .into_response()
}

/// `POST /api/v1/admin/nginx/reload`: regenerate the map and make nginx reload even if nothing changed.
pub async fn reload(State(s): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(r) = super::auth::authorize(&s, &headers, None).await {
        return r;
    }
    let map = match render_nginx_map_recorded(&s.pool, &s.maps, "force").await {
        Ok(v) => v,
        Err(e) => json!({ "error": e }),
    };
    if let Err(e) = s.maps.request_reload() {
        tracing::error!("cannot request an nginx reload: {e}");
        return detail(StatusCode::INTERNAL_SERVER_ERROR, format!("Cannot request the reload: {e}"));
    }
    tracing::info!("nginx reload requested from the admin page");
    (StatusCode::ACCEPTED, Json(json!({ "reload_requested": true, "nginx_map": map }))).into_response()
}
