//! Claims synced from the backends' `users` and `agents` streams, their stream cursors, and the routing
//! lookups built on them.
//!
//! Writes are atomic per stream: a snapshot or a delta page is applied together with its cursor in one
//! transaction, so a crash never leaves claims and cursor out of step.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{PgPool, Postgres, Transaction};
use utoipa::ToSchema;

use super::{ts, Kind};

/// Position in one backend stream: the history (`epoch`) and the last applied `version`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub epoch: String,
    pub version: i64,
}

/// Counts of one applied snapshot or delta page.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApplyCounts {
    pub received: usize,
    pub added: u64,
    pub removed: u64,
}

/// A backend a key is routed to.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct Route {
    /// The backend's logical id (`BACKEND_ID`); empty when not registered.
    pub backend_id: String,
    /// Display name of the backend.
    pub name: String,
    /// Path prefix of the backend on the Router: `/t/<prefix_key>`.
    pub prefix: String,
}

// ------------------------------------------------------------------ cursors

pub async fn get_cursor(pool: &PgPool, backend_pk: i64, kind: Kind) -> Result<Option<Cursor>, sqlx::Error> {
    let row: Option<(String, i64)> =
        sqlx::query_as("SELECT epoch, version FROM sync_cursor WHERE backend_pk = $1 AND entity = $2")
            .bind(backend_pk)
            .bind(kind.entity())
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(epoch, version)| Cursor { epoch, version }))
}

/// Locks the stream for the rest of the transaction (even before its cursor row exists) and reads the cursor.
async fn lock_cursor(
    tx: &mut Transaction<'_, Postgres>,
    backend_pk: i64,
    kind: Kind,
) -> Result<Option<Cursor>, sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock($1, hashtext($2))")
        .bind(backend_pk as i32)
        .bind(kind.entity())
        .execute(&mut **tx)
        .await?;
    let row: Option<(String, i64)> =
        sqlx::query_as("SELECT epoch, version FROM sync_cursor WHERE backend_pk = $1 AND entity = $2 FOR UPDATE")
            .bind(backend_pk)
            .bind(kind.entity())
            .fetch_optional(&mut **tx)
            .await?;
    Ok(row.map(|(epoch, version)| Cursor { epoch, version }))
}

async fn save_cursor(
    tx: &mut Transaction<'_, Postgres>,
    backend_pk: i64,
    kind: Kind,
    c: &Cursor,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO sync_cursor (backend_pk, entity, epoch, version) VALUES ($1, $2, $3, $4)
         ON CONFLICT (backend_pk, entity)
         DO UPDATE SET epoch = EXCLUDED.epoch, version = EXCLUDED.version, updated_at = now()",
    )
    .bind(backend_pk)
    .bind(kind.entity())
    .bind(&c.epoch)
    .bind(c.version)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn insert_keys(
    tx: &mut Transaction<'_, Postgres>,
    backend_pk: i64,
    kind: Kind,
    keys: &[String],
) -> Result<u64, sqlx::Error> {
    if keys.is_empty() {
        return Ok(0);
    }
    Ok(sqlx::query(&format!(
        "INSERT INTO {} (backend_pk, key) SELECT $1, k FROM UNNEST($2::text[]) AS k ON CONFLICT DO NOTHING",
        kind.table()
    ))
    .bind(backend_pk)
    .bind(keys)
    .execute(&mut **tx)
    .await?
    .rows_affected())
}

// ------------------------------------------------------------------ stream writes

/// Publishes a full snapshot: exactly `keys` are claimed and the cursor is `cursor`, in one transaction.
/// `None` (nothing written) when the stored cursor is already further along the same epoch, i.e. a
/// concurrent delta is newer than this snapshot.
pub async fn replace_all(
    pool: &PgPool,
    backend_pk: i64,
    kind: Kind,
    keys: &[String],
    cursor: &Cursor,
) -> Result<Option<ApplyCounts>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    if let Some(current) = lock_cursor(&mut tx, backend_pk, kind).await? {
        if current.epoch == cursor.epoch && current.version > cursor.version {
            tx.rollback().await?;
            return Ok(None);
        }
    }
    let removed =
        sqlx::query(&format!("DELETE FROM {} WHERE backend_pk = $1 AND NOT (key = ANY($2::text[]))", kind.table()))
            .bind(backend_pk)
            .bind(keys)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    let added = insert_keys(&mut tx, backend_pk, kind, keys).await?;
    save_cursor(&mut tx, backend_pk, kind, cursor).await?;
    tx.commit().await?;
    Ok(Some(ApplyCounts { received: keys.len(), added, removed }))
}

