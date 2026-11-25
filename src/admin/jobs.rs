//! Schedules (replaces Celery beat) and run history.

use std::{collections::HashMap, sync::Arc};

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

use super::fields::{authorized_body, optional_bool, optional_int};
use crate::{
    db::jobs::{self as db, Schedule},
    http::drf::{detail, field_error, server_error},
    jobs::Job,
    state::AppState,
};

pub const MIN_INTERVAL: i64 = 5;
pub const MAX_INTERVAL: i64 = 31_536_000;

async fn schedule_out(s: &AppState, sched: Schedule) -> Result<Value, sqlx::Error> {
    let running = Job::from_name(&sched.name).map(|j| s.runner.is_running(j)).unwrap_or(false);
    let last_run = db::last_run(&s.pool, &sched.name).await?;
    let mut v = serde_json::to_value(&sched).unwrap_or_else(|_| json!({}));
    v["running"] = json!(running);
    v["last_run"] = last_run.unwrap_or(Value::Null);
    Ok(v)
}

/// `GET /api/v1/admin/schedules`
pub async fn list(State(s): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(r) = super::auth::authorize(&s, &headers, None).await {
        return r;
    }
    let schedules = match db::list_schedules(&s.pool).await {
        Ok(v) => v,
        Err(e) => return server_error(e),
    };
    let mut out = Vec::with_capacity(schedules.len());
    for sched in schedules {
        match schedule_out(&s, sched).await {
            Ok(v) => out.push(v),
            Err(e) => return server_error(e),
        }
    }
    Json(json!({
        "scheduler_enabled": s.scheduler_enabled,
        "leader": s.leadership.is_leader(),
        "schedules": out,
    }))
    .into_response()
}

/// `GET /api/v1/admin/schedules/{name}`
pub async fn get_one(State(s): State<Arc<AppState>>, headers: HeaderMap, Path(name): Path<String>) -> Response {
    if let Err(r) = super::auth::authorize(&s, &headers, None).await {
        return r;
    }
    match db::get_schedule(&s.pool, &name).await {
        Ok(Some(sched)) => match schedule_out(&s, sched).await {
            Ok(v) => Json(v).into_response(),
            Err(e) => server_error(e),
        },
        Ok(None) => detail(StatusCode::NOT_FOUND, "Not found."),
        Err(e) => server_error(e),
    }
}

/// `PATCH|PUT /api/v1/admin/schedules/{name}`: `enabled`, `interval_secs`.
pub async fn update(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    body: Bytes,
) -> Response {
    let data = match authorized_body(&s, &headers, &body).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let enabled = match optional_bool(&data, "enabled") {
        Ok(v) => v,
        Err(r) => return r,
    };
    let interval = match optional_int(&data, "interval_secs", MIN_INTERVAL, MAX_INTERVAL) {
        Ok(v) => v.map(|n| n as i32),
        Err(r) => return r,
    };
    if enabled.is_none() && interval.is_none() {
        return field_error("non_field_errors", "Send enabled and/or interval_secs.");
    }
    match db::update_schedule(&s.pool, &name, enabled, interval).await {
        Ok(Some(sched)) => {
            tracing::info!(task = %name, enabled = sched.enabled, interval_secs = sched.interval_secs, "schedule updated");
            match schedule_out(&s, sched).await {
                Ok(v) => Json(v).into_response(),
                Err(e) => server_error(e),
            }
        }
        Ok(None) => detail(StatusCode::NOT_FOUND, "Not found."),
        Err(e) => server_error(e),
    }
}

/// `POST /api/v1/admin/schedules/{name}/run`
pub async fn run(State(s): State<Arc<AppState>>, headers: HeaderMap, Path(name): Path<String>) -> Response {
    if let Err(r) = super::auth::authorize(&s, &headers, None).await {
        return r;
    }
    let Some(job) = Job::from_name(&name) else {
        return detail(StatusCode::NOT_FOUND, "Not found.");
    };
    match s.runner.start(job, "manual").await {
        Ok(Some(run_id)) => (StatusCode::ACCEPTED, Json(json!({ "run_id": run_id, "task": name }))).into_response(),
        Ok(None) => detail(StatusCode::CONFLICT, "This task is already running."),
        Err(e) => server_error(e),
    }
}

/// `GET /api/v1/admin/task-runs?task=&limit=`
pub async fn runs(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Err(r) = super::auth::authorize(&s, &headers, None).await {
        return r;
    }
    let task = q.get("task").map(|t| t.trim()).filter(|t| !t.is_empty());
    let limit = match q.get("limit").map(|v| v.trim()).filter(|v| !v.is_empty()) {
        None => 50,
        Some(v) => match v.parse::<i64>() {
            Ok(n) if (1..=500).contains(&n) => n,
            _ => return field_error("limit", "Use a number between 1 and 500."),
        },
    };
    match db::list_runs(&s.pool, task, limit).await {
        Ok(runs) => Json(json!({ "runs": runs })).into_response(),
        Err(e) => server_error(e),
    }
}
