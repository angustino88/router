//! State shared by the HTTP handlers.

use std::sync::Arc;

use sqlx::PgPool;

use crate::{
    admin::AdminAccount,
    jobs::{leader::Leadership, Runner},
    nginx::NginxMaps,
    routing::aad::Aad,
    settings::ConflictPolicy,
};

pub struct AppState {
    pub pool: PgPool,
    pub aad: Aad,
    /// `None` when ADMIN_USERNAME / ADMIN_PASSWORD are not set: the admin page and API answer 503.
    pub admin: Option<AdminAccount>,
    pub runner: Arc<Runner>,
    pub maps: Arc<NginxMaps>,
    /// SYNC_ENABLED: the scheduler and the long-poll sync run (on the leader replica).
    pub scheduler_enabled: bool,
    pub conflict_policy: ConflictPolicy,
    /// Whether this replica currently holds the sync leadership.
    pub leadership: Arc<Leadership>,
}
