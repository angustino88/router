//! Request id and error envelope middleware.

use axum::{
    body::to_bytes,
    extract::Request,
    http::{header, HeaderValue},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

use super::api::code_for;

pub const REQUEST_ID: &str = "x-request-id";
/// Error bodies larger than this are replaced by a generic envelope instead of being parsed.
const MAX_ERROR_BODY: usize = 64 * 1024;

/// `true` for 32 lowercase hex characters (the backend's request id format).
fn valid_request_id(v: &str) -> bool {
    v.len() == 32 && v.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Honours a valid inbound `X-Request-ID`, otherwise generates one; sets it on the request (for the handlers
/// and the access log) and on the response.
pub async fn request_id(mut req: Request, next: Next) -> Response {
    let id = req
        .headers()
        .get(REQUEST_ID)
        .and_then(|v| v.to_str().ok())
        .filter(|v| valid_request_id(v))
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
    let value = HeaderValue::from_str(&id).expect("hex is a valid header value");
    req.headers_mut().insert(REQUEST_ID, value.clone());
    let mut resp = next.run(req).await;
    resp.headers_mut().insert(REQUEST_ID, value);
    resp
}

/// Every 4xx/5xx response gets the error envelope with the request id: handler errors are completed, and errors
/// produced by the framework (plain-text 404/405/408/413…) are converted.
pub async fn envelope_errors(req: Request, next: Next) -> Response {
    let id = req.headers().get(REQUEST_ID).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let resp = next.run(req).await;
    let status = resp.status();
    if !(status.is_client_error() || status.is_server_error()) {
        return resp;
    }
    let (mut parts, body) = resp.into_parts();
    let is_json = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    let parsed = if is_json { to_bytes(body, MAX_ERROR_BODY).await.ok() } else { None };
    let mut envelope = parsed
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .filter(|v| v.get("error").is_some_and(Value::is_object))
        .unwrap_or_else(|| {
            json!({"error": {
                "code": code_for(status),
                "message": status.canonical_reason().unwrap_or("Error"),
                "details": [],
            }})
        });
    envelope["error"]["request_id"] = json!(id);
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    let mut out = (status, Json(envelope)).into_response();
    for (name, value) in parts.headers.iter() {
        if name != header::CONTENT_LENGTH {
            out.headers_mut().insert(name.clone(), value.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ids() {
        assert!(valid_request_id("0123456789abcdef0123456789abcdef"));
        assert!(!valid_request_id("0123456789ABCDEF0123456789abcdef"));
        assert!(!valid_request_id("abc"));
    }
}
