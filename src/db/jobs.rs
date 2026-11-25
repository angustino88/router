//! Schedules (`rust_periodic_task`, replaces django_celery_beat) and run history (`rust_task_run`,
//! replaces the Celery result backend).

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
        "SELECT name, enabled, interval_secs, {} AS last_run_at, {} AS updated_at FROM rust_periodic_task {filter} ORDER BY name",
        ts("last_run_at"),
        ts("updated_at")
    )
}

/// Interval of an old Celery beat schedule for `celery_task`, when the Django tables are present.
async fn celery_interval(pool: &PgPool, celery_task: &str) -> Result<Option<i64>, sqlx::Error> {
    let present: bool = sqlx::query_scalar(
        "SELECT to_regclass('django_celery_beat_periodictask') IS NOT NULL
            AND to_regclass('django_celery_beat_intervalschedule') IS NOT NULL",
    )
    .fetch_one(pool)
    .await?;
    if !present {
        return Ok(None);
    }
    let row: Option<(i32, String)> = sqlx::query_as(
        "SELECT i.every, i.period FROM django_celery_beat_periodictask p
           JOIN django_celery_beat_intervalschedule i ON i.id = p.interval_id
          WHERE p.task = $1 ORDER BY p.enabled DESC, p.id LIMIT 1",
    )
    .bind(celery_task)
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|(every, period)| {
        let unit: i64 = match period.as_str() {
            "days" => 86_400,
            "hours" => 3_600,
            "minutes" => 60,
            "seconds" => 1,
            _ => return None, // microseconds or unknown: ignore
        };
        Some(every as i64 * unit).filter(|s| *s > 0)
    }))
}

/// Creates the schedule row once (existing rows, edited on the admin page, are never overwritten).
/// The first interval comes from the old Celery beat schedule when one exists, else `default_secs`.
pub async fn seed_schedule(
    pool: &PgPool,
    name: &str,
    celery_task: Option<&str>,
    default_secs: i32,
) -> Result<(), sqlx::Error> {
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM rust_periodic_task WHERE name = $1)")
        .bind(name)
        .fetch_one(pool)
        .await?;
    if exists {
        return Ok(());
    }
    let (secs, source) = match celery_task {
        Some(t) => match celery_interval(pool, t).await? {
            Some(s) => (s.clamp(1, i32::MAX as i64) as i32, "celery beat"),
            None => (default_secs, "settings"),
        },
        None => (default_secs, "default"),
    };
    let inserted = sqlx::query(
        "INSERT INTO rust_periodic_task (name, enabled, interval_secs) VALUES ($1, true, $2) ON CONFLICT (name) DO NOTHING",
    )
    .bind(name)
    .bind(secs)
    .execute(pool)
    .await?
    .rows_affected();
    if inserted > 0 {
        tracing::info!(task = name, interval_secs = secs, source, "schedule created");
    }
    Ok(())
}

/// Names of enabled schedules whose interval has elapsed.
pub async fn due_schedules(pool: &PgPool) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT name FROM rust_periodic_task
          WHERE enabled AND (last_run_at IS NULL OR last_run_at + make_interval(secs => interval_secs) <= now())
          ORDER BY name",
    )
    .fetch_all(pool)
    .await
}

/// Atomically takes a due run: `true` for exactly one caller across all replicas.
pub async fn claim_schedule(pool: &PgPool, name: &str) -> Result<bool, sqlx::Error> {
    let n = sqlx::query(
        "UPDATE rust_periodic_task SET last_run_at = now()
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
        "UPDATE rust_periodic_task
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

pub async fn insert_run(pool: &PgPool, task: &str, trigger: &str) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("INSERT INTO rust_task_run (task, trigger, status) VALUES ($1, $2, 'running') RETURNING id")
        .bind(task)
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
    sqlx::query("UPDATE rust_task_run SET status = $2, result = $3, error = $4, finished_at = now() WHERE id = $1")
        .bind(id)
        .bind(status)
        .bind(result.map(sqlx::types::Json))
        .bind(error)
        .execute(pool)
        .await?;
    Ok(())
}

/// Keeps the newest `keep` runs of `task`.
pub async fn prune_runs(pool: &PgPool, task: &str, keep: i64) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query(
        "DELETE FROM rust_task_run WHERE task = $1
            AND id < COALESCE((SELECT id FROM rust_task_run WHERE task = $1 ORDER BY id DESC OFFSET $2 - 1 LIMIT 1), 0)",
    )
    .bind(task)
    .bind(keep.max(1))
    .execute(pool)
    .await?
    .rows_affected())
}

/// Runs still `running` when a process starts were cut off by a restart. (A run that is in fact still going on
/// another replica overwrites this when it finishes.)
pub async fn mark_interrupted_runs(pool: &PgPool) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query(
        "UPDATE rust_task_run SET status = 'failed', finished_at = now(),
                error = 'interrupted: the process stopped before the run finished'
          WHERE status = 'running'",
    )
    .execute(pool)
    .await?
    .rows_affected())
}

const RUN_COLUMNS: &str = "json_build_object(
    'id', id, 'task', task, 'trigger', trigger, 'status', status,
    'started_at', to_char(started_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'),
    'finished_at', to_char(finished_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'),
    'duration_ms', (extract(epoch FROM finished_at - started_at) * 1000)::bigint,
    'result', result, 'error', error)";

/// Latest run of `task` as JSON (`TaskRun` in the OpenAPI document).
pub async fn last_run(pool: &PgPool, task: &str) -> Result<Option<Value>, sqlx::Error> {
    let row: Option<sqlx::types::Json<Value>> = sqlx::query_scalar(&format!(
        "SELECT {RUN_COLUMNS} FROM rust_task_run WHERE task = $1 ORDER BY id DESC LIMIT 1"
    ))
    .bind(task)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|j| j.0))
}

/// Newest runs first, optionally of one task.
pub async fn list_runs(pool: &PgPool, task: Option<&str>, limit: i64) -> Result<Vec<Value>, sqlx::Error> {
    let rows: Vec<sqlx::types::Json<Value>> = sqlx::query_scalar(&format!(
        "SELECT {RUN_COLUMNS} FROM rust_task_run WHERE ($1::text IS NULL OR task = $1) ORDER BY id DESC LIMIT $2"
    ))
    .bind(task)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|j| j.0).collect())
}
