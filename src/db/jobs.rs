//! Job schedules (`job_schedule`) and run history (`job_run`).

use serde::Serialize;
use serde_json::Value;
use sqlx::PgPool;

use super::ts;

/// One schedule row.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Schedule {
    pub name: String,
    pub enabled: bool,
    pub interval_secs: i32,
    pub last_run_at: Option<String>,
    pub updated_at: String,
}

fn schedule_sql(filter: &str) -> String {
    format!(
        "SELECT name, enabled, interval_secs, {} AS last_run_at, {} AS updated_at FROM job_schedule {filter} ORDER BY name",
        ts("last_run_at"),
        ts("updated_at")
    )
}

/// Creates the schedule row with `interval_secs` unless it exists (rows edited on the admin page are kept).
pub async fn seed_schedule(pool: &PgPool, name: &str, interval_secs: i32) -> Result<(), sqlx::Error> {
    let inserted =
        sqlx::query("INSERT INTO job_schedule (name, interval_secs) VALUES ($1, $2) ON CONFLICT (name) DO NOTHING")
            .bind(name)
            .bind(interval_secs)
            .execute(pool)
            .await?
            .rows_affected();
    if inserted > 0 {
        tracing::info!(job = name, interval_secs, "schedule created");
    }
    Ok(())
}

/// Names of enabled schedules whose interval has elapsed.
pub async fn due_schedules(pool: &PgPool) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT name FROM job_schedule
          WHERE enabled AND (last_run_at IS NULL OR last_run_at + make_interval(secs => interval_secs) <= now())
          ORDER BY name",
    )
    .fetch_all(pool)
    .await
}

/// Atomically takes a due run: `true` for exactly one caller across all replicas.
pub async fn claim_schedule(pool: &PgPool, name: &str) -> Result<bool, sqlx::Error> {
    let n = sqlx::query(
        "UPDATE job_schedule SET last_run_at = now()
          WHERE name = $1 AND enabled
            AND (last_run_at IS NULL OR last_run_at + make_interval(secs => interval_secs) <= now())",
    )
    .bind(name)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n == 1)
}

pub async fn list_schedules(pool: &PgPool) -> Result<Vec<Schedule>, sqlx::Error> {
    sqlx::query_as(&schedule_sql("")).fetch_all(pool).await
}

pub async fn get_schedule(pool: &PgPool, name: &str) -> Result<Option<Schedule>, sqlx::Error> {
    sqlx::query_as(&schedule_sql("WHERE name = $1")).bind(name).fetch_optional(pool).await
}

pub async fn update_schedule(
    pool: &PgPool,
    name: &str,
    enabled: Option<bool>,
    interval_secs: Option<i32>,
) -> Result<Option<Schedule>, sqlx::Error> {
    let n = sqlx::query(
        "UPDATE job_schedule
            SET enabled = COALESCE($2, enabled), interval_secs = COALESCE($3, interval_secs), updated_at = now()
          WHERE name = $1",
    )
    .bind(name)
    .bind(enabled)
    .bind(interval_secs)
    .execute(pool)
    .await?
    .rows_affected();
    if n == 0 {
        return Ok(None);
    }
    get_schedule(pool, name).await
}

pub async fn insert_run(pool: &PgPool, job: &str, trigger: &str) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("INSERT INTO job_run (job, trigger, status) VALUES ($1, $2, 'running') RETURNING id")
        .bind(job)
        .bind(trigger)
        .fetch_one(pool)
        .await
}

pub async fn finish_run(
    pool: &PgPool,
    id: i64,
    status: &str,
    result: Option<&Value>,
    error: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE job_run SET status = $2, result = $3, error = $4, finished_at = now() WHERE id = $1")
        .bind(id)
        .bind(status)
        .bind(result.map(sqlx::types::Json))
        .bind(error)
        .execute(pool)
        .await?;
    Ok(())
}

/// Keeps the newest `keep` runs of `job`.
pub async fn prune_runs(pool: &PgPool, job: &str, keep: i64) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query(
        "DELETE FROM job_run WHERE job = $1
            AND id < COALESCE((SELECT id FROM job_run WHERE job = $1 ORDER BY id DESC OFFSET $2 - 1 LIMIT 1), 0)",
    )
    .bind(job)
    .bind(keep.max(1))
    .execute(pool)
    .await?
    .rows_affected())
}

/// Runs still `running` when a process starts were cut off by a restart. (A run that is in fact still going on
/// another replica overwrites this when it finishes.)
pub async fn mark_interrupted_runs(pool: &PgPool) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query(
        "UPDATE job_run SET status = 'failed', finished_at = now(),
                error = 'interrupted: the process stopped before the run finished'
          WHERE status = 'running'",
    )
    .execute(pool)
    .await?
    .rows_affected())
}

const RUN_JSON: &str = "json_build_object(
    'id', id, 'job', job, 'trigger', trigger, 'status', status,
    'started_at', to_char(started_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'),
    'finished_at', to_char(finished_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'),
    'duration_ms', (extract(epoch FROM finished_at - started_at) * 1000)::bigint,
    'result', result, 'error', error)";

/// One run as JSON (`JobRun` in the OpenAPI document).
pub async fn get_run(pool: &PgPool, id: i64) -> Result<Option<Value>, sqlx::Error> {
    let row: Option<sqlx::types::Json<Value>> =
        sqlx::query_scalar(&format!("SELECT {RUN_JSON} FROM job_run WHERE id = $1"))
            .bind(id)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|j| j.0))
}

/// Latest run of `job`.
pub async fn last_run(pool: &PgPool, job: &str) -> Result<Option<Value>, sqlx::Error> {
    let row: Option<sqlx::types::Json<Value>> =
        sqlx::query_scalar(&format!("SELECT {RUN_JSON} FROM job_run WHERE job = $1 ORDER BY id DESC LIMIT 1"))
            .bind(job)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|j| j.0))
}

/// Newest runs first, optionally of one job.
pub async fn list_runs(pool: &PgPool, job: Option<&str>, limit: i64) -> Result<Vec<Value>, sqlx::Error> {
    let rows: Vec<sqlx::types::Json<Value>> = sqlx::query_scalar(&format!(
        "SELECT {RUN_JSON} FROM job_run WHERE ($1::text IS NULL OR job = $1) ORDER BY id DESC LIMIT $2"
    ))
    .bind(job)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|j| j.0).collect())
}