/// Applies one change page and moves the cursor from `from` to `to`, in one transaction.
/// `None` (nothing written) when the stored cursor is no longer `from` (a resync moved it).
pub async fn apply_delta(
    pool: &PgPool,
    backend_pk: i64,
    kind: Kind,
    from: &Cursor,
    upserts: &[String],
    deletes: &[String],
    to: &Cursor,
) -> Result<Option<ApplyCounts>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    if lock_cursor(&mut tx, backend_pk, kind).await?.as_ref() != Some(from) {
        tx.rollback().await?;
        return Ok(None);
    }
    let added = insert_keys(&mut tx, backend_pk, kind, upserts).await?;
    let removed = if deletes.is_empty() {
        0
    } else {
        sqlx::query(&format!("DELETE FROM {} WHERE backend_pk = $1 AND key = ANY($2::text[])", kind.table()))
            .bind(backend_pk)
            .bind(deletes)
            .execute(&mut *tx)
            .await?
            .rows_affected()
    };
    save_cursor(&mut tx, backend_pk, kind, to).await?;
    tx.commit().await?;
    Ok(Some(ApplyCounts { received: upserts.len() + deletes.len(), added, removed }))
}

/// Every key claimed by one backend (digest checksum).
pub async fn keys(pool: &PgPool, backend_pk: i64, kind: Kind) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar(&format!("SELECT key FROM {} WHERE backend_pk = $1", kind.table()))
        .bind(backend_pk)
        .fetch_all(pool)
        .await
}

// ------------------------------------------------------------------ routing lookups

/// Active backends that claim `key` (canonical form). More than one is a conflict for the caller to refuse.
pub async fn routes_for(pool: &PgPool, kind: Kind, key: &str) -> Result<Vec<Route>, sqlx::Error> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(&format!(
        "SELECT b.backend_id, b.name, b.prefix_key FROM {} c JOIN backend b ON b.id = c.backend_pk
          WHERE c.key = $1 AND b.is_active ORDER BY b.id",
        kind.table()
    ))
    .bind(key)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(backend_id, name, prefix_key)| Route { backend_id, name, prefix: format!("/t/{prefix_key}") })
        .collect())
}

/// `Some("xxxxxxxx-xxxx-...")` (lowercase) for a UUID in any accepted spelling, else `None`.
pub fn canonical_uuid(raw: &str) -> Option<String> {
    uuid::Uuid::parse_str(raw.trim()).ok().map(|u| u.hyphenated().to_string())
}

// ------------------------------------------------------------------ admin views

/// Claims of one backend, filtered by a case-insensitive substring. Returns `(total matching, rows)`.
pub async fn list(
    pool: &PgPool,
    kind: Kind,
    backend_pk: i64,
    search: Option<&str>,
    limit: i64,
) -> Result<(i64, Vec<Value>), sqlx::Error> {
    let table = kind.table();
    let filter = "WHERE backend_pk = $1 AND ($2::text IS NULL OR strpos(key, lower($2)) > 0)";
    let total: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table} {filter}"))
        .bind(backend_pk)
        .bind(search)
        .fetch_one(pool)
        .await?;
    let rows: Vec<(String, String)> =
        sqlx::query_as(&format!("SELECT key, {} FROM {table} {filter} ORDER BY key LIMIT $3", ts("created_at")))
            .bind(backend_pk)
            .bind(search)
            .bind(limit)
            .fetch_all(pool)
            .await?;
    Ok((total, rows.into_iter().map(|(key, created_at)| json!({"key": key, "created_at": created_at})).collect()))
}

/// Keys claimed by more than one active backend. Returns `(total, rows)`; each row is
/// `{"key": "...", "backends": ["<prefix_key>", ...]}`.
pub async fn conflicts(pool: &PgPool, kind: Kind, limit: i64) -> Result<(i64, Vec<Value>), sqlx::Error> {
    let base = format!(
        "SELECT c.key, array_agg(b.prefix_key ORDER BY b.prefix_key) AS backends
           FROM {} c JOIN backend b ON b.id = c.backend_pk
          WHERE b.is_active GROUP BY c.key HAVING count(*) > 1",
        kind.table()
    );
    let total: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM ({base}) c")).fetch_one(pool).await?;
    let rows: Vec<(String, Vec<String>)> =
        sqlx::query_as(&format!("{base} ORDER BY c.key LIMIT $1")).bind(limit).fetch_all(pool).await?;
    Ok((total, rows.into_iter().map(|(key, backends)| json!({"key": key, "backends": backends})).collect()))
}

/// Every stored stream cursor with its backend.
pub async fn cursors(pool: &PgPool) -> Result<Vec<Value>, sqlx::Error> {
    let rows: Vec<(i64, String, String, String, i64, String)> = sqlx::query_as(&format!(
        "SELECT b.id, b.prefix_key, c.entity, c.epoch, c.version, {}
           FROM sync_cursor c JOIN backend b ON b.id = c.backend_pk ORDER BY b.id, c.entity",
        ts("c.updated_at")
    ))
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, key, entity, epoch, version, updated_at)| {
            json!({"backend": id, "prefix_key": key, "entity": entity, "epoch": epoch, "version": version, "updated_at": updated_at})
        })
        .collect())
}

