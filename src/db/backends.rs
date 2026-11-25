//! The backend registry: table `tenant_backend` plus the Router-only columns
//! `upstream_url` (where nginx sends `/t/<prefix_key>/...`) and `backend_id` (the logical name the backend
//! reports in `GET /internal/info`, routing-spec CNS-1/CNS-2).

use serde::Serialize;
use sqlx::{PgPool, Postgres, Transaction};
use utoipa::ToSchema;

use super::ts;

/// A backend as used by the sync and the CLI.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Backend {
    pub id: i64,
    pub prefix_key: String,
    pub name: String,
    pub is_active: bool,
    #[allow(dead_code)]
    pub computer_sync_version: i32,
    pub upstream_url: String,
    /// Expected `backend_id` of `GET /internal/info`; empty = not verified.
    pub backend_id: String,
}

/// A backend as shown by the admin API.
#[derive(Debug, Clone, Serialize, ToSchema, sqlx::FromRow)]
pub struct BackendView {
    pub id: i64,
    /// Path prefix key: the backend is reachable at `/t/<prefix_key>/`. Generated, never changes.
    pub prefix_key: String,
    pub name: String,
    pub is_active: bool,
    pub computer_sync_version: i32,
    /// `scheme://host[:port]` nginx proxies to; empty = not routed by the generated map.
    pub upstream_url: String,
    /// Logical backend id (`BACKEND_ID` of the backend); checked against `GET /internal/info`.
    pub backend_id: String,
    pub created: String,
    pub modified: String,
    /// Active user claims stored for this backend.
    pub users_active: i64,
    /// Active agent claims stored for this backend.
    pub agents_active: i64,
    /// Version of the `users` stream applied so far (`null` before the first sync).
    pub users_version: Option<i64>,
    /// Version of the `agents` stream applied so far (`null` before the first sync).
    pub agents_version: Option<i64>,
}

/// Rows removed by deleting a backend.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
pub struct DeleteCounts {
    pub backend: u64,
    pub users: u64,
    pub agents: u64,
}

/// Fields of a new backend (validated by the caller).
#[derive(Debug, Clone)]
pub struct NewBackend {
    pub name: String,
    pub upstream_url: String,
    pub backend_id: String,
    pub is_active: bool,
    pub computer_sync_version: i32,
}

/// Changed fields of a backend (`None` = unchanged).
#[derive(Debug, Clone, Default)]
pub struct BackendPatch {
    pub name: Option<String>,
    pub upstream_url: Option<String>,
    pub backend_id: Option<String>,
    pub is_active: Option<bool>,
    pub computer_sync_version: Option<i32>,
}

const BACKEND_COLUMNS: &str = "id, prefix_key, name, is_active, computer_sync_version, upstream_url, backend_id";

/// Letters, digits, `_` and `-`, 1 to 100 characters: safe in a URL path and as an nginx map key.
pub fn valid_prefix_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 100 && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Random key of `len` characters `a-z0-9`.
pub fn random_prefix_key(len: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut out = String::with_capacity(len);
    while out.len() < len {
        for b in uuid::Uuid::new_v4().as_bytes() {
            // 252 = 7 * 36: reject the top values so every character is equally likely.
            if *b < 252 && out.len() < len {
                out.push(ALPHABET[(*b % 36) as usize] as char);
            }
        }
    }
    out
}

pub async fn prefix_key_exists(pool: &PgPool, key: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM tenant_backend WHERE prefix_key = $1)")
        .bind(key)
        .fetch_one(pool)
        .await
}

pub async fn insert_backend(pool: &PgPool, key: &str, name: &str, active: bool) -> Result<Backend, sqlx::Error> {
    sqlx::query_as(&format!(
        "INSERT INTO tenant_backend (created, modified, prefix_key, name, is_active, computer_sync_version)
         VALUES (now(), now(), $1, $2, $3, 0) RETURNING {BACKEND_COLUMNS}"
    ))
    .bind(key)
    .bind(name)
    .bind(active)
    .fetch_one(pool)
    .await
}

pub async fn all_backends(pool: &PgPool) -> Result<Vec<Backend>, sqlx::Error> {
    sqlx::query_as(&format!("SELECT {BACKEND_COLUMNS} FROM tenant_backend ORDER BY id")).fetch_all(pool).await
}

pub async fn active_backends(pool: &PgPool) -> Result<Vec<Backend>, sqlx::Error> {
    sqlx::query_as(&format!("SELECT {BACKEND_COLUMNS} FROM tenant_backend WHERE is_active ORDER BY id"))
        .fetch_all(pool)
        .await
}

pub async fn set_backend_active(pool: &PgPool, key: &str, active: bool) -> Result<bool, sqlx::Error> {
    let n = sqlx::query("UPDATE tenant_backend SET is_active = $2, modified = now() WHERE prefix_key = $1")
        .bind(key)
        .bind(active)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(n > 0)
}

/// `(prefix_key, upstream_url)` of every active backend that has a URL: the generated nginx map.
pub async fn routed_backends(pool: &PgPool) -> Result<Vec<(String, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT prefix_key, upstream_url FROM tenant_backend
          WHERE is_active AND upstream_url <> '' ORDER BY prefix_key",
    )
    .fetch_all(pool)
    .await
}

