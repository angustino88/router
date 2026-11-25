//! PostgreSQL access.
//!
//! * Registry and claims: `tenant_backend`, `tenant_usertenantresolver`, `tenant_agenttenantresolver`. Their layout
//!   is unchanged from the original Django service, so an existing database is reused as is.
//! * Router tables: `rust_periodic_task` (schedules), `rust_task_run` (run history), `rust_sync_cursor` (stream cursors).
//!
//! Submodules by feature: [`routing`] (lookups for the public API), [`backends`] (backend CRUD),
//! [`mappings`] (sync snapshots and mapping views), [`jobs`] (schedules and run history).

pub mod backends;
pub mod jobs;
pub mod mappings;
pub mod routing;

use std::{str::FromStr, time::Duration};

use anyhow::{Context, Result};
use sqlx::{
    postgres::{PgConnectOptions, PgPoolOptions},
    PgPool,
};

const SCHEMA: &str = include_str!("schema.sql");

/// Connects with retries (the database may start after the service). Every session gets a statement timeout,
/// so a stuck query can never hold a connection forever.
pub async fn connect(url: &str, max_connections: u32) -> Result<PgPool> {
    let options = PgConnectOptions::from_str(url)
        .context("parse DATABASE_URL")?
        .application_name("labmango-router")
        .options([("statement_timeout", "30s"), ("idle_in_transaction_session_timeout", "60s")]);
    let mut last = None;
    for attempt in 1..=30 {
        match PgPoolOptions::new()
            .max_connections(max_connections)
            .min_connections(1)
            .acquire_timeout(Duration::from_secs(5))
            .idle_timeout(Duration::from_secs(600))
            .max_lifetime(Duration::from_secs(1800))
            .connect_with(options.clone())
            .await
        {
            Ok(p) => return Ok(p),
            Err(e) => {
                tracing::warn!(attempt, "database not ready: {e}");
                last = Some(e);
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
    Err(last.expect("at least one attempt")).context("connect to DATABASE_URL")
}

pub async fn migrate(pool: &PgPool) -> Result<()> {
    sqlx::raw_sql(SCHEMA).execute(pool).await.context("create tables")?;
    Ok(())
}

/// Which resolver table a routing stream is applied to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `tenant_usertenantresolver (user_id = routing key; display_name unused)`
    Users,
    /// `tenant_agenttenantresolver (agent_id = agent UUID, sku unused)`
    Agents,
}

impl Kind {
    pub(crate) fn sql(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Kind::Users => ("tenant_usertenantresolver", "user_id", "display_name"),
            Kind::Agents => ("tenant_agenttenantresolver", "agent_id", "sku"),
        }
    }

    /// Stream name in the core routing sync API (`/internal/{snapshot,changes}/{entity}`).
    pub fn entity(self) -> &'static str {
        match self {
            Kind::Users => "users",
            Kind::Agents => "agents",
        }
    }
}

/// ISO-8601 UTC text for a timestamptz column.
pub(crate) fn ts(col: &str) -> String {
    format!("to_char({col} AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')")
}

/// Scratch database for tests: only when TEST_DATABASE_URL is set (all tenant data in it is deleted).
#[cfg(test)]
pub(crate) async fn test_pool() -> Option<PgPool> {
    let url = std::env::var("TEST_DATABASE_URL").ok()?;
    let pool = PgPool::connect(&url).await.expect("connect TEST_DATABASE_URL");
    migrate(&pool).await.unwrap();
    sqlx::raw_sql(
        "TRUNCATE rust_sync_cursor, tenant_usertenantresolver, tenant_agenttenantresolver, tenant_backend, rust_task_run RESTART IDENTITY CASCADE",
    )
    .execute(&pool)
    .await
    .unwrap();
    Some(pool)
}
