//! User / agent mappings: applying the core routing streams (full snapshot or one delta page, each
//! together with its stream cursor) and the read-only admin views.

use serde_json::{json, Value};
use sqlx::{PgPool, Postgres, Transaction};

use super::{ts, Kind};

/// Position in one core change stream: the history (`epoch`) and the last applied `version`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub epoch: String,
    pub version: i64,
}

/// Counts of one applied snapshot or delta.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApplyCounts {
    pub received: usize,
    pub created: usize,
    pub updated: usize,
    pub deactivated: u64,
}

pub async fn get_cursor(pool: &PgPool, backend_id: i64, kind: Kind) -> Result<Option<Cursor>, sqlx::Error> {
    let row: Option<(String, i64)> =
        sqlx::query_as("SELECT epoch, version FROM rust_sync_cursor WHERE backend_id = $1 AND entity = $2")
            .bind(backend_id)
            .bind(kind.entity())
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(epoch, version)| Cursor { epoch, version }))
}

/// Locks the cursor row of this stream for the rest of the transaction (the row may not exist yet).
async fn lock_cursor(
    tx: &mut Transaction<'_, Postgres>,
    backend_id: i64,
    kind: Kind,
) -> Result<Option<Cursor>, sqlx::Error> {
    // Serialises writers of one stream even before its cursor row exists.
    sqlx::query("SELECT pg_advisory_xact_lock($1, hashtext($2))")
        .bind(backend_id as i32)
        .bind(kind.entity())
        .execute(&mut **tx)
        .await?;
    let row: Option<(String, i64)> =
        sqlx::query_as("SELECT epoch, version FROM rust_sync_cursor WHERE backend_id = $1 AND entity = $2 FOR UPDATE")
            .bind(backend_id)
            .bind(kind.entity())
            .fetch_optional(&mut **tx)
            .await?;
    Ok(row.map(|(epoch, version)| Cursor { epoch, version }))
}

