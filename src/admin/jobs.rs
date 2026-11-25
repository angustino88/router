//! Jobs (schedule, run now) and run history.

use std::{collections::HashMap, sync::Arc};

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::Response,
};
use serde_json::{json, Value};

use super::{auth::guarded, API};
use crate::{
    db::jobs::{self as db, Schedule},
    http::api::{data, json_body, ApiError, Fields},
    jobs::Job,
    state::AppState,
};

pub const MIN_INTERVAL: i64 = 5;
pub const MAX_INTERVAL: i64 = 31_536_000;

/// A schedule plus whether it runs now and its last run (`Job` in the OpenAPI document).
async fn job_out(s: &AppState, sched: Schedule) -> Result<Value, sqlx::Error> {
    let running = Job::from_name(&sched.name).is_some_and(|j| s.runner.is_running(j));
    let last_run = db::last_run(&s.pool, &sched.name).await?;
    let mut v = serde_json::to_value(&sched).unwrap_or_else(|_| json!({}));
    v["running"] = json!(running);
    v["last_run"] = last_run.unwrap_or(Value::Null);
    Ok(v)
}

/// `GET /admin/api/jobs`
pub async fn list(State(s): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    guarded(&s, &headers, || async {
        let mut out = Vec::new();
        for sched in db::list_schedules(&s.pool).await? {
            out.push(job_out(&s, sched).await?);
        }
        Ok(data(StatusCode::OK, out))
    })
    .await
}

/// `GET /admin/api/jobs/{name}`
pub async fn get_one(State(s): State<Arc<AppState>>, headers: HeaderMap, Path(name): Path<String>) -> Response {
    guarded(&s, &headers, || async {
        let sched = db::get_schedule(&s.pool, &name).await?.ok_or_else(ApiError::not_found)?;
        Ok(data(StatusCode::OK, job_out(&s, sched).await?))
    })
    .await
}

/// `PATCH /admin/api/jobs/{name}`: `enabled`, `interval_secs`.
pub async fn update(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    body: Bytes,
) -> Response {
    guarded(&s, &headers, || async {
        let body = json_body(&headers, &body)?;
        let mut f = Fields::new(&body);
        let enabled = f.boolean("enabled")?;
        let interval = f.integer("interval_secs", MIN_INTERVAL, MAX_INTERVAL)?.map(|n| n as i32);
        f.finish()?;
        let sched = db::update_schedule(&s.pool, &name, enabled, interval).await?.ok_or_else(ApiError::not_found)?;
        tracing::info!(job = %name, enabled = sched.enabled, interval_secs = sched.interval_secs, "schedule updated");
        Ok(data(StatusCode::OK, job_out(&s, sched).await?))
    })
    .await
}

/// `POST /admin/api/jobs/{name}/run`: `202` with the run, `409` when the job is already running.
pub async fn run(State(s): State<Arc<AppState>>, headers: HeaderMap, Path(name): Path<String>) -> Response {
    guarded(&s, &headers, || async {
        let job = Job::from_name(&name).ok_or_else(ApiError::not_found)?;
        let run_id =
            s.runner.start(job, "manual").await?.ok_or_else(|| {
                ApiError::new(StatusCode::CONFLICT, "already_running", "This job is already running.")
            })?;
        let mut r = data(StatusCode::ACCEPTED, json!({ "run_id": run_id, "job": name }));
        if let Ok(location) = HeaderValue::from_str(&format!("{API}/runs/{run_id}")) {
            r.headers_mut().insert(header::LOCATION, location);
        }
        Ok(r)
    })
    .await
}

/// `GET /admin/api/runs?job=&limit=`
pub async fn runs(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    guarded(&s, &headers, || async {
        if let Some(unknown) = q.keys().find(|k| !matches!(k.as_str(), "job" | "limit")) {
            return Err(ApiError::field(unknown, "unknown_parameter", "Unknown query parameter."));
        }
        let job = q.get("job").map(|t| t.trim()).filter(|t| !t.is_empty());
        let limit = match q.get("limit") {
            None => 50,
            Some(v) => match v.trim().parse::<i64>() {
                Ok(n) if (1..=500).contains(&n) => n,
                _ => return Err(ApiError::field("limit", "out_of_range", "Must be between 1 and 500.")),
            },
        };
        Ok(data(StatusCode::OK, db::list_runs(&s.pool, job, limit).await?))
    })
    .await
}

/// `GET /admin/api/runs/{id}`
pub async fn run_one(State(s): State<Arc<AppState>>, headers: HeaderMap, Path(id): Path<i64>) -> Response {
    guarded(&s, &headers, || async {
        Ok(data(StatusCode::OK, db::get_run(&s.pool, id).await?.ok_or_else(ApiError::not_found)?))
    })
    .await
}
