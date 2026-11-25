//! Routing sync with the core backends (the private `/internal/*` API of each department backend).
//!
//! Two streams per backend, `users` and `agents`. Each claim is only a key:
//!
//! * users: `entra:<tenant UUID>:<object UUID>`;
//! * agents: the agent UUID.
//!
//! Only the keys are stored.
//!
//! Protocol (see the core's routing-sync spec):
//!
//! 1. [`Syncer::resync`]: download `/internal/snapshot/{entity}` page by page, replay
//!    `/internal/changes/{entity}` from the first page's watermark, then publish the whole list and its
//!    cursor `(epoch, version)` in one transaction.
//! 2. [`Syncer::poll_once`]: long-poll `/internal/changes/{entity}?since=&epoch=&wait=30` and apply each
//!    page together with the new cursor. `mode: "reset"` means a full resync.
//!
//! The long-poll loops run in [`super::stream`]. The scheduled jobs (`sync_all_backend_*`) are a safety net:
//! [`Syncer::sync_all`] checks each stream against `/internal/info` and `/internal/digest` and resyncs on drift.
//!
//! Requests go through the internal Nginx entrypoint: `GET {NGINX_INTERNAL_BASE_URL}/t/{prefix_key}/internal/...`
//! with `Host: {NGINX_INTERNAL_HOST_HEADER}`, `Authorization: Bearer <credential>` (see [`super::credentials`])
//! and a fresh `X-Request-ID`, bypassing any HTTP(S)_PROXY settings and never following redirects (GET-6).
//!
//! Consumer rules of the backend spec implemented here:
//!
//! * CNS-2: `GET /internal/info` is read before every resync and check; its `backend_id` must equal the one
//!   registered for the backend (when set), and a stream that is not advertised is not synced (it is retried
//!   later without counting as a failure).
//! * CNS-5: every response is validated (epoch, version, key format) before anything is written.
//! * CNS-8 / DIG-1..5: the digest `count` **and** `checksum` are compared with the local claims; a missing
//!   digest endpoint (404 `not_found`) means "not supported", not a failure.

use std::{collections::HashSet, sync::Arc, time::Duration};

use anyhow::Result;
use reqwest::{
    header::{ACCEPT, AUTHORIZATION, HOST},
    redirect, StatusCode,
};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

use super::credentials::SyncCredentials;

use crate::{
    db::{
        backends::{self as backend_db, Backend},
        mappings::{self, conflicts, ApplyCounts, Cursor},
        routing::canonical_uuid,
        Kind,
    },
    outbound,
    settings::Settings,
};

pub const INFO: &str = "internal/info";
/// `data_hash` of the only claim payload the streams carry, `{"data":{},"state":"active"}` (spec 5.3).
pub const ACTIVE_DATA_HASH: &str = "43f154ed4a9512251f4b2bf1e0588c167a2ac83c224de4f4bcc28adf2d720fd5";
const SNAPSHOT_LIMIT: u32 = 1000;
const CHANGES_LIMIT: u32 = 500;
/// Upper bound of change pages replayed by one resync; a stream changing faster than that is retried later.
const MAX_REPLAY_PAGES: usize = 1000;

/// Why a sync request failed, when the caller reacts differently from a plain error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Other,
    /// The stream is not advertised by `/internal/info` (or the backend answered `404 entity_not_supported`):
    /// the backend is not ready to publish it yet (spec PUB-9). Retried later, not an error.
    NotAdvertised,
    /// `404 not_found`, e.g. the optional digest endpoint is not implemented (DIG-5).
    NotFound,
    /// `/internal/info` reported another `backend_id` than the registry (CNS-2): nothing is applied.
    WrongBackend,
}

/// Error reported as `"error"` in the task result and in the logs.
#[derive(Debug)]
pub struct SyncError {
    pub name: &'static str,
    pub message: String,
    pub kind: ErrorKind,
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.name, self.message)
    }
}