async fn save_cursor(
    tx: &mut Transaction<'_, Postgres>,
    backend_id: i64,
    kind: Kind,
    c: &Cursor,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO rust_sync_cursor (backend_id, entity, epoch, version, updated_at) VALUES ($1, $2, $3, $4, now())
         ON CONFLICT (backend_id, entity) DO UPDATE SET epoch = EXCLUDED.epoch, version = EXCLUDED.version, updated_at = now()",
    )
    .bind(backend_id)
    .bind(kind.entity())
    .bind(&c.epoch)
    .bind(c.version)
    .execute(&mut **tx)
    .await?;
    if kind == Kind::Agents {
        // Shown on the admin page as before; the real position is in rust_sync_cursor.
        sqlx::query("UPDATE tenant_backend SET computer_sync_version = $2 WHERE id = $1")
            .bind(backend_id)
            .bind(c.version.clamp(0, i32::MAX as i64) as i32)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

/// Inserts or re-activates `(key, extra)` rows. The streams carry only keys, so `extra` is empty and the
/// stored legacy value (display_name / sku) is kept.
async fn upsert_active(
    tx: &mut Transaction<'_, Postgres>,
    backend_id: i64,
    kind: Kind,
    records: &[(String, String)],
) -> Result<usize, sqlx::Error> {
    if records.is_empty() {
        return Ok(0);
    }
    let (table, id_col, extra_col) = kind.sql();
    let ids: Vec<&str> = records.iter().map(|r| r.0.as_str()).collect();
    let extras: Vec<&str> = records.iter().map(|r| r.1.as_str()).collect();
    // `xmax = 0` is true only for freshly inserted rows.
    let inserted: Vec<bool> = sqlx::query_scalar(&format!(
        "INSERT INTO {table} (created, modified, {id_col}, {extra_col}, is_active, backend_id)
         SELECT now(), now(), s.ident, s.extra, true, $3
           FROM UNNEST($1::text[], $2::text[]) AS s(ident, extra)
         ON CONFLICT ({id_col}, backend_id)
         DO UPDATE SET {extra_col} = COALESCE(NULLIF(EXCLUDED.{extra_col}, ''), {table}.{extra_col}),
                       is_active = true, modified = now()
         RETURNING (xmax = 0)"
    ))
    .bind(&ids)
    .bind(&extras)
    .bind(backend_id)
    .fetch_all(&mut **tx)
    .await?;
    Ok(inserted.iter().filter(|c| **c).count())
}

/// Publishes a full snapshot: every key in `records` active, every other row of the backend inactive, and the
/// cursor set to `cursor`, in one transaction. Returns `None` (nothing written) when the stored cursor is
/// already further along the same epoch, i.e. a concurrent delta is newer than this snapshot.
pub async fn replace_all(
    pool: &PgPool,
    backend_id: i64,
    kind: Kind,
    records: &[(String, String)],
    cursor: &Cursor,
) -> Result<Option<ApplyCounts>, sqlx::Error> {
    let (table, id_col, _) = kind.sql();
    let mut tx = pool.begin().await?;
    if let Some(current) = lock_cursor(&mut tx, backend_id, kind).await? {
        if current.epoch == cursor.epoch && current.version > cursor.version {
            tx.rollback().await?;
            return Ok(None);
        }
    }
    let created = upsert_active(&mut tx, backend_id, kind, records).await?;
    let ids: Vec<&str> = records.iter().map(|r| r.0.as_str()).collect();
    let deactivated = sqlx::query(&format!(
        "UPDATE {table} SET is_active = false, modified = now()
          WHERE backend_id = $1 AND is_active AND NOT ({id_col} = ANY($2::text[]))"
    ))
    .bind(backend_id)
    .bind(&ids)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    save_cursor(&mut tx, backend_id, kind, cursor).await?;
    tx.commit().await?;
    Ok(Some(ApplyCounts { received: records.len(), created, updated: records.len() - created, deactivated }))
}

/// Applies one page of changes and moves the cursor from `from` to `to`, in one transaction.
/// Returns `None` (nothing written) when the stored cursor is no longer `from`: another writer (a full
/// resync) moved it, and the caller must re-read the cursor.
pub async fn apply_delta(
    pool: &PgPool,
    backend_id: i64,
    kind: Kind,
    from: &Cursor,
    upserts: &[(String, String)],
    deletes: &[String],
    to: &Cursor,
) -> Result<Option<ApplyCounts>, sqlx::Error> {
    let (table, id_col, _) = kind.sql();
    let mut tx = pool.begin().await?;
    if lock_cursor(&mut tx, backend_id, kind).await?.as_ref() != Some(from) {
        tx.rollback().await?;
        return Ok(None);
    }
    let created = upsert_active(&mut tx, backend_id, kind, upserts).await?;
    let deactivated = if deletes.is_empty() {
        0
    } else {
        sqlx::query(&format!(
            "UPDATE {table} SET is_active = false, modified = now()
              WHERE backend_id = $1 AND is_active AND {id_col} = ANY($2::text[])"
        ))
        .bind(backend_id)
        .bind(deletes)
        .execute(&mut *tx)
        .await?
        .rows_affected()
    };
    save_cursor(&mut tx, backend_id, kind, to).await?;
    tx.commit().await?;
    Ok(Some(ApplyCounts {
        received: upserts.len() + deletes.len(),
        created,
        updated: upserts.len() - created,
        deactivated,
    }))
}

/// Active rows of one backend.
#[cfg(test)]
pub async fn active_count(pool: &PgPool, backend_id: i64, kind: Kind) -> Result<i64, sqlx::Error> {
    let (table, _, _) = kind.sql();
    sqlx::query_scalar(&format!("SELECT count(*) FROM {table} WHERE backend_id = $1 AND is_active"))
        .bind(backend_id)
        .fetch_one(pool)
        .await
}

// ------------------------------------------------------------------ mappings (read-only)
/// Mappings of one backend, filtered. Returns `(total matching, rows)`; rows are JSON objects with the
/// column names (`user_id`/`display_name` or `agent_id`/`sku`).
pub async fn list_mappings(
    pool: &PgPool,
    kind: Kind,
    backend_id: i64,
    search: Option<&str>,
    active: Option<bool>,
    limit: i64,
) -> Result<(i64, Vec<Value>), sqlx::Error> {
    let (table, id_col, extra_col) = kind.sql();
    let filter = format!(
        "WHERE backend_id = $1
           AND ($2::text IS NULL OR strpos(lower({id_col}), lower($2)) > 0 OR strpos(lower({extra_col}), lower($2)) > 0)
           AND ($3::boolean IS NULL OR is_active = $3)"
    );
    let total: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table} {filter}"))
        .bind(backend_id)
        .bind(search)
        .bind(active)
        .fetch_one(pool)
        .await?;
    let rows: Vec<(i64, String, String, bool, String, String)> = sqlx::query_as(&format!(
        "SELECT id, {id_col}, {extra_col}, is_active, {}, {} FROM {table} {filter} ORDER BY {id_col} LIMIT $4",
        ts("created"),
        ts("modified")
    ))
    .bind(backend_id)
    .bind(search)
    .bind(active)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    let items = rows
        .into_iter()
        .map(|(id, key, extra, is_active, created, modified)| {
            let mut m = serde_json::Map::new();
            m.insert("id".into(), json!(id));
            m.insert(id_col.into(), json!(key));
            m.insert(extra_col.into(), json!(extra));
            m.insert("is_active".into(), json!(is_active));
            m.insert("created".into(), json!(created));
            m.insert("modified".into(), json!(modified));
            Value::Object(m)
        })
        .collect();
    Ok((total, items))
}

