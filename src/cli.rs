//! Maintenance commands (in a container: `docker compose -f docker-compose.<env>.yml exec router labmango-router ...`).

use anyhow::{bail, Result};
use clap::Subcommand;
use sqlx::PgPool;

use crate::{
    db::backends::{self as backend_db, valid_prefix_key, NewBackend},
    jobs::sync::{Syncer, Task},
    settings::Settings,
};

#[derive(Subcommand)]
pub enum BackendCmd {
    /// List all backends.
    List,
    /// Create a backend. Without --prefix-key a random 10-character key is generated.
    Add {
        #[arg(long)]
        name: String,
        #[arg(long)]
        prefix_key: Option<String>,
        /// Backend URL nginx proxies `/t/<prefix_key>/` to, e.g. https://lab-a.example.com:8443
        #[arg(long)]
        url: Option<String>,
        /// The backend's BACKEND_ID, checked against GET /internal/info
        #[arg(long)]
        backend_id: Option<String>,
        #[arg(long)]
        inactive: bool,
    },
    /// Mark a backend active.
    Activate { prefix_key: String },
    /// Mark a backend inactive.
    Deactivate { prefix_key: String },
}

/// Full resync (snapshot + replay) of one stream on every active backend, then prints the result.
pub async fn sync_once(pool: PgPool, s: &Settings, task: Task) -> Result<()> {
    let result = Syncer::new(pool, s)?.sync_all(task, true).await?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

pub async fn backend(pool: &PgPool, cmd: BackendCmd) -> Result<()> {
    match cmd {
        BackendCmd::List => {
            println!("{:<6} {:<14} {:<8} {:<14} {:<36} name", "id", "prefix_key", "active", "backend_id", "url");
            for b in backend_db::all_backends(pool).await? {
                println!(
                    "{:<6} {:<14} {:<8} {:<14} {:<36} {}",
                    b.id, b.prefix_key, b.is_active, b.backend_id, b.upstream_url, b.name
                );
            }
        }
        BackendCmd::Add { name, prefix_key, url, backend_id, inactive } => {
            let field = |flag: &str, r: Result<String, String>| r.map_err(|m| anyhow::anyhow!("invalid {flag}: {m}"));
            if let Some(k) = &prefix_key {
                if !valid_prefix_key(k) {
                    bail!("invalid --prefix-key {k:?} (use letters, digits, '_' or '-')");
                }
            }
            let new = NewBackend {
                name: field("--name", crate::admin::validate_name(&name))?,
                backend_id: field("--backend-id", crate::admin::validate_backend_id(&backend_id.unwrap_or_default()))?,
                upstream_url: field("--url", crate::admin::validate_upstream_url(&url.unwrap_or_default()))?,
                is_active: !inactive,
            };
            let id = backend_db::create_backend(pool, &new, prefix_key.as_deref()).await?;
            println!("created backend id={id} name={:?}", new.name);
            println!("the running service writes it to the nginx map within a minute (write_nginx_map)");
        }
        BackendCmd::Activate { prefix_key } => set_active(pool, &prefix_key, true).await?,
        BackendCmd::Deactivate { prefix_key } => set_active(pool, &prefix_key, false).await?,
    }
    Ok(())
}

async fn set_active(pool: &PgPool, prefix_key: &str, active: bool) -> Result<()> {
    if !backend_db::set_active(pool, prefix_key, active).await? {
        bail!("no backend with prefix_key {prefix_key:?}");
    }
    println!("backend {prefix_key} is now {}", if active { "active" } else { "inactive" });
    Ok(())
}