impl From<sqlx::Error> for SyncError {
    fn from(e: sqlx::Error) -> Self {
        SyncError { name: "DatabaseError", message: e.to_string(), kind: ErrorKind::Other }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Task {
    Users,
    Agents,
}

impl Task {
    pub const ALL: [Task; 2] = [Task::Users, Task::Agents];

    fn error_name(self) -> &'static str {
        match self {
            Task::Users => "BackendUserSyncError",
            Task::Agents => "BackendAgentSyncError",
        }
    }

    fn err(self, message: impl Into<String>) -> SyncError {
        self.err_kind(ErrorKind::Other, message)
    }

    fn err_kind(self, kind: ErrorKind, message: impl Into<String>) -> SyncError {
        SyncError { name: self.error_name(), message: message.into(), kind }
    }

    /// Name used for schedules and run history (unchanged, so existing schedule rows keep working).
    pub fn name(self) -> &'static str {
        match self {
            Task::Users => "sync_all_backend_users",
            Task::Agents => "sync_all_backend_agents",
        }
    }

    /// Task name registered in Celery, used to import old beat schedules.
    pub fn celery_name(self) -> &'static str {
        match self {
            Task::Users => "tenant.tasks.sync_all_backend_users",
            Task::Agents => "tenant.tasks.sync_all_backend_agents",
        }
    }

    pub fn kind(self) -> Kind {
        match self {
            Task::Users => Kind::Users,
            Task::Agents => Kind::Agents,
        }
    }

    pub fn entity(self) -> &'static str {
        self.kind().entity()
    }
}

// ------------------------------------------------------------------ wire formats

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotPage {
    pub epoch: String,
    pub head_version: u64,
    pub keys: Vec<String>,
    pub next_after: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Upsert,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub op: Op,
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangesPage {
    Delta { epoch: String, version: u64, has_more: bool, changes: Vec<Change> },
    Reset { reason: String },
}

/// Non-negative JSON integer (not a bool or float).
fn uint(m: &Map<String, Value>, name: &str) -> Option<u64> {
    match m.get(name)? {
        Value::Number(n) if !n.is_f64() => n.as_u64(),
        _ => None,
    }
}

fn string<'a>(m: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    m.get(name).and_then(Value::as_str)
}

pub fn parse_snapshot(data: &Map<String, Value>) -> Result<SnapshotPage, String> {
    let epoch = string(data, "epoch").ok_or("snapshot: epoch is missing")?.to_string();
    let head_version = uint(data, "head_version").ok_or("snapshot: head_version is missing")?;
    let items = data.get("items").and_then(Value::as_array).ok_or("snapshot: items must be a list")?;
    let mut keys = Vec::with_capacity(items.len());
    for item in items {
        let key = item.as_object().and_then(|i| string(i, "key")).ok_or("snapshot: each item needs a key")?;
        keys.push(key.to_string());
    }
    let next_after = match data.get("next_after") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err("snapshot: next_after must be a string or null".into()),
    };
    Ok(SnapshotPage { epoch, head_version, keys, next_after })
}

pub fn parse_changes(data: &Map<String, Value>) -> Result<ChangesPage, String> {
    match string(data, "mode") {
        Some("reset") => Ok(ChangesPage::Reset { reason: string(data, "reason").unwrap_or("unknown").to_string() }),
        Some("delta") => {
            let epoch = string(data, "epoch").ok_or("changes: epoch is missing")?.to_string();
            let version = uint(data, "version").ok_or("changes: version is missing")?;
            let has_more = data.get("has_more").and_then(Value::as_bool).unwrap_or(false);
            let list = data.get("changes").and_then(Value::as_array).ok_or("changes: changes must be a list")?;
            let mut changes = Vec::with_capacity(list.len());
            for c in list {
                let c = c.as_object().ok_or("changes: each change must be an object")?;
                let key = string(c, "key").ok_or("changes: each change needs a key")?.to_string();
                let op = match string(c, "op") {
                    Some("upsert") => Op::Upsert,
                    Some("delete") => Op::Delete,
                    _ => return Err("changes: op must be upsert or delete".into()),
                };
                changes.push(Change { op, key });
            }
            Ok(ChangesPage::Delta { epoch, version, has_more, changes })
        }
        _ => Err("changes: mode must be delta or reset".into()),
    }
}

