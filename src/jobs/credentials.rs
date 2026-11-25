//! Credentials the Router sends to the backends' `/internal/*` sync API (routing-spec section 10.3, CNS-11).
//!
//! * **Static token (Method B)**: `ROUTING_SYNC_TOKEN` (or `ROUTING_SYNC_TOKEN_FILE`) for every backend, and
//!   optionally one token per backend in `ROUTING_SYNC_TOKENS_DIR/<prefix_key>` (or `/<backend_id>`), which is
//!   what the spec recommends (AUTH-2: never share one token across backends). The backend stores only the
//!   SHA-256 of the token in `ROUTING_SYNC_STATIC_TOKEN_HASHES`.
//! * **Entra application token (Method A)**: OAuth2 client credentials, one token per backend audience
//!   (`api://labmango-<backend_id>/.default` by default), cached until shortly before it expires and refreshed
//!   after a `401`. Needs the backend's `backend_id` in the registry.

use std::{
    collections::HashMap,
    path::PathBuf,
    time::{Duration, Instant},
};

use serde_json::Value;

use crate::{db::backends::Backend, settings::SyncAuthSettings};

/// Refresh an Entra token this long before it expires.
const TOKEN_EARLY_REFRESH: Duration = Duration::from_secs(300);

struct CachedToken {
    token: String,
    valid_until: Instant,
}

enum Mode {
    Static {
        token: Option<String>,
        dir: Option<PathBuf>,
    },
    Entra {
        http: reqwest::Client,
        token_url: String,
        client_id: String,
        client_secret: String,
        scope_template: String,
        cache: tokio::sync::Mutex<HashMap<String, CachedToken>>,
    },
}

pub struct SyncCredentials {
    mode: Mode,
}

impl SyncCredentials {
    /// `outbound` is the client used for Microsoft endpoints (proxy and CA bundle aware).
    pub fn new(settings: &SyncAuthSettings, outbound: reqwest::Client) -> SyncCredentials {
        let mode = match settings {
            SyncAuthSettings::Static { token, tokens_dir } => {
                Mode::Static { token: token.clone(), dir: tokens_dir.as_ref().map(PathBuf::from) }
            }
            SyncAuthSettings::Entra { tenant_id, client_id, client_secret, scope_template, token_url } => Mode::Entra {
                http: outbound,
                token_url: token_url.replace("{tenant}", tenant_id),
                client_id: client_id.clone(),
                client_secret: client_secret.clone(),
                scope_template: scope_template.clone(),
                cache: tokio::sync::Mutex::new(HashMap::new()),
            },
        };
        SyncCredentials { mode }
    }

