//! Runtime settings, read from the environment (and an optional `.env` file).

use std::{env, fs, path::Path, time::Duration};

use anyhow::{bail, Context, Result};

pub const DEFAULT_AAD_AUDIENCE: &str = "eb57f92a-fe50-46b7-b0d8-775cb6b9bdc6";
pub const DEFAULT_AAD_JWKS_URL: &str =
    "https://login.microsoftonline.com/0ae51e19-07c8-4e4b-bb6d-648ee58410f4/discovery/v2.0/keys";

#[derive(Debug, Clone)]
pub struct Settings {
    /// Address the HTTP API listens on. `LISTEN_ADDR`, else `0.0.0.0:$HOST_PORT`, else `0.0.0.0:8000`.
    pub listen_addr: String,
    pub database_url: String,
    pub db_max_connections: u32,
    /// Create or upgrade the tables at start-up.
    pub auto_migrate: bool,

    pub aad_audience: String,
    pub aad_jwks_url: String,
    /// How long fetched signing keys are trusted before a background refresh.
    pub aad_jwks_cache_ttl: Duration,
    /// Extra PEM CA bundle for outbound HTTPS (JWKS). Defaults to the system bundle when present.
    pub ca_bundle: Option<String>,

    pub nginx_internal_base_url: String,
    pub nginx_internal_host_header: String,
    pub backend_sync_connect_timeout: Duration,
    pub backend_sync_read_timeout: Duration,
    /// How the Router authenticates to the backends' `/internal/*` API (`ROUTING_SYNC_AUTH`).
    pub sync_auth: SyncAuthSettings,
    /// Long-poll wait sent as `wait=` to `/internal/changes/{entity}` (`SYNC_LONGPOLL_WAIT`, 0..30 s).
    pub sync_longpoll_wait: u64,

    /// Master switch for the scheduler and the stream sync. Schedules live in the database.
    pub sync_enabled: bool,
    /// Initial intervals, used only the first time the schedule rows are created
    /// (a legacy Celery beat interval found in the database takes precedence).
    pub user_sync_interval: Duration,
    pub agent_sync_interval: Duration,

    /// Git-managed nginx map (read-only here), e.g. the mounted `deploy/nginx/prod/tenants/backends.map`.
    pub nginx_map_file: String,
    /// Shared volume with the nginx container: generated map, reload request, nginx status.
    pub nginx_admin_dir: String,

    /// Admin account for /admin/ and /admin/api/*. Both must be set, otherwise admin is disabled.
    pub admin_username: Option<String>,
    pub admin_password: Option<String>,

    /// What the routing API answers when a key is claimed by more than one backend (`ROUTING_CONFLICT_POLICY`).
    pub conflict_policy: ConflictPolicy,
    /// Only one replica runs the stream sync and the scheduler (PostgreSQL advisory lock, `LEADER_ELECTION`).
    pub leader_election: bool,
}

/// Routing-spec CNS-6: a key claimed by several backends is a conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictPolicy {
    /// Resolve to nothing (HTTP 409) and log a warning. Default, as required by the backend spec.
    Reject,
    /// Legacy behaviour: return every claiming backend and let the client choose.
    All,
}

/// Sync credential (routing-spec section 10.3).
#[derive(Debug, Clone)]
pub enum SyncAuthSettings {
    /// Method B: static bearer token. `token` is the default for every backend; a file
    /// `<tokens_dir>/<prefix_key>` overrides it per backend (one token per backend, AUTH-2).
    Static { token: Option<String>, tokens_dir: Option<String> },
    /// Method A: Entra application token (OAuth2 client credentials), one audience per backend.
    Entra {
        tenant_id: String,
        client_id: String,
        client_secret: String,
        /// `{backend_id}` is replaced with the backend's registered `backend_id`.
        scope_template: String,
        /// Token endpoint; `{tenant}` is replaced with `tenant_id`.
        token_url: String,
    },
}