/// Server limits advertised by `GET /internal/info` (defaults of the spec when absent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub snapshot_max: u32,
    pub changes_max: u32,
    pub longpoll_max_wait: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { snapshot_max: 5000, changes_max: 2000, longpoll_max_wait: 30 }
    }
}

pub fn parse_limits(data: &Map<String, Value>) -> Limits {
    let d = Limits::default();
    let Some(l) = data.get("limits").and_then(Value::as_object) else { return d };
    let pos = |name: &str| uint(l, name).filter(|v| *v > 0);
    Limits {
        snapshot_max: pos("snapshot_max_limit").map(|v| v.min(u32::MAX as u64) as u32).unwrap_or(d.snapshot_max),
        changes_max: pos("changes_max_limit").map(|v| v.min(u32::MAX as u64) as u32).unwrap_or(d.changes_max),
        longpoll_max_wait: uint(l, "longpoll_max_wait_s").unwrap_or(d.longpoll_max_wait),
    }
}

/// `backend_id` reported by `GET /internal/info`.
pub fn parse_backend_id(data: &Map<String, Value>) -> Option<&str> {
    string(data, "backend_id")
}

/// `(epoch, version)` of one stream in `GET /internal/info`.
pub fn parse_info(data: &Map<String, Value>, entity: &str) -> Result<(String, u64), String> {
    let e = data
        .get("entities")
        .and_then(Value::as_object)
        .and_then(|m| m.get(entity))
        .and_then(Value::as_object)
        .ok_or_else(|| format!("info: stream {entity} is not advertised"))?;
    let epoch = string(e, "epoch").ok_or("info: epoch is missing")?.to_string();
    let version = uint(e, "version").ok_or("info: version is missing")?;
    Ok((epoch, version))
}

/// Canonical stored form of a stream key: user keys lowercase, agent keys as canonical UUIDs.
pub fn normalize_key(kind: Kind, raw: &str) -> Result<String, String> {
    match kind {
        Kind::Users => {
            let key = raw.trim().to_ascii_lowercase();
            if !key.starts_with("entra:") || key.len() > 255 {
                // The key itself is sensitive (spec 10.6): never put it into logs or run results.
                return Err(format!("invalid user key ({} bytes, expected entra:<tid>:<oid>)", raw.len()));
            }
            Ok(key)
        }
        Kind::Agents => canonical_uuid(raw).ok_or_else(|| "invalid agent key (expected a UUID)".to_string()),
    }
}

/// Digest checksum of a stream (spec DIG-1): SHA-256 over every live claim in ascending bytewise key order,
/// each as `u32be(len key) key u32be(len state) state data_hash`. Every stored claim is `state="active"`
/// with `data={}`.
pub fn checksum(keys: &[String]) -> String {
    let mut sorted: Vec<&str> = keys.iter().map(String::as_str).collect();
    sorted.sort_unstable(); // `str` order is bytewise UTF-8 order
    let mut h = Sha256::new();
    for key in sorted {
        h.update((key.len() as u32).to_be_bytes());
        h.update(key.as_bytes());
        h.update(("active".len() as u32).to_be_bytes());
        h.update(b"active");
        h.update(ACTIVE_DATA_HASH.as_bytes());
    }
    let digest = h.finalize();
    let mut out = String::with_capacity(7 + 64);
    out.push_str("sha256:");
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Message of a spec error envelope `{"error": {"code", "message", "request_id"}}`, or the raw text.
fn error_text(body: &str) -> (Option<String>, String) {
    if let Ok(Value::Object(m)) = serde_json::from_str::<Value>(body) {
        if let Some(Value::Object(e)) = m.get("error") {
            let code = e.get("code").and_then(Value::as_str).map(str::to_string);
            let msg = e.get("message").and_then(Value::as_str).unwrap_or("");
            let rid = e.get("request_id").and_then(Value::as_str).unwrap_or("");
            return (code.clone(), format!("{} {msg} (request_id {rid})", code.unwrap_or_default()).trim().to_string());
        }
    }
    (None, body.chars().take(300).collect())
}

// ------------------------------------------------------------------ syncer

/// Result of one long-poll round.
#[derive(Debug)]
pub enum PollOutcome {
    /// Nothing changed within the wait.
    Idle,
    /// A page was applied; `has_more` asks for the next page right away.
    Applied { counts: ApplyCounts, to: Cursor, has_more: bool },
    /// The stored cursor moved meanwhile (a resync ran); re-read it.
    Stale,
    /// The backend asked for a full resync (`epoch_changed`, `cursor_ahead`, `history_pruned`).
    Reset(String),
}

/// A stream as advertised by `GET /internal/info`.
#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub epoch: String,
    pub version: u64,
    pub limits: Limits,
}