// ------------------------------------------------------------------ conflicts and stream status

/// Keys claimed by more than one active backend (routing-spec CNS-6). Returns `(total, rows)`; each row is
/// `{"key": "...", "backends": ["<prefix_key>", ...]}`.
pub async fn conflicts(pool: &PgPool, kind: Kind, limit: i64) -> Result<(i64, Vec<Value>), sqlx::Error> {
    let (table, id_col, _) = kind.sql();
    let base = format!(
        "SELECT r.{id_col} AS key, array_agg(DISTINCT b.prefix_key ORDER BY b.prefix_key) AS backends
           FROM {table} r JOIN tenant_backend b ON b.id = r.backend_id
          WHERE r.is_active AND b.is_active
          GROUP BY r.{id_col} HAVING count(DISTINCT b.id) > 1"
    );
    let total: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM ({base}) c")).fetch_one(pool).await?;
    let rows: Vec<(String, Vec<String>)> =
        sqlx::query_as(&format!("{base} ORDER BY key LIMIT $1")).bind(limit).fetch_all(pool).await?;
    Ok((total, rows.into_iter().map(|(key, backends)| json!({"key": key, "backends": backends})).collect()))
}

/// Every stored stream cursor with its backend, for the admin "Sync" view.
pub async fn stream_cursors(pool: &PgPool) -> Result<Vec<Value>, sqlx::Error> {
    let rows: Vec<(i64, String, String, String, i64, String)> = sqlx::query_as(&format!(
        "SELECT b.id, b.prefix_key, c.entity, c.epoch, c.version, {}
           FROM rust_sync_cursor c JOIN tenant_backend b ON b.id = c.backend_id
          ORDER BY b.id, c.entity",
        ts("c.updated_at")
    ))
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, key, entity, epoch, version, updated)| {
            json!({"backend": id, "prefix_key": key, "entity": entity, "epoch": epoch, "version": version, "updated_at": updated})
        })
        .collect())
}

/// Active keys of one backend, for the digest checksum (routing-spec DIG-1).
pub async fn active_keys(pool: &PgPool, backend_id: i64, kind: Kind) -> Result<Vec<String>, sqlx::Error> {
    let (table, id_col, _) = kind.sql();
    sqlx::query_scalar(&format!("SELECT {id_col} FROM {table} WHERE backend_id = $1 AND is_active"))
        .bind(backend_id)
        .fetch_all(pool)
        .await
}