impl Settings {
    pub fn from_env() -> Result<Settings> {
        load_env_file();

        let listen_addr = match var("LISTEN_ADDR") {
            Some(a) => a,
            None => format!("0.0.0.0:{}", var("HOST_PORT").unwrap_or_else(|| "8000".into())),
        };

        let database_url = normalize_db_url(&var("DATABASE_URL").context("DATABASE_URL is not set")?)?;

        let ca_bundle = match var("CA_BUNDLE") {
            Some(p) if p.is_empty() => None,
            Some(p) => Some(p),
            None => ["/etc/ssl/certs/ca-certificates.crt", "/etc/pki/tls/certs/ca-bundle.crt"]
                .into_iter()
                .find(|p| Path::new(p).exists())
                .map(str::to_string),
        };

        let settings = Settings {
            listen_addr,
            database_url,
            db_max_connections: parse("DB_MAX_CONNECTIONS", 20u32)?,
            auto_migrate: parse_bool("AUTO_MIGRATE", true)?,

            aad_audience: var("AAD_AUDIENCE").unwrap_or_else(|| DEFAULT_AAD_AUDIENCE.into()),
            aad_jwks_url: var("AAD_JWKS_URL").unwrap_or_else(|| DEFAULT_AAD_JWKS_URL.into()),
            aad_jwks_cache_ttl: Duration::from_secs(parse("AAD_JWKS_CACHE_TTL", 3600u64)?),
            ca_bundle,

            nginx_internal_base_url: var("NGINX_INTERNAL_BASE_URL")
                .unwrap_or_else(|| "http://nginx:8080".into())
                .trim_end_matches('/')
                .to_string(),
            nginx_internal_host_header: var("NGINX_INTERNAL_HOST_HEADER").unwrap_or_else(|| "localhost".into()),
            backend_sync_connect_timeout: secs_f64("BACKEND_SYNC_CONNECT_TIMEOUT", 5.0)?,
            backend_sync_read_timeout: secs_f64("BACKEND_SYNC_READ_TIMEOUT", 60.0)?,
            sync_auth: sync_auth_from_env()?,
            sync_longpoll_wait: parse("SYNC_LONGPOLL_WAIT", 30u64)?,

            sync_enabled: parse_bool("SYNC_ENABLED", true)?,
            user_sync_interval: Duration::from_secs(parse("USER_SYNC_INTERVAL", 300u64)?),
            agent_sync_interval: Duration::from_secs(parse("AGENT_SYNC_INTERVAL", 60u64)?),

            nginx_map_file: var("NGINX_MAP_FILE").unwrap_or_else(|| "/nginx-tenants/backends.map".into()),
            nginx_admin_dir: var("NGINX_ADMIN_DIR").unwrap_or_else(|| "/nginx-admin".into()),

            // Not trimmed beyond `var`: spaces at the ends of a password in .env are almost always a mistake.
            admin_username: var("ADMIN_USERNAME"),
            admin_password: var("ADMIN_PASSWORD"),

            conflict_policy: match var("ROUTING_CONFLICT_POLICY").as_deref().map(str::to_ascii_lowercase).as_deref() {
                None | Some("reject") => ConflictPolicy::Reject,
                Some("all") => ConflictPolicy::All,
                Some(v) => bail!("ROUTING_CONFLICT_POLICY={v:?} must be reject or all"),
            },
            leader_election: parse_bool("LEADER_ELECTION", true)?,
        };
        if settings.sync_longpoll_wait > 30 {
            bail!("SYNC_LONGPOLL_WAIT must be between 0 and 30 seconds (the backend maximum)");
        }
        // A held long-poll must end on the server's side, not on our read timeout.
        if settings.backend_sync_read_timeout.as_secs_f64() < settings.sync_longpoll_wait as f64 + 10.0 {
            bail!("BACKEND_SYNC_READ_TIMEOUT must be at least SYNC_LONGPOLL_WAIT + 10 seconds");
        }
        Ok(settings)
    }
}

fn sync_auth_from_env() -> Result<SyncAuthSettings> {
    let mode = var("ROUTING_SYNC_AUTH").unwrap_or_else(|| "static".into()).to_ascii_lowercase();
    match mode.as_str() {
        "static" => Ok(SyncAuthSettings::Static {
            token: secret("ROUTING_SYNC_TOKEN")?,
            tokens_dir: var("ROUTING_SYNC_TOKENS_DIR"),
        }),
        "entra" => {
            let need =
                |name: &str| var(name).with_context(|| format!("{name} is required when ROUTING_SYNC_AUTH=entra"));
            Ok(SyncAuthSettings::Entra {
                tenant_id: need("ROUTING_SYNC_ENTRA_TENANT_ID")?,
                client_id: need("ROUTING_SYNC_CLIENT_ID")?,
                client_secret: secret("ROUTING_SYNC_CLIENT_SECRET")?
                    .context("ROUTING_SYNC_CLIENT_SECRET or ROUTING_SYNC_CLIENT_SECRET_FILE is required when ROUTING_SYNC_AUTH=entra")?,
                scope_template: var("ROUTING_SYNC_SCOPE_TEMPLATE")
                    .unwrap_or_else(|| "api://labmango-{backend_id}/.default".into()),
                token_url: var("ROUTING_SYNC_TOKEN_URL")
                    .unwrap_or_else(|| "https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token".into()),
            })
        }
        other => bail!("ROUTING_SYNC_AUTH={other:?} must be static or entra"),
    }
}