fn view_sql(filter: &str) -> String {
    format!(
        "SELECT b.id, b.prefix_key, b.name, b.is_active, b.computer_sync_version, b.upstream_url, b.backend_id,
                {created} AS created, {modified} AS modified,
                (SELECT count(*) FROM tenant_usertenantresolver u WHERE u.backend_id = b.id AND u.is_active) AS users_active,
                (SELECT count(*) FROM tenant_agenttenantresolver a WHERE a.backend_id = b.id AND a.is_active) AS agents_active,
                (SELECT c.version FROM rust_sync_cursor c WHERE c.backend_id = b.id AND c.entity = 'users') AS users_version,
                (SELECT c.version FROM rust_sync_cursor c WHERE c.backend_id = b.id AND c.entity = 'agents') AS agents_version
           FROM tenant_backend b {filter} ORDER BY b.id",
        created = ts("b.created"),
        modified = ts("b.modified"),
    )
}

pub async fn list_views(pool: &PgPool) -> Result<Vec<BackendView>, sqlx::Error> {
    sqlx::query_as(&view_sql("")).fetch_all(pool).await
}

pub async fn get_view(pool: &PgPool, id: i64) -> Result<Option<BackendView>, sqlx::Error> {
    sqlx::query_as(&view_sql("WHERE b.id = $1")).bind(id).fetch_optional(pool).await
}

/// Creates a backend with a fresh random `prefix_key` (retried on the unlikely collision).
pub async fn create_backend(pool: &PgPool, new: &NewBackend) -> Result<BackendView, sqlx::Error> {
    let mut attempts = 0;
    let id: i64 = loop {
        let key = random_prefix_key(10);
        let res = sqlx::query_scalar(
            "INSERT INTO tenant_backend
                    (created, modified, prefix_key, name, is_active, computer_sync_version, upstream_url, backend_id)
             VALUES (now(), now(), $1, $2, $3, $4, $5, $6)
             ON CONFLICT (prefix_key) DO NOTHING RETURNING id",
        )
        .bind(&key)
        .bind(&new.name)
        .bind(new.is_active)
        .bind(new.computer_sync_version)
        .bind(&new.upstream_url)
        .bind(&new.backend_id)
        .fetch_optional(pool)
        .await?;
        match res {
            Some(id) => break id,
            None if attempts < 5 => attempts += 1,
            None => return Err(sqlx::Error::Protocol("could not generate a unique prefix_key".into())),
        }
    };
    get_view(pool, id).await?.ok_or(sqlx::Error::RowNotFound)
}

pub async fn update_backend(pool: &PgPool, id: i64, p: &BackendPatch) -> Result<Option<BackendView>, sqlx::Error> {
    let n = sqlx::query(
        "UPDATE tenant_backend SET
                name                  = COALESCE($2, name),
                upstream_url          = COALESCE($3, upstream_url),
                backend_id            = COALESCE($4, backend_id),
                is_active             = COALESCE($5, is_active),
                computer_sync_version = COALESCE($6, computer_sync_version),
                modified              = now()
          WHERE id = $1",
    )
    .bind(id)
    .bind(&p.name)
    .bind(&p.upstream_url)
    .bind(&p.backend_id)
    .bind(p.is_active)
    .bind(p.computer_sync_version)
    .execute(pool)
    .await?
    .rows_affected();
    if n == 0 {
        return Ok(None);
    }
    get_view(pool, id).await
}

/// Deletes a backend and its mappings in one transaction. `None` when it does not exist.
pub async fn delete_backend(pool: &PgPool, id: i64) -> Result<Option<DeleteCounts>, sqlx::Error> {
    let mut tx: Transaction<'_, Postgres> = pool.begin().await?;
    let exists: Option<i64> = sqlx::query_scalar("SELECT id FROM tenant_backend WHERE id = $1 FOR UPDATE")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    if exists.is_none() {
        tx.rollback().await?;
        return Ok(None);
    }
    let users = sqlx::query("DELETE FROM tenant_usertenantresolver WHERE backend_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    let agents = sqlx::query("DELETE FROM tenant_agenttenantresolver WHERE backend_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    // rust_sync_cursor rows go with ON DELETE CASCADE.
    let backend =
        sqlx::query("DELETE FROM tenant_backend WHERE id = $1").bind(id).execute(&mut *tx).await?.rows_affected();
    tx.commit().await?;
    Ok(Some(DeleteCounts { backend, users, agents }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_keys() {
        for _ in 0..50 {
            let k = random_prefix_key(10);
            assert_eq!(k.len(), 10);
            assert!(k.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
            assert!(valid_prefix_key(&k));
        }
        assert!(valid_prefix_key("dept_a-1"));
        assert!(!valid_prefix_key(""));
        assert!(!valid_prefix_key("a b"));
        assert!(!valid_prefix_key("a;b"));
        assert!(!valid_prefix_key(&"x".repeat(101)));
    }
}