#[cfg(test)]
mod tests {
    //! Database tests run only when TEST_DATABASE_URL points at a scratch PostgreSQL database.
    use super::*;
    use crate::db::{
        backends::{create_backend, update_backend, BackendPatch, NewBackend},
        test_pool,
    };

    const KEY_A: &str = "entra:0ae51e19-07c8-4e4b-bb6d-648ee58410f4:7709c6de-b8bf-4ffa-af27-f641536c746d";
    const KEY_B: &str = "entra:0ae51e19-07c8-4e4b-bb6d-648ee58410f4:11111111-2222-4333-8444-555555555555";
    const AGENT: &str = "bbbbbbbb-1111-4000-8000-000000000001";

    fn c(epoch: &str, version: i64) -> Cursor {
        Cursor { epoch: epoch.into(), version }
    }

    fn new(name: &str, id: &str) -> NewBackend {
        NewBackend { name: name.into(), backend_id: id.into(), upstream_url: String::new(), is_active: true }
    }

    #[tokio::test]
    async fn snapshot_delta_lookup_and_conflicts() {
        let Some(pool) = test_pool().await else { return };
        let a = create_backend(&pool, &new("Backend A", "dept_a"), Some("backend001")).await.unwrap();
        let b = create_backend(&pool, &new("Backend B", "dept_b"), Some("backend002")).await.unwrap();
        update_backend(&pool, b, &BackendPatch { is_active: Some(false), ..Default::default() }).await.unwrap();

        let users = vec![KEY_A.to_string(), KEY_B.to_string()];
        let n = replace_all(&pool, a, Kind::Users, &users, &c("e1", 2)).await.unwrap().unwrap();
        assert_eq!(n, ApplyCounts { received: 2, added: 2, removed: 0 });
        replace_all(&pool, b, Kind::Users, &users[..1], &c("e9", 1)).await.unwrap().unwrap();
        assert_eq!(get_cursor(&pool, a, Kind::Users).await.unwrap(), Some(c("e1", 2)));

        // The inactive backend is neither routed nor a conflict.
        let expected = Route { backend_id: "dept_a".into(), name: "Backend A".into(), prefix: "/t/backend001".into() };
        assert_eq!(routes_for(&pool, Kind::Users, KEY_A).await.unwrap(), vec![expected]);
        assert_eq!(conflicts(&pool, Kind::Users, 10).await.unwrap().0, 0);
        update_backend(&pool, b, &BackendPatch { is_active: Some(true), ..Default::default() }).await.unwrap();
        let (total, rows) = conflicts(&pool, Kind::Users, 10).await.unwrap();
        assert_eq!((total, rows[0]["key"].as_str()), (1, Some(KEY_A)));
        assert_eq!(routes_for(&pool, Kind::Users, KEY_A).await.unwrap().len(), 2);

        // A delta from a stale cursor is refused and changes nothing.
        assert!(apply_delta(&pool, a, Kind::Users, &c("e1", 1), &[], &[KEY_B.into()], &c("e1", 3))
            .await
            .unwrap()
            .is_none());
        let n = apply_delta(&pool, a, Kind::Users, &c("e1", 2), &[KEY_A.into()], &[KEY_B.into()], &c("e1", 3))
            .await
            .unwrap()
            .unwrap();
        assert_eq!((n.added, n.removed), (0, 1));
        assert!(routes_for(&pool, Kind::Users, KEY_B).await.unwrap().is_empty());
        assert_eq!(list(&pool, Kind::Users, a, Some("7709C6DE"), 10).await.unwrap().0, 1);

        // An older snapshot of the same epoch does not overwrite a newer cursor; a new epoch always wins.
        assert!(replace_all(&pool, a, Kind::Users, &users, &c("e1", 2)).await.unwrap().is_none());
        replace_all(&pool, a, Kind::Users, &[], &c("e2", 0)).await.unwrap().unwrap();
        assert!(keys(&pool, a, Kind::Users).await.unwrap().is_empty());

        replace_all(&pool, a, Kind::Agents, &[AGENT.into()], &c("e3", 4)).await.unwrap().unwrap();
        let key = canonical_uuid(" BBBBBBBB-1111-4000-8000-000000000001 ").unwrap();
        assert_eq!(routes_for(&pool, Kind::Agents, &key).await.unwrap().len(), 1);
        assert_eq!(cursors(&pool).await.unwrap().len(), 3); // users of a and b, agents of a
    }
}
