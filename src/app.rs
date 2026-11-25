//! `serve`: wires everything together and runs the HTTP server, the scheduler and the stream sync until SIGTERM.

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

use crate::{
    admin::AdminAccount,
    db, http,
    jobs::{
        self,
        leader::{run_while_leader, Leadership},
        sync::Syncer,
        Runner,
    },
    nginx::NginxMaps,
    outbound,
    routing::aad::Aad,
    settings::Settings,
    state::AppState,
};

pub async fn serve(s: Settings, pool: PgPool) -> Result<()> {
    if s.auto_migrate {
        db::migrate(&pool).await?;
    }
    jobs::init(&pool, &s).await?;

    let maps = Arc::new(NginxMaps::new(&s.nginx_map_dir));
    // Write the map at start-up so nginx has a current file before the first scheduled run.
    match jobs::write_nginx_map_recorded(&pool, &maps, "startup").await {
        Ok(v) => tracing::info!(result = %v, "nginx map generated"),
        Err(e) => tracing::warn!("nginx map not generated at start-up: {e}"),
    }

    let aad = Aad::new(outbound::client(&s)?, s.aad_jwks_url.clone(), &s.aad_audience, s.aad_jwks_cache_ttl);
    let syncer = Arc::new(Syncer::new(pool.clone(), &s)?);
    let runner = Arc::new(Runner::new(pool.clone(), syncer.clone(), maps.clone()));
    let admin = match (&s.admin_username, &s.admin_password) {
        (Some(u), Some(p)) => Some(AdminAccount { username: u.clone(), password: p.clone() }),
        _ => {
            tracing::warn!("ADMIN_USERNAME / ADMIN_PASSWORD not set: admin page and admin API are disabled");
            None
        }
    };
    let leadership = Arc::new(Leadership::new(s.leader_election));
    let state = Arc::new(AppState {
        pool: pool.clone(),
        aad,
        admin,
        runner: runner.clone(),
        maps,
        sync_enabled: s.sync_enabled,
        leadership: leadership.clone(),
    });

    let cancel = CancellationToken::new();
    let mut handles = Vec::new();
    if s.sync_enabled {
        // Scheduler + continuous routing sync (one long-poll loop per active backend and stream), on the leader only.
        let (p, r, sy) = (pool.clone(), runner, syncer);
        let work = move |token: CancellationToken| {
            let (p, r, sy) = (p.clone(), r.clone(), sy.clone());
            async move {
                let scheduler = tokio::spawn(jobs::run_scheduler(p.clone(), r, token.clone()));
                let streams = tokio::spawn(jobs::stream::run_streams(p, sy, token));
                let _ = tokio::join!(scheduler, streams);
            }
        };
        handles.push(tokio::spawn(run_while_leader(pool.clone(), leadership, cancel.clone(), work)));
    } else {
        tracing::info!(
            "scheduler and stream sync disabled (SYNC_ENABLED=false); jobs can still be run from the admin page"
        );
    }

    let listener =
        tokio::net::TcpListener::bind(&s.listen_addr).await.with_context(|| format!("bind {}", s.listen_addr))?;
    tracing::info!(addr = %s.listen_addr, "router listening");

    let shutdown = cancel.clone();
    axum::serve(listener, http::router(state))
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            shutdown.cancel();
        })
        .await?;

    cancel.cancel();
    for h in handles {
        let _ = tokio::time::timeout(Duration::from_secs(20), h).await;
    }
    pool.close().await;
    tracing::info!("stopped");
    Ok(())
}

async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("shutdown requested");
}
