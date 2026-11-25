//! Admin authentication: one account from the environment, sent as HTTP Basic credentials.

use std::{sync::Arc, time::Duration};

use axum::{
    extract::State,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use base64::Engine;
use serde_json::json;
use subtle::ConstantTimeEq;

use crate::{
    http::api::{data, ApiError, ApiResult},
    state::AppState,
};

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

/// `(username, password)` from `Authorization: Basic ...`.
fn basic_credentials(headers: &HeaderMap) -> Option<(String, String)> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, encoded) = value.trim().split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD.decode(encoded.trim()).ok()?;
    let (u, p) = String::from_utf8(decoded).ok()?.split_once(':').map(|(u, p)| (u.to_string(), p.to_string()))?;
    Some((u, p))
}

fn equal(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

/// An authentication failure: `401` with a Basic challenge. The admin page sends `X-Requested-With`, which
/// suppresses the challenge so the browser does not open its own sign-in dialog.
pub struct Unauthorized {
    challenge: bool,
}

impl IntoResponse for Unauthorized {
    fn into_response(self) -> Response {
        let mut r =
            ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized", "Invalid username or password.").into_response();
        if self.challenge {
            r.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Basic realm=\"router-admin\", charset=\"UTF-8\""),
            );
        }
        r
    }
}

/// Error of [`authorize`]: `401` or `503` (admin disabled).
pub enum AuthError {
    Unauthorized(Unauthorized),
    Disabled,
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        match self {
            AuthError::Unauthorized(u) => u.into_response(),
            AuthError::Disabled => ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "admin_disabled",
                "The admin account is not configured (ADMIN_USERNAME, ADMIN_PASSWORD).",
            )
            .into_response(),
        }
    }
}

/// Checks the admin credentials. Returns the username.
pub async fn authorize(s: &AppState, headers: &HeaderMap) -> Result<String, AuthError> {
    let Some(account) = &s.admin else { return Err(AuthError::Disabled) };
    let challenge = !headers.contains_key("x-requested-with");
    let Some((user, pass)) = basic_credentials(headers) else {
        return Err(AuthError::Unauthorized(Unauthorized { challenge }));
    };
    // Evaluate both comparisons: no early exit on the username.
    if !(equal(&user, &account.username) & equal(&pass, &account.password)) {
        tracing::warn!(username = %user, "admin sign-in failed");
        tokio::time::sleep(Duration::from_millis(500)).await;
        return Err(AuthError::Unauthorized(Unauthorized { challenge }));
    }
    Ok(user)
}

/// Runs `handler` for an authorized admin request.
pub async fn guarded<F, Fut>(s: &AppState, headers: &HeaderMap, handler: F) -> Response
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ApiResult>,
{
    match authorize(s, headers).await {
        Ok(_) => handler().await.unwrap_or_else(IntoResponse::into_response),
        Err(e) => e.into_response(),
    }
}

/// `GET /admin/api/session`: checks the credentials and returns the account name.
pub async fn session(State(s): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    match authorize(&s, &headers).await {
        Ok(username) => data(StatusCode::OK, json!({ "username": username })),
        Err(e) => e.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_header() {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Basic YWRtaW46cDpx"));
        assert_eq!(basic_credentials(&h), Some(("admin".into(), "p:q".into())));
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer x"));
        assert_eq!(basic_credentials(&h), None);
    }
}
