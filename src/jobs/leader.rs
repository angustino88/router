//! Leader election (routing-spec CNS-9: only one Router instance pulls at a time).
//!
//! The leader holds a PostgreSQL session advisory lock on a dedicated connection. Only the leader runs the
//! long-poll stream sync and the scheduler; every replica serves the routing API and the admin page from the
//! shared database. If the leader dies its connection closes, the lock is released, and another replica takes
//! over within [`RETRY`].

use std::{
    future::Future,
    sync::atomic::{AtomicBool, Ordering},
    sync::Arc,
    time::Duration,
};

use sqlx::{Connection, PgConnection, PgPool};
use tokio_util::sync::CancellationToken;

/// Arbitrary, fixed advisory-lock key of the Router's sync leadership.
const LOCK_KEY: i64 = 0x4c4d_5f52_4f55_5445; // "LM_ROUTE"
const RETRY: Duration = Duration::from_secs(10);
const PING: Duration = Duration::from_secs(5);

pub struct Leadership {
    enabled: bool,
    leader: AtomicBool,
}

impl Leadership {
    pub fn new(enabled: bool) -> Leadership {
        Leadership { enabled, leader: AtomicBool::new(false) }
    }

    /// Whether leader election is used (`LEADER_ELECTION`).
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn is_leader(&self) -> bool {
        self.leader.load(Ordering::Relaxed)
    }

    fn set(&self, v: bool) {
        self.leader.store(v, Ordering::Relaxed);
    }
}

async fn try_lock(pool: &PgPool) -> Result<Option<PgConnection>, sqlx::Error> {
    let mut conn = pool.acquire().await?.detach();
    let got: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)").bind(LOCK_KEY).fetch_one(&mut conn).await?;
    if got {
        Ok(Some(conn))
    } else {
        let _ = conn.close().await;
        Ok(None)
    }
}

/// Runs `work` whenever this replica is the leader, until `cancel` fires. Without leader election `work`
/// simply runs once.
pub async fn run_while_leader<F, Fut>(pool: PgPool, leadership: Arc<Leadership>, cancel: CancellationToken, work: F)
where
    F: Fn(CancellationToken) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    if !leadership.enabled() {
        leadership.set(true);
        work(cancel).await;
        leadership.set(false);
        return;
    }
    loop {
        match try_lock(&pool).await {
            Ok(Some(mut conn)) => {
                tracing::info!("sync leadership acquired: this replica runs the stream sync and the scheduler");
                leadership.set(true);
                let token = cancel.child_token();
                let mut handle = tokio::spawn(work(token.clone()));
                let mut finished = false;
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        _ = &mut handle => { finished = true; break; }
                        _ = tokio::time::sleep(PING) => {
                            if let Err(e) = sqlx::query("SELECT 1").execute(&mut conn).await {
                                tracing::warn!("sync leadership lost (lock connection failed): {e}");
                                break;
                            }
                        }
                    }
                }
                token.cancel();
                if !finished {
                    let _ = tokio::time::timeout(Duration::from_secs(15), handle).await;
                }
                leadership.set(false);
                let _ = sqlx::query("SELECT pg_advisory_unlock($1)").bind(LOCK_KEY).execute(&mut conn).await;
                let _ = conn.close().await;
                if cancel.is_cancelled() {
                    return;
                }
                tracing::info!("sync leadership released");
            }
            Ok(None) => tracing::debug!("another replica holds the sync leadership"),
            Err(e) => tracing::warn!("leader election: database error: {e}"),
        }
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(RETRY) => {}
        }
    }
}