#[derive(Clone)]
pub struct Syncer {
    pub pool: PgPool,
    http: reqwest::Client,
    credentials: Arc<SyncCredentials>,
    base_url: String,
    host_header: String,
    wait: u64,
}

impl Syncer {
    pub fn new(pool: PgPool, s: &Settings) -> Result<Syncer> {
        let http = reqwest::Client::builder()
            // Internal requests must bypass inherited HTTP_PROXY/HTTPS_PROXY settings.
            .no_proxy()
            // GET-6: never follow redirects.
            .redirect(redirect::Policy::none())
            .connect_timeout(s.backend_sync_connect_timeout)
            .read_timeout(s.backend_sync_read_timeout)
            .pool_idle_timeout(Duration::from_secs(90))
            .build()?;
        let credentials = SyncCredentials::new(&s.sync_auth, outbound::client(s)?);
        if let Some(w) = credentials.startup_warning() {
            tracing::warn!("{w}");
        }
        tracing::debug!(method = credentials.method(), "routing sync credentials");
        Ok(Syncer {
            pool,
            http,
            credentials: Arc::new(credentials),
            base_url: s.nginx_internal_base_url.clone(),
            host_header: s.nginx_internal_host_header.clone(),
            wait: s.sync_longpoll_wait,
        })
    }

    pub fn url(&self, backend: &Backend, endpoint: &str) -> String {
        format!("{}/t/{}/{}", self.base_url, backend.prefix_key, endpoint)
    }

