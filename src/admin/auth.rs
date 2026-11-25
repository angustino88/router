//! Admin authentication: a single account from the environment.

use std::{sync::Arc, time::Duration};

use axum::{
    body::Bytes,
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use base64::Engine;
use serde_json::{json, Map, Value};
use subtle::ConstantTimeEq;

use crate::{http::api::detail, state::AppState};

#[derive(Clone)]
pub struct AdminAccount {
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for AdminAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminAccount").field("username", &self.username).finish_non_exhaustive()
    }
}

const INVALID: &str = "Invalid username or password.";
const DISABLED: &str = "Admin is disabled: set ADMIN_USERNAME and ADMIN_PASSWORD in .env.";

/// `(username, password)` from `Authorization: Basic ...`.
fn basic_credentials(headers: &HeaderMap) -> Option<(String, String)> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, encoded) = value.trim().split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD.decode(encoded.trim()).ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let (u, p) = text.split_once(':')?;
    Some((u.to_string(), p.to_string()))
}

fn body_credentials(body: Option<&Map<String, Value>>) -> Option<(String, String)> {
    let b = body?;
    match (b.get("username"), b.get("password")) {
        (Some(Value::String(u)), Some(Value::String(p))) => Some((u.clone(), p.clone())),
        _ => None,
    }
}

fn equal(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

/// Checks the admin credentials (Basic header first, then the body). Returns the username.
/// No `WWW-Authenticate` header on purpose: the admin page handles 401 itself (no browser dialog).
pub async fn authorize(
    s: &AppState,
    headers: &HeaderMap,
    body: Option<&Map<String, Value>>,
) -> Result<String, Response> {
    let Some(account) = &s.admin else {
        return Err(detail(StatusCode::SERVICE_UNAVAILABLE, DISABLED));
    };
    let Some((user, pass)) = basic_credentials(headers).or_else(|| body_credentials(body)) else {
        return Err(detail(StatusCode::UNAUTHORIZED, INVALID));
    };
    // Evaluate both comparisons: no early exit on the username.
    let ok = equal(&user, &account.username) & equal(&pass, &account.password);
    if !ok {
        tracing::warn!(username = %user, "admin login failed");
        tokio::time::sleep(Duration::from_millis(500)).await;
        return Err(detail(StatusCode::UNAUTHORIZED, INVALID));
    }
    Ok(user)
}

/// `POST /admin/api/login`
pub async fn login(State(s): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let data = match crate::http::api::request_data(&headers, &body) {
        Ok(d) => d,
        Err(r) => return r,
    };
    match authorize(&s, &headers, Some(&data)).await {
        Ok(username) => (StatusCode::OK, Json(json!({"ok": true, "username": username}))).into_response(),
        Err(r) => r,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn basic_header() {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Basic YWRtaW46cDpx"));
        assert_eq!(basic_credentials(&h), Some(("admin".into(), "p:q".into())));
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer x"));
        assert_eq!(basic_credentials(&h), None);
    }
}