    /// Human-readable method, for logs and the admin page.
    pub fn method(&self) -> &'static str {
        match self.mode {
            Mode::Static { .. } => "static",
            Mode::Entra { .. } => "entra",
        }
    }

    /// Warning to log at start-up when the configuration cannot authenticate at all.
    pub fn startup_warning(&self) -> Option<&'static str> {
        match &self.mode {
            Mode::Static { token: None, dir: None } => Some(
                "neither ROUTING_SYNC_TOKEN nor ROUTING_SYNC_TOKENS_DIR is set: the backends will answer 401 to every sync request",
            ),
            _ => None,
        }
    }

    /// Bearer credential for `backend`; `Ok(None)` when none is configured (the request is sent without one).
    pub async fn bearer(&self, backend: &Backend) -> Result<Option<String>, String> {
        match &self.mode {
            Mode::Static { token, dir } => {
                if let Some(dir) = dir {
                    let mut names = vec![backend.prefix_key.as_str()];
                    if !backend.backend_id.is_empty() {
                        names.push(backend.backend_id.as_str());
                    }
                    for name in names {
                        let path = dir.join(name);
                        if let Ok(text) = tokio::fs::read_to_string(&path).await {
                            let t = text.trim();
                            if !t.is_empty() {
                                return Ok(Some(t.to_string()));
                            }
                        }
                    }
                }
                Ok(token.clone())
            }
            Mode::Entra { http, token_url, client_id, client_secret, scope_template, cache } => {
                if backend.backend_id.is_empty() {
                    return Err(format!(
                        "backend {} has no backend_id; it is required with ROUTING_SYNC_AUTH=entra (token audience)",
                        backend.prefix_key
                    ));
                }
                let scope = scope_template.replace("{backend_id}", &backend.backend_id);
                let mut cache = cache.lock().await;
                if let Some(c) = cache.get(&scope) {
                    if c.valid_until > Instant::now() {
                        return Ok(Some(c.token.clone()));
                    }
                }
                let form = [
                    ("grant_type", "client_credentials"),
                    ("client_id", client_id.as_str()),
                    ("client_secret", client_secret.as_str()),
                    ("scope", scope.as_str()),
                ];
                let resp = http
                    .post(token_url)
                    .form(&form)
                    .send()
                    .await
                    .map_err(|e| format!("Entra token request failed: {e}"))?;
                let status = resp.status();
                let body: Value = resp.json().await.unwrap_or(Value::Null);
                if !status.is_success() {
                    let reason = body.get("error_description").or_else(|| body.get("error")).and_then(Value::as_str);
                    return Err(format!("Entra token endpoint answered {status}: {}", reason.unwrap_or("no details")));
                }
                let token = body
                    .get("access_token")
                    .and_then(Value::as_str)
                    .ok_or("Entra token response has no access_token")?
                    .to_string();
                let lifetime = Duration::from_secs(body.get("expires_in").and_then(Value::as_u64).unwrap_or(3600));
                let valid_for = lifetime.saturating_sub(TOKEN_EARLY_REFRESH).max(Duration::from_secs(60));
                cache.insert(scope, CachedToken { token: token.clone(), valid_until: Instant::now() + valid_for });
                Ok(Some(token))
            }
        }
    }

    /// Forgets the cached token of `backend` (after a `401`). Returns whether a retry can help.
    pub async fn invalidate(&self, backend: &Backend) -> bool {
        match &self.mode {
            Mode::Static { .. } => false,
            Mode::Entra { scope_template, cache, .. } => {
                let scope = scope_template.replace("{backend_id}", &backend.backend_id);
                cache.lock().await.remove(&scope).is_some()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(key: &str, id: &str) -> Backend {
        Backend {
            id: 1,
            prefix_key: key.into(),
            name: "B".into(),
            is_active: true,
            computer_sync_version: 0,
            upstream_url: String::new(),
            backend_id: id.into(),
        }
    }

    #[tokio::test]
    async fn static_tokens_per_backend() {
        let dir = std::env::temp_dir().join(format!("router-tokens-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("abc"), "per-prefix\n").unwrap();
        std::fs::write(dir.join("dept_b"), "per-id").unwrap();
        let c = SyncCredentials::new(
            &SyncAuthSettings::Static { token: Some("shared".into()), tokens_dir: Some(dir.display().to_string()) },
            reqwest::Client::new(),
        );
        assert_eq!(c.bearer(&backend("abc", "")).await.unwrap().as_deref(), Some("per-prefix"));
        assert_eq!(c.bearer(&backend("zzz", "dept_b")).await.unwrap().as_deref(), Some("per-id"));
        assert_eq!(c.bearer(&backend("zzz", "")).await.unwrap().as_deref(), Some("shared"));
        assert!(!c.invalidate(&backend("abc", "")).await);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn entra_needs_backend_id() {
        let c = SyncCredentials::new(
            &SyncAuthSettings::Entra {
                tenant_id: "t".into(),
                client_id: "c".into(),
                client_secret: "s".into(),
                scope_template: "api://labmango-{backend_id}/.default".into(),
                token_url: "http://127.0.0.1:9/{tenant}".into(),
            },
            reqwest::Client::new(),
        );
        assert!(c.bearer(&backend("abc", "")).await.unwrap_err().contains("backend_id"));
    }
}