    /// GET a sync endpoint and return its `data` object.
    async fn get(
        &self,
        backend: &Backend,
        endpoint: &str,
        query: &[(&str, String)],
        task: Task,
    ) -> Result<Map<String, Value>, SyncError> {
        let url = self.url(backend, endpoint);
        let fail = |kind: ErrorKind, cause: String| {
            task.err_kind(kind, format!("{endpoint} on {}: {cause}", backend.prefix_key))
        };
        let mut retried = false;
        let resp = loop {
            let token = self.credentials.bearer(backend).await.map_err(|e| fail(ErrorKind::Other, e))?;
            let request_id = uuid::Uuid::new_v4().simple().to_string();
            let mut req = self
                .http
                .get(&url)
                .header(HOST, &self.host_header)
                .header(ACCEPT, "application/json")
                .header("X-Request-ID", &request_id)
                .query(query);
            if let Some(t) = &token {
                req = req.header(AUTHORIZATION, format!("Bearer {t}"));
            }
            let resp = req.send().await.map_err(|e| fail(ErrorKind::Other, e.to_string()))?;
            // An Entra token can be revoked or rotated before it expires: fetch a new one once.
            if resp.status() == StatusCode::UNAUTHORIZED && !retried && self.credentials.invalidate(backend).await {
                retried = true;
                continue;
            }
            break resp;
        };
        let status = resp.status();
        if status.is_success() {
            let payload: Value = resp.json().await.map_err(|e| fail(ErrorKind::Other, format!("invalid JSON: {e}")))?;
            return match payload {
                Value::Object(mut m) => match m.remove("data") {
                    Some(Value::Object(d)) => Ok(d),
                    _ => Err(fail(ErrorKind::Other, "response has no data object".into())),
                },
                _ => Err(fail(ErrorKind::Other, "response must be an object".into())),
            };
        }
        let body = resp.text().await.unwrap_or_default();
        let (code, text) = error_text(&body);
        let kind = match (status, code.as_deref()) {
            (StatusCode::NOT_FOUND, Some("entity_not_supported")) => ErrorKind::NotAdvertised,
            (StatusCode::NOT_FOUND, Some("not_found")) => ErrorKind::NotFound,
            _ => ErrorKind::Other,
        };
        let hint = match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                " (check the sync credential: ROUTING_SYNC_TOKEN / ROUTING_SYNC_TOKENS_DIR, or the Entra app role)"
            }
            s if s.is_redirection() => {
                " (redirects are not followed: check NGINX_INTERNAL_BASE_URL and the backend URL)"
            }
            StatusCode::NOT_FOUND if code.is_none() => " (is the backend routed by nginx? check its URL on /admin/)",
            _ => "",
        };
        Err(fail(kind, format!("HTTP {status}: {text}{hint}")))
    }

    /// `GET /internal/info`: verifies the backend identity (CNS-2) and returns the stream, or a
    /// [`ErrorKind::NotAdvertised`] error when the backend does not publish it (yet).
    pub async fn stream_info(&self, backend: &Backend, task: Task) -> Result<StreamInfo, SyncError> {
        let info = self.get(backend, INFO, &[], task).await?;
        if !backend.backend_id.is_empty() {
            let reported = parse_backend_id(&info).unwrap_or("");
            if reported != backend.backend_id {
                return Err(task.err_kind(
                    ErrorKind::WrongBackend,
                    format!(
                        "{} reports backend_id {reported:?}, the registry expects {:?}; nothing applied (check its URL)",
                        backend.prefix_key, backend.backend_id
                    ),
                ));
            }
        }
        let limits = parse_limits(&info);
        match parse_info(&info, task.entity()) {
            Ok((epoch, version)) => Ok(StreamInfo { epoch, version, limits }),
            Err(_) => Err(task.err_kind(
                ErrorKind::NotAdvertised,
                format!("{} does not advertise the {} stream yet", backend.prefix_key, task.entity()),
            )),
        }
    }

    async fn changes(
        &self,
        backend: &Backend,
        task: Task,
        epoch: &str,
        since: u64,
        wait: u64,
        limit: u32,
    ) -> Result<ChangesPage, SyncError> {
        let mut query = vec![("since", since.to_string()), ("epoch", epoch.to_string()), ("limit", limit.to_string())];
        if wait > 0 {
            query.push(("wait", wait.to_string()));
        }
        let data = self.get(backend, &format!("internal/changes/{}", task.entity()), &query, task).await?;
        let page = parse_changes(&data).map_err(|m| task.err(m))?;
        // The backend answers another epoch with a reset; treat a mismatching delta the same way.
        if let ChangesPage::Delta { epoch: got, .. } = &page {
            if got != epoch {
                return Ok(ChangesPage::Reset { reason: "epoch_changed".into() });
            }
        }
        Ok(page)
    }

    /// Rows to store: the key only (the extra column stays empty).
    fn records(keys: Vec<String>) -> Vec<(String, String)> {
        keys.into_iter().map(|k| (k, String::new())).collect()
    }

    /// Full snapshot + replay, then atomic publish of the list and its cursor.
    pub async fn resync(&self, backend: &Backend, task: Task) -> Result<Value, SyncError> {
        let kind = task.kind();
        let entity = task.entity();
        let limits = self.stream_info(backend, task).await?.limits;
        let snapshot_limit = SNAPSHOT_LIMIT.min(limits.snapshot_max);
        let changes_limit = CHANGES_LIMIT.min(limits.changes_max);
        let mut staged: HashSet<String> = HashSet::new();
        let mut after: Option<String> = None;
        let mut watermark: Option<(String, u64)> = None;

        loop {
            let mut query = vec![("limit", snapshot_limit.to_string())];
            if let Some(a) = &after {
                query.push(("after", a.clone()));
            }
            let data = self.get(backend, &format!("internal/snapshot/{entity}"), &query, task).await?;
            let page = parse_snapshot(&data).map_err(|m| task.err(m))?;
            match watermark.as_ref().map(|(epoch, _)| *epoch != page.epoch) {
                None => watermark = Some((page.epoch.clone(), page.head_version)),
                Some(true) => return Err(task.err("epoch changed during the snapshot; retrying")),
                Some(false) => {}
            }
            for k in &page.keys {
                staged.insert(normalize_key(kind, k).map_err(|m| task.err(m))?);
            }
            match page.next_after {
                Some(n) => after = Some(n),
                None => break,
            }
        }

        let (epoch, mut version) = watermark.expect("the first snapshot page sets the watermark");
        let mut pages = 0;
        loop {
            match self.changes(backend, task, &epoch, version, 0, changes_limit).await? {
                ChangesPage::Reset { reason } => {
                    return Err(task.err(format!("stream reset during resync ({reason}); retrying")));
                }
                ChangesPage::Delta { version: v, has_more, changes, .. } => {
                    for c in changes {
                        let key = normalize_key(kind, &c.key).map_err(|m| task.err(m))?;
                        match c.op {
                            Op::Upsert => staged.insert(key),
                            Op::Delete => staged.remove(&key),
                        };
                    }
                    version = v;
                    if !has_more {
                        break;
                    }
                }
            }
            pages += 1;
            if pages >= MAX_REPLAY_PAGES {
                return Err(task.err("too many change pages during resync; retrying"));
            }
        }

        let mut keys: Vec<String> = staged.into_iter().collect();
        keys.sort();
        let records = Self::records(keys);
        let cursor = Cursor { epoch: epoch.clone(), version: version as i64 };
        let status = match mappings::replace_all(&self.pool, backend.id, kind, &records, &cursor).await? {
            Some(c) => json!({
                "status": "resynced", "backend": backend.prefix_key, "epoch": epoch, "version": version,
                "received": c.received, "created": c.created, "updated": c.updated, "deactivated": c.deactivated,
            }),
            // A concurrent long-poll already stored a newer position of the same history.
            None => json!({"status": "ok", "backend": backend.prefix_key, "note": "newer cursor already stored"}),
        };
        tracing::info!(backend = %backend.prefix_key, entity, result = %status, "resync finished");
        Ok(status)
    }

    /// One long-poll round from `cursor`.
    pub async fn poll_once(&self, backend: &Backend, task: Task, cursor: &Cursor) -> Result<PollOutcome, SyncError> {
        let since = u64::try_from(cursor.version).unwrap_or(0);
        let (version, has_more, changes) =
            match self.changes(backend, task, &cursor.epoch, since, self.wait, CHANGES_LIMIT).await? {
                ChangesPage::Reset { reason } => return Ok(PollOutcome::Reset(reason)),
                ChangesPage::Delta { version, has_more, changes, .. } => (version, has_more, changes),
            };
        if changes.is_empty() && version == since {
            return Ok(PollOutcome::Idle);
        }
        let kind = task.kind();
        let (mut upserts, mut deletes) = (Vec::new(), Vec::new());
        for c in changes {
            // The backend sends only the latest change per key, so a key appears once per page.
            let key = normalize_key(kind, &c.key).map_err(|m| task.err(m))?;
            match c.op {
                Op::Upsert => upserts.push(key),
                Op::Delete => deletes.push(key),
            }
        }
        let upserts = Self::records(upserts);
        let to = Cursor { epoch: cursor.epoch.clone(), version: version as i64 };
        match mappings::apply_delta(&self.pool, backend.id, kind, cursor, &upserts, &deletes, &to).await? {
            Some(counts) => Ok(PollOutcome::Applied { counts, to, has_more }),
            None => Ok(PollOutcome::Stale),
        }
    }

    /// Safety-net check of one stream: resync when there is no cursor, the epoch changed, the cursor is ahead
    /// of the backend, or (cursor at the backend's head) the local claims differ from the backend digest
    /// (count or checksum).
    pub async fn check(&self, backend: &Backend, task: Task) -> Result<Value, SyncError> {
        let kind = task.kind();
        let info = match self.stream_info(backend, task).await {
            Ok(i) => i,
            Err(e) if e.kind == ErrorKind::NotAdvertised => {
                return Ok(json!({"status": "not_advertised", "backend": backend.prefix_key}));
            }
            Err(e) => return Err(e),
        };
        let Some(cursor) = mappings::get_cursor(&self.pool, backend.id, kind).await? else {
            return self.resync(backend, task).await;
        };
        let head = info.version;
        if info.epoch != cursor.epoch || cursor.version > head as i64 {
            return self.resync(backend, task).await;
        }
        if cursor.version < head as i64 {
            // The long-poll worker is catching up; nothing to compare yet.
            return Ok(
                json!({"status": "behind", "backend": backend.prefix_key, "version": cursor.version, "head": head}),
            );
        }
        let digest = match self.get(backend, &format!("internal/digest/{}", task.entity()), &[], task).await {
            Ok(d) => d,
            Err(e) if e.kind == ErrorKind::NotFound => {
                return Ok(
                    json!({"status": "ok", "backend": backend.prefix_key, "version": cursor.version, "digest": "not supported"}),
                );
            }
            Err(e) => return Err(e),
        };
        let same_point = string(&digest, "epoch") == Some(cursor.epoch.as_str())
            && uint(&digest, "version") == Some(cursor.version as u64);
        if same_point {
            let keys = mappings::active_keys(&self.pool, backend.id, kind).await?;
            let remote_count = uint(&digest, "count").unwrap_or(0) as usize;
            let remote_sum = string(&digest, "checksum");
            let local_sum = checksum(&keys);
            let sum_differs = remote_sum.is_some_and(|r| r != local_sum);
            if remote_count != keys.len() || sum_differs {
                tracing::warn!(
                    backend = %backend.prefix_key, entity = task.entity(), remote = remote_count, local = keys.len(),
                    checksum_differs = sum_differs, "digest drift, resyncing"
                );
                return self.resync(backend, task).await;
            }
        }
        Ok(json!({"status": "ok", "backend": backend.prefix_key, "version": cursor.version}))
    }

    /// Every active backend, one failure does not stop the others. `force` resyncs every stream;
    /// otherwise each stream is only checked (see [`Syncer::check`]).
    pub async fn sync_all(&self, task: Task, force: bool) -> Result<Value, sqlx::Error> {
        let mut statuses: Map<String, Value> = Map::new();
        let mut entries = Vec::new();
        let mut failed = 0usize;
        let backends = backend_db::active_backends(&self.pool).await?;
        for backend in &backends {
            let outcome = if force { self.resync(backend, task).await } else { self.check(backend, task).await };
            let entry = outcome.unwrap_or_else(|e| {
                if e.kind == ErrorKind::NotAdvertised {
                    return json!({"status": "not_advertised", "backend": backend.prefix_key});
                }
                tracing::error!(backend = %backend.prefix_key, entity = task.entity(), "sync failed: {e}");
                json!({"status": "failed", "backend": backend.prefix_key, "error": e.to_string()})
            });
            let status = entry.get("status").and_then(Value::as_str).unwrap_or("ok").to_string();
            if status == "failed" {
                failed += 1;
            }
            let n = statuses.get(&status).and_then(Value::as_u64).unwrap_or(0);
            statuses.insert(status, json!(n + 1));
            entries.push(entry);
        }
        // CNS-6: report (and log) keys claimed by more than one backend; the routing API refuses them.
        let (conflict_count, _) = conflicts(&self.pool, task.kind(), 0).await?;
        if conflict_count > 0 {
            tracing::warn!(
                entity = task.entity(),
                conflicts = conflict_count,
                "keys are claimed by more than one backend"
            );
        }
        Ok(json!({
            "total": backends.len(),
            "succeeded": backends.len() - failed,
            "failed": failed,
            "statuses": statuses,
            "conflicts": conflict_count,
            "backends": entries,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn snapshot_page() {
        let p = parse_snapshot(&obj(json!({
            "backend_id": "dept_a", "entity_type": "agents", "epoch": "e1", "head_version": 3,
            "items": [{"key": "k1", "version": 1, "state": "active", "data": {}}],
            "next_after": "k1"
        })))
        .unwrap();
        assert_eq!(
            p,
            SnapshotPage {
                epoch: "e1".into(),
                head_version: 3,
                keys: vec!["k1".into()],
                next_after: Some("k1".into())
            }
        );
        let last =
            parse_snapshot(&obj(json!({"epoch": "e1", "head_version": 0, "items": [], "next_after": null}))).unwrap();
        assert_eq!(last.next_after, None);
        assert!(parse_snapshot(&obj(json!({"epoch": "e1", "head_version": -1, "items": []}))).is_err());
        assert!(parse_snapshot(&obj(json!({"epoch": "e1", "head_version": 1, "items": [{}]}))).is_err());
    }

    #[test]
    fn changes_page() {
        let d = parse_changes(&obj(json!({
            "epoch": "e1", "mode": "delta", "version": 3, "head_version": 3, "has_more": false,
            "oldest_retained_version": 1,
            "changes": [{"version": 3, "op": "delete", "key": "k"}, {"version": 2, "op": "upsert", "key": "j", "state": "active", "data": {}}]
        })))
        .unwrap();
        assert_eq!(
            d,
            ChangesPage::Delta {
                epoch: "e1".into(),
                version: 3,
                has_more: false,
                changes: vec![Change { op: Op::Delete, key: "k".into() }, Change { op: Op::Upsert, key: "j".into() }],
            }
        );
        assert_eq!(
            parse_changes(&obj(json!({"mode": "reset", "reason": "epoch_changed", "epoch": "e2", "head_version": 5})))
                .unwrap(),
            ChangesPage::Reset { reason: "epoch_changed".into() }
        );
        assert!(parse_changes(&obj(
            json!({"mode": "delta", "epoch": "e", "version": 1, "changes": [{"op": "x", "key": "k"}]})
        ))
        .is_err());
        assert!(parse_changes(&obj(json!({"mode": "other"}))).is_err());
    }

    #[test]
    fn info() {
        let info = obj(json!({"backend_id": "dept_a", "entities": {"users": {"version": 2, "epoch": "e"}}}));
        assert_eq!(parse_info(&info, "users").unwrap(), ("e".to_string(), 2));
        assert!(parse_info(&info, "agents").is_err());
    }

    #[test]
    fn digest_test_vectors() {
        // Appendix E of the backend routing-sync spec.
        let users = vec![
            "entra:11111111-2222-3333-4444-555555555555:aaaaaaaa-0000-0000-0000-000000000002".to_string(),
            "entra:11111111-2222-3333-4444-555555555555:aaaaaaaa-0000-0000-0000-000000000001".to_string(),
        ];
        assert_eq!(checksum(&users), "sha256:e55f21dda5b3742e8baa44eeadfb712312907f2226c940cdb9dc843b5dce5a13");
        assert_eq!(
            checksum(&["bbbbbbbb-1111-0000-0000-000000000001".to_string()]),
            "sha256:2ce74e51f5d7cb73601acd527318a602672acf5cb3f712dcf1b18ac183f96e70"
        );
        assert_eq!(checksum(&[]), "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        let mut h = Sha256::new();
        h.update(br#"{"data":{},"state":"active"}"#);
        let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, ACTIVE_DATA_HASH);
    }

    #[test]
    fn info_limits_and_identity() {
        let info = obj(json!({"backend_id": "dept_a", "entities": {},
            "limits": {"snapshot_max_limit": 200, "changes_max_limit": 100, "longpoll_max_wait_s": 10}}));
        assert_eq!(parse_limits(&info), Limits { snapshot_max: 200, changes_max: 100, longpoll_max_wait: 10 });
        assert_eq!(parse_limits(&obj(json!({}))), Limits::default());
        assert_eq!(parse_backend_id(&info), Some("dept_a"));
    }

    #[test]
    fn error_envelopes() {
        let (code, text) = error_text(
            r#"{"error": {"code": "entity_not_supported", "message": "Entity not enabled or not ready.", "details": [], "request_id": "abc"}}"#,
        );
        assert_eq!(code.as_deref(), Some("entity_not_supported"));
        assert!(text.contains("request_id abc"));
        assert_eq!(error_text("<html>").0, None);
    }

    #[test]
    fn keys_are_normalized() {
        assert_eq!(
            normalize_key(Kind::Agents, " BBBBBBBB-1111-4000-8000-000000000001 ").unwrap(),
            "bbbbbbbb-1111-4000-8000-000000000001"
        );
        assert!(normalize_key(Kind::Agents, "HC-1").is_err());
        assert_eq!(normalize_key(Kind::Users, "ENTRA:X:Y").unwrap(), "entra:x:y");
        assert!(normalize_key(Kind::Users, "alice").is_err());
    }
}
