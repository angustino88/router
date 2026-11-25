//! Azure AD ID-token validation, equivalent to `service._decode_id_token`:
//! RS256, signing key picked by `kid` from `AAD_JWKS_URL`, `aud == AAD_AUDIENCE`, `exp` (and `nbf`) checked.
//!
//! Difference from Python (on purpose, for speed): the Python code builds a new `PyJWKClient` per request and
//! therefore downloads the JWKS on every call. Here keys are cached for `AAD_JWKS_CACHE_TTL`, refreshed
//! immediately when an unknown `kid` shows up (at most once every 30 s), and the last good keys stay in use if
//! Microsoft is unreachable.

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde_json::Value;

/// Message returned to clients for every validation failure (same text as Python's `AADAuthError`).
pub const INVALID_TOKEN: &str = "Microsoft Token is invalid or expired";

const MIN_FORCED_REFRESH: Duration = Duration::from_secs(30);

struct KeySet {
    keys: HashMap<String, DecodingKey>,
    fetched_at: Instant,
}

pub struct Aad {
    http: reqwest::Client,
    jwks_url: String,
    validation: Validation,
    ttl: Duration,
    cache: RwLock<Option<Arc<KeySet>>>,
    /// Serialises refreshes so a burst of requests triggers one download.
    refresh_lock: tokio::sync::Mutex<Option<Instant>>,
}

impl Aad {
    pub fn new(http: reqwest::Client, jwks_url: String, audience: &str, ttl: Duration) -> Aad {
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(&[audience]);
        // PyJWT defaults: no leeway, `exp`/`nbf`/`aud` verified when present, `aud` required because an audience is set.
        validation.leeway = 0;
        validation.validate_exp = true;
        validation.validate_nbf = true;
        validation.set_required_spec_claims(&["aud"]);
        Aad { http, jwks_url, validation, ttl, cache: RwLock::new(None), refresh_lock: tokio::sync::Mutex::new(None) }
    }

    /// Returns the token payload. The error text is for logs only; clients always get [`INVALID_TOKEN`].
    pub async fn decode_id_token(&self, token: &str) -> Result<serde_json::Map<String, Value>> {
        let header = decode_header(token).context("malformed token header")?;
        if header.alg != Algorithm::RS256 {
            bail!("algorithm {:?} not allowed", header.alg);
        }
        let kid = header.kid.context("token header has no kid")?;
        let key = self.key_for(&kid).await?;
        let data = decode::<serde_json::Map<String, Value>>(token, &key, &self.validation).context("token rejected")?;
        Ok(data.claims)
    }

    async fn key_for(&self, kid: &str) -> Result<DecodingKey> {
        let cached = self.cache.read().unwrap().clone();
        if let Some(set) = &cached {
            if set.fetched_at.elapsed() < self.ttl {
                if let Some(k) = set.keys.get(kid) {
                    return Ok(k.clone());
                }
            }
        }

        // Stale cache or unknown kid: refresh (single flight, rate limited).
        let mut last_refresh = self.refresh_lock.lock().await;
        // Another request may have refreshed while we waited.
        let current = self.cache.read().unwrap().clone();
        if let Some(set) = current {
            let refreshed_meanwhile = cached.as_ref().map(|c| !Arc::ptr_eq(c, &set)).unwrap_or(true);
            if refreshed_meanwhile && set.fetched_at.elapsed() < self.ttl {
                if let Some(k) = set.keys.get(kid) {
                    return Ok(k.clone());
                }
            }
        }
        let allowed = (*last_refresh).map(|t| t.elapsed() >= MIN_FORCED_REFRESH).unwrap_or(true)
            || cached.as_ref().map(|c| c.fetched_at.elapsed() >= self.ttl).unwrap_or(true);
        if allowed {
            *last_refresh = Some(Instant::now());
            match self.fetch().await {
                Ok(set) => {
                    let set = Arc::new(set);
                    *self.cache.write().unwrap() = Some(set.clone());
                    return set.keys.get(kid).cloned().with_context(|| format!("no signing key with kid {kid}"));
                }
                Err(e) => tracing::warn!("JWKS refresh failed, using cached keys if any: {e:#}"),
            }
        }
        let current = self.cache.read().unwrap().clone();
        current.and_then(|s| s.keys.get(kid).cloned()).with_context(|| format!("no signing key with kid {kid}"))
    }

