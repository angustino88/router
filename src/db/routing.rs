//! Lookups behind the public routing API.

use std::collections::HashSet;

use serde::Serialize;
use sqlx::PgPool;
use utoipa::ToSchema;

/// One item of the `mappings` list.
#[derive(Debug, Clone, Serialize, PartialEq, Eq, ToSchema)]
pub struct Mapping {
    /// Backend display name.
    pub backend_name: String,
    /// Path prefix of the backend: `/t/<prefix_key>`.
    pub prefix: String,
}

fn mapping(name: String, prefix_key: &str) -> Mapping {
    Mapping { backend_name: name, prefix: format!("/t/{prefix_key}") }
}

/// Active backends that claim this user routing key (`entra:<tenant UUID>:<object UUID>`, lowercase,
/// as built by [`crate::routing::aad::extract_user_key`]).
pub async fn active_backends_by_user_key(pool: &PgPool, key: &str) -> Result<Vec<Mapping>, sqlx::Error> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT b.name, b.prefix_key
           FROM tenant_usertenantresolver r
           JOIN tenant_backend b ON b.id = r.backend_id
          WHERE r.user_id = $1 AND r.is_active AND b.is_active
          ORDER BY r.id",
    )
    .bind(key)
    .fetch_all(pool)
    .await?;

    let mut seen = HashSet::new();
    Ok(rows.into_iter().filter(|(_, key)| seen.insert(key.clone())).map(|(name, key)| mapping(name, &key)).collect())
}

/// Active backends that claim this agent. The agent id is the agent UUID; it is compared in canonical form
/// (lowercase, hyphenated), which is how the core streams it.
pub async fn active_backends_by_agent_id(pool: &PgPool, agent_id: &str) -> Result<Vec<Mapping>, sqlx::Error> {
    let Some(agent_id) = canonical_uuid(agent_id) else {
        return Ok(Vec::new());
    };
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT b.name, b.prefix_key
           FROM tenant_agenttenantresolver r
           JOIN tenant_backend b ON b.id = r.backend_id
          WHERE r.agent_id = $1 AND r.is_active AND b.is_active
          ORDER BY r.id",
    )
    .bind(agent_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(name, key)| mapping(name, &key)).collect())
}

/// `Some("xxxxxxxx-xxxx-...")` (lowercase) for a UUID in any accepted spelling, else `None`.
pub fn canonical_uuid(raw: &str) -> Option<String> {
    uuid::Uuid::parse_str(raw.trim()).ok().map(|u| u.hyphenated().to_string())
}
