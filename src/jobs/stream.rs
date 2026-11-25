//! Long-poll workers: one loop per (active backend, stream), replacing the periodic snapshot download.
//!
//! [`run_streams`] looks at the active backends every [`BACKEND_REFRESH`] and starts or stops workers so
//! that each active backend has a `users` and an `agents` worker. Each worker:
//!
//! * without a stored cursor: full resync (snapshot + replay), then
//! * loops on `GET /internal/changes/{entity}?since=<version>&epoch=<epoch>&wait=30` and applies every page
//!   together with its cursor (pages with `has_more` are fetched right away);
//! * on `mode: "reset"`: full resync;
//! * on errors: retries with exponential backoff and jitter, keeping the last saved cursor;
//! * while the backend does not advertise the stream (spec PUB-9): checks again every
//!   [`NOT_ADVERTISED_RETRY`] without logging it as an error.
//!
//! The supervisor runs only on the leader replica (see [`super::leader`]).

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use sqlx::PgPool;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::sync::{ErrorKind, PollOutcome, SyncError, Syncer, Task};
use crate::db::{
    backends::{self as backend_db, Backend},
    mappings,
};

const BACKEND_REFRESH: Duration = Duration::from_secs(15);
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
const NOT_ADVERTISED_RETRY: Duration = Duration::from_secs(60);

type WorkerKey = (i64, Task);

struct Worker {
    cancel: CancellationToken,
    handle: JoinHandle<()>,
    prefix_key: String,
    backend_id: String,
}

/// Supervises the long-poll workers until `cancel` fires.
pub async fn run_streams(pool: PgPool, syncer: Arc<Syncer>, cancel: CancellationToken) {
    tracing::info!("routing stream sync started (long-poll)");
    let mut workers: HashMap<WorkerKey, Worker> = HashMap::new();
    loop {
        match backend_db::active_backends(&pool).await {
            Ok(backends) => {
                let wanted: HashSet<WorkerKey> = backends.iter().flat_map(|b| Task::ALL.map(|t| (b.id, t))).collect();
                // Stop workers of backends that were deactivated, deleted or renamed, and restart finished ones.
                workers.retain(|key, w| {
                    let still_valid = wanted.contains(key)
                        && backends
                            .iter()
                            .any(|b| b.id == key.0 && b.prefix_key == w.prefix_key && b.backend_id == w.backend_id)
                        && !w.handle.is_finished();
                    if !still_valid {
                        w.cancel.cancel();
                    }
                    still_valid
                });
                for backend in &backends {
                    for task in Task::ALL {
                        workers.entry((backend.id, task)).or_insert_with(|| {
                            let token = cancel.child_token();
                            let handle = tokio::spawn(worker(syncer.clone(), backend.clone(), task, token.clone()));
                            Worker {
                                cancel: token,
                                handle,
                                prefix_key: backend.prefix_key.clone(),
                                backend_id: backend.backend_id.clone(),
                            }
                        });
                    }
                }
            }
            Err(e) => tracing::warn!("stream sync: cannot read backends: {e}"),
        }
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(BACKEND_REFRESH) => {}
        }
    }
    for (_, w) in workers.drain() {
        w.cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), w.handle).await;
    }
    tracing::info!("routing stream sync stopped");
}

async fn worker(syncer: Arc<Syncer>, backend: Backend, task: Task, cancel: CancellationToken) {
    let entity = task.entity();
    tracing::info!(backend = %backend.prefix_key, entity, "stream worker started");
    let mut backoff = BACKOFF_MIN;
    let mut waiting_for_stream = false;
    loop {
        let outcome = tokio::select! {
            _ = cancel.cancelled() => break,
            r = step(&syncer, &backend, task) => r,
        };
        match outcome {
            Ok(()) => {
                backoff = BACKOFF_MIN;
                if waiting_for_stream {
                    tracing::info!(backend = %backend.prefix_key, entity, "stream is now advertised");
                    waiting_for_stream = false;
                }
            }
            Err(e) if e.kind == ErrorKind::NotAdvertised => {
                if !waiting_for_stream {
                    tracing::info!(backend = %backend.prefix_key, entity, "stream not advertised by the backend yet; checking every minute");
                    waiting_for_stream = true;
                }
                backoff = BACKOFF_MIN;
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(NOT_ADVERTISED_RETRY) => {}
                }
            }
            Err(e) => {
                crate::metrics::inc("router_sync_errors_total", &[("entity", entity)]);
                tracing::warn!(backend = %backend.prefix_key, entity, retry_in_ms = backoff.as_millis() as u64, "stream sync error: {e}");
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(jitter(backoff)) => {}
                }
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        }
    }
    tracing::info!(backend = %backend.prefix_key, entity, "stream worker stopped");
}

/// One round: resync without a cursor, else one long-poll (and a resync when the backend asks for it).
async fn step(syncer: &Syncer, backend: &Backend, task: Task) -> Result<(), SyncError> {
    let Some(cursor) = mappings::get_cursor(&syncer.pool, backend.id, task.kind()).await? else {
        syncer.resync(backend, task).await?;
        return Ok(());
    };
    match syncer.poll_once(backend, task, &cursor).await? {
        PollOutcome::Idle | PollOutcome::Stale => {}
        PollOutcome::Applied { counts, to, has_more } => {
            crate::metrics::inc("router_sync_pages_applied_total", &[("entity", task.entity())]);
            tracing::info!(
                backend = %backend.prefix_key, entity = task.entity(), version = to.version, has_more,
                created = counts.created, updated = counts.updated, deactivated = counts.deactivated,
                "stream changes applied"
            );
        }
        PollOutcome::Reset(reason) => {
            tracing::info!(backend = %backend.prefix_key, entity = task.entity(), reason, "stream reset, resyncing");
            syncer.resync(backend, task).await?;
        }
    }
    Ok(())
}

/// Random delay between 50 % and 100 % of `d`.
fn jitter(d: Duration) -> Duration {
    let r = (uuid::Uuid::new_v4().as_u128() % 1000) as f64 / 2000.0;
    d.mul_f64(0.5 + r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_bounds() {
        for _ in 0..100 {
            let j = jitter(Duration::from_secs(10));
            assert!(j >= Duration::from_secs(5) && j <= Duration::from_secs(10));
        }
    }
}
