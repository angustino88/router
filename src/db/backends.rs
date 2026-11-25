//! The backend registry (table `backend`).

use serde::Serialize;
use sqlx::PgPool;
use utoipa::ToSchema;

use super::ts;

/// A backend as used by the sync and the CLI.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Backend {
    pub id: i64,
    pub prefix_key: String,
    pub name: String,
    pub backend_id: String,
    pub upstream_url: String,
    pub is_active: bool,
}

/// A backend as returned by the admin API.
#[derive(Debug, Clone, Serialize, ToSchema, sqlx::FromRow)]
pub struct BackendView {
    pub id: i64,
    /// Public path prefix key: the backend is reachable at `/t/<prefix_key>/`. Generated, immutable.
    pub prefix_key: String,
    pub name: String,
    /// The backend's `BACKEND_ID`, checked against `GET /internal/info`; empty = not checked.
    pub backend_id: String,
    /// `scheme://host[:port]` nginx proxies to; empty = not routed.
    pub upstream_url: String,
    pub is_active: bool,
    /// User claims currently synced from this backend.
    pub user_claims: i64,
    /// Agent claims currently synced from this backend.
    pub agent_claims: i64,
    /// Applied version of the `users` stream (`null` before the first sync).
    pub users_version: Option<i64>,
    /// Applied version of the `agents` stream (`null` before the first sync).
    pub agents_version: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
}

/// Fields of a new backend (validated by the caller).
#[derive(Debug, Clone)]
pub struct NewBackend {
    pub name: String,
    pub backend_id: String,
    pub upstream_url: String,
    pub is_active: bool,
}

/// Changed fields of a backend (`None` = unchanged).
#[derive(Debug, Clone, Default)]
pub struct BackendPatch {
    pub name: Option<String>,
    pub backend_id: Option<String>,
    pub upstream_url: Option<String>,
    pub is_active: Option<bool>,
}

const COLUMNS: &str = "id, prefix_key, name, backend_id, upstream_url, is_active";
/// Name of the unique index on `backend_id` (duplicate detection).
pub const BACKEND_ID_INDEX: &str = "backend_backend_id_key";

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

pub async fn all_backends(pool: &PgPool) -> Result<Vec<Backend>, sqlx::Error> {
    sqlx::query_as(&format!("SELECT {COLUMNS} FROM backend ORDER BY id")).fetch_all(pool).await
}

pub async fn active_backends(pool: &PgPool) -> Result<Vec<Backend>, sqlx::Error> {
    sqlx::query_as(&format!("SELECT {COLUMNS} FROM backend WHERE is_active ORDER BY id")).fetch_all(pool).await
}

/// `(prefix_key, upstream_url)` of every active backend that has a URL: the nginx map.
pub async fn routed_backends(pool: &PgPool) -> Result<Vec<(String, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT prefix_key, upstream_url FROM backend WHERE is_active AND upstream_url <> '' ORDER BY prefix_key",
    )
    .fetch_all(pool)
    .await
}

fn view_sql(filter: &str) -> String {
    format!(
        "SELECT b.id, b.prefix_key, b.name, b.backend_id, b.upstream_url, b.is_active,
                (SELECT count(*) FROM user_claim c WHERE c.backend_pk = b.id) AS user_claims,
                (SELECT count(*) FROM agent_claim c WHERE c.backend_pk = b.id) AS agent_claims,
                (SELECT version FROM sync_cursor c WHERE c.backend_pk = b.id AND c.entity = 'users') AS users_version,
                (SELECT version FROM sync_cursor c WHERE c.backend_pk = b.id AND c.entity = 'agents') AS agents_version,
                {created} AS created_at, {updated} AS updated_at
           FROM backend b {filter} ORDER BY b.id",
        created = ts("b.created_at"),
        updated = ts("b.updated_at"),
    )
}

pub async fn list_views(pool: &PgPool) -> Result<Vec<BackendView>, sqlx::Error> {
    sqlx::query_as(&view_sql("")).fetch_all(pool).await
}

pub async fn get_view(pool: &PgPool, id: i64) -> Result<Option<BackendView>, sqlx::Error> {
    sqlx::query_as(&view_sql("WHERE b.id = $1")).bind(id).fetch_optional(pool).await
}

/// Creates a backend with `prefix_key`, or a fresh random one (retried on the unlikely collision).
pub async fn create_backend(pool: &PgPool, new: &NewBackend, prefix_key: Option<&str>) -> Result<i64, sqlx::Error> {
    for _ in 0..5 {
        let key = prefix_key.map(str::to_string).unwrap_or_else(|| random_prefix_key(10));
        let id: Option<i64> = sqlx::query_scalar(
            "INSERT INTO backend (prefix_key, name, backend_id, upstream_url, is_active)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (prefix_key) DO NOTHING RETURNING id",
        )
        .bind(&key)
        .bind(&new.name)
        .bind(&new.backend_id)
        .bind(&new.upstream_url)
        .bind(new.is_active)
        .fetch_optional(pool)
        .await?;
        match (id, prefix_key) {
            (Some(id), _) => return Ok(id),
            (None, Some(k)) => return Err(sqlx::Error::Protocol(format!("prefix_key {k:?} already exists"))),
            (None, None) => continue,
        }
    }
    Err(sqlx::Error::Protocol("could not generate a unique prefix_key".into()))
}

/// Returns `false` when the backend does not exist.
pub async fn update_backend(pool: &PgPool, id: i64, p: &BackendPatch) -> Result<bool, sqlx::Error> {
    let n = sqlx::query(
        "UPDATE backend SET
                name         = COALESCE($2, name),
                backend_id   = COALESCE($3, backend_id),
                upstream_url = COALESCE($4, upstream_url),
                is_active    = COALESCE($5, is_active),
                updated_at   = now()
          WHERE id = $1",
    )
    .bind(id)
    .bind(&p.name)
    .bind(&p.backend_id)
    .bind(&p.upstream_url)
    .bind(p.is_active)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n > 0)
}

/// Deletes a backend; its claims and cursors go with it (`ON DELETE CASCADE`). `false` when it does not exist.
pub async fn delete_backend(pool: &PgPool, id: i64) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query("DELETE FROM backend WHERE id = $1").bind(id).execute(pool).await?.rows_affected() > 0)
}

pub async fn set_active(pool: &PgPool, prefix_key: &str, active: bool) -> Result<bool, sqlx::Error> {
    let n = sqlx::query("UPDATE backend SET is_active = $2, updated_at = now() WHERE prefix_key = $1")
        .bind(prefix_key)
        .bind(active)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(n > 0)
}

/// Unique index on `backend_id` violated.
pub fn is_duplicate_backend_id(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(d) if d.constraint() == Some(BACKEND_ID_INDEX))
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
