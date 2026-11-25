//! Background jobs.
//!
//! * [`Runner`] executes a job in the background and records each run in `job_run` (status, timing, result).
//! * [`run_scheduler`] checks `job_schedule` every few seconds and starts jobs that are due.
//!   Schedules are edited from the admin page; claiming a due run is atomic in the database, so several
//!   replicas never start the same scheduled run twice.
//! * A job never overlaps itself in one process.
//!
//! Jobs: `check_users_stream` / `check_agents_stream` (safety net of the routing streams: resync on a changed
//! epoch or digest drift; the continuous sync is the long-poll loop in [`stream`]) and `write_nginx_map`
//! (rewrites the nginx map from the registry).

use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::Duration,
};

use serde_json::{json, Value};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

pub mod credentials;
pub mod leader;
pub mod stream;
pub mod sync;

use crate::{
    db::{backends::routed_backends, jobs as jobdb},
    nginx::NginxMaps,
    settings::Settings,
};

use self::sync::{Syncer, Task};

const KEEP_RUNS_PER_JOB: i64 = 500;
const SCHEDULER_TICK: Duration = Duration::from_secs(2);
const NGINX_MAP_DEFAULT_SECS: i32 = 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Job {
    Sync(Task),
    NginxMap,
}

impl Job {
    pub const ALL: [Job; 3] = [Job::Sync(Task::Users), Job::Sync(Task::Agents), Job::NginxMap];

    pub fn name(self) -> &'static str {
        match self {
            Job::Sync(t) => t.name(),
            Job::NginxMap => "write_nginx_map",
        }
    }

    pub fn from_name(name: &str) -> Option<Job> {
        Job::ALL.into_iter().find(|j| j.name() == name)
    }
}

/// Rewrites the nginx map from the registry (the nginx watcher reloads when it changes).
pub async fn write_nginx_map(pool: &PgPool, maps: &NginxMaps) -> Result<Value, String> {
    let _guard = maps.lock.lock().await;
    let rows = routed_backends(pool).await.map_err(|e| format!("database: {e}"))?;
    maps.write(&rows)
}

/// Same as [`write_nginx_map`], and records the run in the history with `trigger` (`admin` after a backend
/// change, `manual` for a forced reload, `startup`).
pub async fn write_nginx_map_recorded(pool: &PgPool, maps: &NginxMaps, trigger: &str) -> Result<Value, String> {
    let run_id = jobdb::insert_run(pool, Job::NginxMap.name(), trigger).await.ok();
    let outcome = write_nginx_map(pool, maps).await;
    if let Some(id) = run_id {
        let saved = match &outcome {
            Ok(v) => jobdb::finish_run(pool, id, "succeeded", Some(v), None).await,
            Err(e) => jobdb::finish_run(pool, id, "failed", None, Some(e.as_str())).await,
        };
        if let Err(e) = saved {
            tracing::warn!(run_id = id, "could not save nginx map run: {e}");
        }
    }
    outcome
}

pub struct Runner {
    pool: PgPool,
    syncer: Arc<Syncer>,
    maps: Arc<NginxMaps>,
    running: Mutex<HashSet<&'static str>>,
}

impl Runner {
    pub fn new(pool: PgPool, syncer: Arc<Syncer>, maps: Arc<NginxMaps>) -> Runner {
        Runner { pool, syncer, maps, running: Mutex::new(HashSet::new()) }
    }

    pub fn is_running(&self, job: Job) -> bool {
        self.running.lock().unwrap().contains(job.name())
    }

    async fn execute(&self, job: Job) -> Result<Value, String> {
        match job {
            Job::Sync(t) => self.syncer.sync_all(t, false).await.map_err(|e| format!("database: {e}")),
            Job::NginxMap => write_nginx_map(&self.pool, &self.maps).await,
        }
    }

    /// Starts `job` in the background. `Ok(None)` when it is already running in this process.
    pub async fn start(self: &Arc<Self>, job: Job, trigger: &'static str) -> Result<Option<i64>, sqlx::Error> {
        if !self.running.lock().unwrap().insert(job.name()) {
            return Ok(None);
        }
        let run_id = match jobdb::insert_run(&self.pool, job.name(), trigger).await {
            Ok(id) => id,
            Err(e) => {
                self.running.lock().unwrap().remove(job.name());
                return Err(e);
            }
        };
        let me = self.clone();
        tokio::spawn(async move {
            tracing::debug!(job = job.name(), trigger, run_id, "job started");
            let outcome = me.execute(job).await;
            let saved = match &outcome {
                Ok(result) => {
                    // The nginx map job runs every minute: keep its success logs quiet.
                    if job == Job::NginxMap && result.get("changed") != Some(&json!(true)) {
                        tracing::debug!(job = job.name(), run_id, "job succeeded (no change)");
                    } else {
                        tracing::info!(job = job.name(), run_id, result = %result, "job succeeded");
                    }
                    jobdb::finish_run(&me.pool, run_id, "succeeded", Some(result), None).await
                }
                Err(e) => {
                    tracing::error!(job = job.name(), run_id, "job failed: {e}");
                    jobdb::finish_run(&me.pool, run_id, "failed", None, Some(e.as_str())).await
                }
            };
            if let Err(e) = saved {
                tracing::error!(run_id, "could not save the job result: {e}");
            }
            if let Err(e) = jobdb::prune_runs(&me.pool, job.name(), KEEP_RUNS_PER_JOB).await {
                tracing::warn!("could not prune job runs: {e}");
            }
            me.running.lock().unwrap().remove(job.name());
        });
        Ok(Some(run_id))
    }
}

/// Creates the schedule rows on first start and closes runs left open by a previous process.
pub async fn init(pool: &PgPool, s: &Settings) -> Result<(), sqlx::Error> {
    let secs = |d: Duration| d.as_secs().clamp(1, i32::MAX as u64) as i32;
    for task in Task::ALL {
        jobdb::seed_schedule(pool, task.name(), secs(s.sync_check_interval)).await?;
    }
    jobdb::seed_schedule(pool, Job::NginxMap.name(), NGINX_MAP_DEFAULT_SECS).await?;
    let n = jobdb::mark_interrupted_runs(pool).await?;
    if n > 0 {
        tracing::warn!(runs = n, "marked unfinished job runs of a previous process as failed");
    }
    Ok(())
}

/// Starts due schedules. Database errors are logged and retried on the next tick. Runs on the leader replica.
pub async fn run_scheduler(pool: PgPool, runner: Arc<Runner>, cancel: CancellationToken) {
    tracing::info!("scheduler started");
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(SCHEDULER_TICK) => {}
        }
        let due = match jobdb::due_schedules(&pool).await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("scheduler: cannot read schedules: {e}");
                continue;
            }
        };
        for name in due {
            let Some(job) = Job::from_name(&name) else { continue };
            if runner.is_running(job) {
                continue; // still busy with the previous run; try again on a later tick
            }
            match jobdb::claim_schedule(&pool, &name).await {
                Ok(true) => {
                    if let Err(e) = runner.start(job, "schedule").await {
                        tracing::error!(job = %name, "scheduler: cannot start job: {e}");
                    }
                }
                Ok(false) => {} // another replica took it, or the schedule changed
                Err(e) => tracing::warn!(job = %name, "scheduler: cannot claim schedule: {e}"),
            }
        }
    }
}