    async fn fetch(&self) -> Result<KeySet> {
        let resp = self.http.get(&self.jwks_url).send().await.context("fetch JWKS")?.error_for_status()?;
        let body: Value = resp.json().await.context("JWKS is not JSON")?;
        let keys = parse_jwks(&body)?;
        if keys.is_empty() {
            bail!("JWKS has no usable RSA signing keys");
        }
        tracing::info!(keys = keys.len(), "JWKS loaded");
        Ok(KeySet { keys, fetched_at: Instant::now() })
    }
}

/// RSA keys usable for signatures (`use` absent or `sig`), indexed by `kid` — same filter as PyJWKClient.
pub fn parse_jwks(body: &Value) -> Result<HashMap<String, DecodingKey>> {
    let list = body.get("keys").and_then(Value::as_array).context("JWKS has no keys array")?;
    let mut out = HashMap::new();
    for k in list {
        let s = |f: &str| k.get(f).and_then(Value::as_str);
        if s("kty") != Some("RSA") || !matches!(s("use"), None | Some("sig")) {
            continue;
        }
        let (Some(kid), Some(n), Some(e)) = (s("kid"), s("n"), s("e")) else {
            continue;
        };
        match DecodingKey::from_rsa_components(n, e) {
            Ok(key) => {
                out.insert(kid.to_string(), key);
            }
            Err(err) => tracing::warn!(kid, "skipping JWKS key: {err}"),
        }
    }
    Ok(out)
}

/// Routing key of the signed-in user: `entra:<tid>:<oid>`, both UUIDs lowercase and hyphenated.
/// This is the key the core `users` stream publishes for the user's Entra identity.
pub fn extract_user_key(payload: &serde_json::Map<String, Value>) -> Result<String, &'static str> {
    let claim = |name: &str| match payload.get(name) {
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(s.trim()),
        _ => Err("tid and oid were not found in id_token payload."),
    };
    let parse = |raw: &str| uuid::Uuid::parse_str(raw).map_err(|_| "tid and oid must be UUIDs.");
    let tid = parse(claim("tid")?)?;
    let oid = parse(claim("oid")?)?;
    Ok(format!("entra:{}:{}", tid.hyphenated(), oid.hyphenated()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn payload(v: Value) -> serde_json::Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn user_key_rules() {
        let ok = payload(json!({
            "oid": "7709C6DE-B8BF-4FFA-AF27-F641536C746D",
            "tid": " 0ae51e19-07c8-4e4b-bb6d-648ee58410f4 "
        }));
        assert_eq!(
            extract_user_key(&ok).unwrap(),
            "entra:0ae51e19-07c8-4e4b-bb6d-648ee58410f4:7709c6de-b8bf-4ffa-af27-f641536c746d"
        );
        assert_eq!(
            extract_user_key(&payload(json!({"tid": "0ae51e19-07c8-4e4b-bb6d-648ee58410f4"}))).unwrap_err(),
            "tid and oid were not found in id_token payload."
        );
        assert_eq!(
            extract_user_key(&payload(json!({"tid": "x", "oid": "7709c6de-b8bf-4ffa-af27-f641536c746d"}))).unwrap_err(),
            "tid and oid must be UUIDs."
        );
        assert!(extract_user_key(&payload(json!({"tid": 1, "oid": 2}))).is_err());
    }

    #[test]
    fn jwks_filter() {
        let body = json!({"keys": [
            {"kty": "RSA", "use": "sig", "kid": "a", "n": "AQAB", "e": "AQAB"},
            {"kty": "RSA", "use": "enc", "kid": "b", "n": "AQAB", "e": "AQAB"},
            {"kty": "EC", "kid": "c", "crv": "P-256", "x": "AA", "y": "AA"},
            {"kty": "RSA", "kid": "d", "n": "AQAB", "e": "AQAB", "x5c": ["..."], "issuer": "x"}
        ]});
        let keys = parse_jwks(&body).unwrap();
        let mut kids: Vec<_> = keys.keys().cloned().collect();
        kids.sort();
        assert_eq!(kids, vec!["a", "d"]);
    }
}