/// Secret from `NAME`, or from the file named by `NAME_FILE` (Docker secrets, AUTH-7). The file wins.
fn secret(name: &str) -> Result<Option<String>> {
    if let Some(path) = var(&format!("{name}_FILE")) {
        let text = fs::read_to_string(&path).with_context(|| format!("read {name}_FILE {path}"))?;
        let text = text.trim().to_string();
        return Ok((!text.is_empty()).then_some(text));
    }
    Ok(var(name))
}

/// Non-empty environment variable.
fn var(name: &str) -> Option<String> {
    env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn parse<T: std::str::FromStr>(name: &str, default: T) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    match var(name) {
        None => Ok(default),
        Some(v) => v.parse::<T>().map_err(|e| anyhow::anyhow!("{name}={v:?}: {e}")),
    }
}

fn secs_f64(name: &str, default: f64) -> Result<Duration> {
    let v = parse(name, default)?;
    if !(v.is_finite() && v > 0.0) {
        bail!("{name} must be a positive number of seconds");
    }
    Ok(Duration::from_secs_f64(v))
}

/// Accepted boolean spellings: true/on/ok/y/yes/1 and false/off/n/no/0.
fn parse_bool(name: &str, default: bool) -> Result<bool> {
    match var(name) {
        None => Ok(default),
        Some(v) => match v.to_ascii_lowercase().as_str() {
            "true" | "on" | "ok" | "y" | "yes" | "1" => Ok(true),
            "false" | "off" | "n" | "no" | "0" => Ok(false),
            _ => bail!("{name}={v:?} is not a boolean"),
        },
    }
}

/// Accepts `psql://`, `pgsql://`, `postgis://`, `postgresql://` and `postgres://`; sqlx wants the last two.
fn normalize_db_url(url: &str) -> Result<String> {
    for (from, to) in [("psql://", "postgres://"), ("pgsql://", "postgres://"), ("postgis://", "postgres://")] {
        if let Some(rest) = url.strip_prefix(from) {
            return Ok(format!("{to}{rest}"));
        }
    }
    if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        return Ok(url.to_string());
    }
    bail!("DATABASE_URL must be a PostgreSQL URL (postgres://user:pass@host:5432/db)")
}

/// Reads `$ENV_FILE` (default `./.env`) like `environ.Env.read_env`: `KEY=value` lines,
/// optional `export `, optional quotes; variables already set in the environment win.
fn load_env_file() {
    let path = env::var("ENV_FILE").unwrap_or_else(|_| ".env".into());
    let Ok(text) = fs::read_to_string(&path) else { return };
    for (k, v) in parse_env_text(&text) {
        if env::var_os(&k).is_none() {
            env::set_var(k, v);
        }
    }
}

pub fn parse_env_text(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else { continue };
        let k = k.trim();
        if k.is_empty() || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let v = v.trim();
        let v = if v.len() >= 2
            && ((v.starts_with('"') && v.ends_with('"')) || (v.starts_with('\'') && v.ends_with('\'')))
        {
            &v[1..v.len() - 1]
        } else {
            // Strip an inline comment only when it is separated by whitespace.
            v.split_once(" #").map(|(a, _)| a.trim_end()).unwrap_or(v)
        };
        out.push((k.to_string(), v.to_string()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_file_parsing() {
        let got = parse_env_text("# c\nA=1\nexport B = \"two words\"\nC='x#y'\nD=v # note\nbad line\n");
        assert_eq!(
            got,
            vec![
                ("A".into(), "1".into()),
                ("B".into(), "two words".into()),
                ("C".into(), "x#y".into()),
                ("D".into(), "v".into()),
            ]
        );
    }

    #[test]
    fn db_url_schemes() {
        assert_eq!(normalize_db_url("psql://u:p@h/db").unwrap(), "postgres://u:p@h/db");
        assert_eq!(normalize_db_url("postgresql://u@h/db").unwrap(), "postgresql://u@h/db");
        assert!(normalize_db_url("sqlite:///x.db").is_err());
    }
}