#[cfg(test)]
mod tests {
    //! Database tests run only when TEST_DATABASE_URL points at a scratch PostgreSQL database.
    use super::*;
    use crate::db::{
        backends::insert_backend,
        routing::{active_backends_by_agent_id, active_backends_by_user_key, Mapping},
        test_pool,
    };

    const KEY_A: &str = "entra:0ae51e19-07c8-4e4b-bb6d-648ee58410f4:7709c6de-b8bf-4ffa-af27-f641536c746d";
    const KEY_B: &str = "entra:0ae51e19-07c8-4e4b-bb6d-648ee58410f4:11111111-2222-4333-8444-555555555555";
    const AGENT: &str = "bbbbbbbb-1111-4000-8000-000000000001";

    fn c(epoch: &str, version: i64) -> Cursor {
        Cursor { epoch: epoch.into(), version }
    }

    #[tokio::test]
    async fn snapshot_delta_and_lookup() {
        let Some(pool) = test_pool().await else { return };
        let a = insert_backend(&pool, "backend001", "Backend A", true).await.unwrap();
        let b = insert_backend(&pool, "backend002", "Backend B", false).await.unwrap();

        let users = vec![(KEY_A.to_string(), String::new()), (KEY_B.to_string(), String::new())];
        let n = replace_all(&pool, a.id, Kind::Users, &users, &c("e1", 2)).await.unwrap().unwrap();
        assert_eq!(n, ApplyCounts { received: 2, created: 2, updated: 0, deactivated: 0 });
        replace_all(&pool, b.id, Kind::Users, &users[..1], &c("e9", 1)).await.unwrap().unwrap();
        assert_eq!(get_cursor(&pool, a.id, Kind::Users).await.unwrap(), Some(c("e1", 2)));

        // Inactive backend is excluded.
        assert_eq!(
            active_backends_by_user_key(&pool, KEY_A).await.unwrap(),
            vec![Mapping { backend_name: "Backend A".into(), prefix: "/t/backend001".into() }]
        );

        // A delta from a stale cursor is refused and changes nothing.
        assert!(apply_delta(&pool, a.id, Kind::Users, &c("e1", 1), &[], &[KEY_B.into()], &c("e1", 3))
            .await
            .unwrap()
            .is_none());
        // Delete B, cursor moves to 3.
        let n = apply_delta(
            &pool,
            a.id,
            Kind::Users,
            &c("e1", 2),
            &[(KEY_A.into(), String::new())],
            &[KEY_B.into()],
            &c("e1", 3),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(n.deactivated, 1);
        assert!(active_backends_by_user_key(&pool, KEY_B).await.unwrap().is_empty());
        assert_eq!(active_backends_by_user_key(&pool, KEY_A).await.unwrap().len(), 1);

        // An older snapshot of the same epoch does not overwrite a newer cursor.
        assert!(replace_all(&pool, a.id, Kind::Users, &users, &c("e1", 2)).await.unwrap().is_none());
        // A snapshot of a new epoch always wins.
        replace_all(&pool, a.id, Kind::Users, &users[..1], &c("e2", 0)).await.unwrap().unwrap();
        assert_eq!(get_cursor(&pool, a.id, Kind::Users).await.unwrap(), Some(c("e2", 0)));

        // Agents: key only; version mirrored into computer_sync_version.
        replace_all(&pool, a.id, Kind::Agents, &[(AGENT.into(), String::new())], &c("e3", 4)).await.unwrap().unwrap();
        assert_eq!(
            active_backends_by_agent_id(&pool, " BBBBBBBB-1111-4000-8000-000000000001 ").await.unwrap().len(),
            1
        );
        assert_eq!(active_count(&pool, a.id, Kind::Agents).await.unwrap(), 1);
        let v: i32 = sqlx::query_scalar("SELECT computer_sync_version FROM tenant_backend WHERE id = $1")
            .bind(a.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(v, 4);
    }
}
