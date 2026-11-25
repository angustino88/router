//! LabMango Router: routes users and agents to their department backend.
//!
//! Source layout (one folder per feature):
//!
//! | Module | Feature |
//! |---|---|
//! | [`routing`] | public API `POST /routing/resolve/{user,agent}` (+ Microsoft ID-token check) |
//! | [`admin`]   | admin page `/admin/` and admin API `/admin/api/*` |
//! | [`jobs`]    | background jobs: routing sync with the backends, nginx map, scheduler, leader election |
//! | [`nginx`]   | nginx backend map |
//! | [`metrics`] | Prometheus metrics (`GET /metrics`) |
//! | [`openapi`] | OpenAPI document built from `api-contracts` (`openapi` command, `GET /openapi.json`) |
//! | [`db`]      | PostgreSQL schema and queries |
//! | [`http`]    | server assembly, middleware, response envelope, request parsing |
//! | [`app`], [`cli`], [`settings`], [`state`], [`outbound`] | start-up, commands, configuration, shared state, Microsoft HTTP client |

#![allow(clippy::result_large_err)] // API errors carry their status, code, message and details

mod admin;
mod app;
mod cli;
mod db;
mod http;
mod jobs;
mod metrics;
mod nginx;
mod openapi;
mod outbound;
mod routing;
mod settings;
mod state;

use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::{jobs::sync::Task, settings::Settings};

#[derive(Parser)]
#[command(
    name = "labmango-router",
    version,
    about = "LabMango Router: routes users and agents to their department backend"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the HTTP server and the scheduler (default).
    Serve,
    /// Create or upgrade the tables (also done by `serve` unless AUTO_MIGRATE=false).
    Migrate,
    /// Full resync (snapshot + replay) of one stream from every active backend, printed as JSON.
    Resync {
        #[arg(value_enum)]
        stream: Stream,
    },
    /// Manage backends from the command line.
    Backend {
        #[command(subcommand)]
        cmd: cli::BackendCmd,
    },
    /// Print the OpenAPI 3.1 document (JSON) to stdout; needs no database.
    Openapi,
    /// Container health probe: exit 0 when the URL answers 2xx.
    Healthcheck {
        #[arg(long, default_value = "http://127.0.0.1:8000/healthz")]
        url: String,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum Stream {
    Users,
    Agents,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // Read env/.env before any runtime thread exists.
    let settings = match &cli.cmd {
        Some(Cmd::Healthcheck { .. }) | Some(Cmd::Openapi) => None,
        _ => Some(Settings::from_env()?),
    };
    init_tracing();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(run(cli.cmd.unwrap_or(Cmd::Serve), settings))
}

async fn run(cmd: Cmd, settings: Option<Settings>) -> Result<()> {
    if let Cmd::Healthcheck { url } = &cmd {
        let ok = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(3))
            .build()?
            .get(url)
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        std::process::exit(if ok { 0 } else { 1 });
    }
    if matches!(cmd, Cmd::Openapi) {
        println!("{}", serde_json::to_string_pretty(&openapi::document()?)?);
        return Ok(());
    }
    let s = settings.expect("settings are loaded for every command except healthcheck and openapi");
    let pool = db::connect(&s.database_url, s.db_max_connections).await?;

    match cmd {
        Cmd::Serve => app::serve(s, pool).await,
        Cmd::Migrate => {
            db::migrate(&pool).await?;
            println!("tables are up to date");
            Ok(())
        }
        Cmd::Resync { stream: Stream::Users } => cli::sync_once(pool, &s, Task::Users).await,
        Cmd::Resync { stream: Stream::Agents } => cli::sync_once(pool, &s, Task::Agents).await,
        Cmd::Backend { cmd } => cli::backend(&pool, cmd).await,
        Cmd::Healthcheck { .. } | Cmd::Openapi => unreachable!(),
    }
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    if std::env::var("LOG_FORMAT").map(|v| v == "json").unwrap_or(false) {
        tracing_subscriber::fmt().json().with_env_filter(filter).init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
}
