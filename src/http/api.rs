//! Response envelope and request parsing, following the backend's API response standard:
//!
//! * success: `{"data": ...}` (plus `"meta"` where documented), `204` without a body for deletes;
//! * error: `{"error": {"code", "message", "details": [{"field", "code", "message"}], "request_id"}}`.
//!
//! `request_id` is filled in by [`super::middleware::envelope_errors`], which also turns errors produced
//! outside the handlers (unknown route, wrong method, body too large, timeout) into the same envelope.

use axum::{
    body::Bytes,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use serde_json::{json, Map, Value};

/// `{"data": value}` with `status`.
pub fn data(status: StatusCode, value: impl Serialize) -> Response {
    (status, Json(json!({ "data": value }))).into_response()
}

/// `{"data": value, "meta": meta}` with `status`.
pub fn data_with_meta(status: StatusCode, value: impl Serialize, meta: Value) -> Response {
    (status, Json(json!({ "data": value, "meta": meta }))).into_response()
}

/// `204 No Content`.
pub fn no_content() -> Response {
    StatusCode::NO_CONTENT.into_response()
}

/// One entry of `error.details`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Detail {
    pub field: String,
    pub code: &'static str,
    pub message: String,
}

/// An error response.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub details: Vec<Detail>,
}

pub type ApiResult = Result<Response, ApiError>;

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> ApiError {
        ApiError { status, code, message: message.into(), details: Vec::new() }
    }

    /// `400 validation_failed` for one field.
    pub fn field(field: &str, code: &'static str, message: impl Into<String>) -> ApiError {
        ApiError {
            status: StatusCode::BAD_REQUEST,
            code: "validation_failed",
            message: "One or more fields are invalid.".into(),
            details: vec![Detail { field: field.into(), code, message: message.into() }],
        }
    }

    pub fn not_found() -> ApiError {
        ApiError::new(StatusCode::NOT_FOUND, "not_found", "Not found.")
    }

    /// `500`; the cause goes to the log only.
    pub fn internal(e: impl std::fmt::Display) -> ApiError {
        tracing::error!("internal error: {e}");
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", "An internal error occurred.")
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        ApiError::internal(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({"error": {
            "code": self.code,
            "message": self.message,
            "details": self.details,
            "request_id": "",
        }});
        (self.status, Json(body)).into_response()
    }
}

/// Default error `code` for a status produced outside the handlers.
pub fn code_for(status: StatusCode) -> &'static str {
    match status.as_u16() {
        400 => "bad_request",
        401 => "unauthorized",
        403 => "forbidden",
        404 => "not_found",
        405 => "method_not_allowed",
        408 => "request_timeout",
        409 => "conflict",
        413 => "payload_too_large",
        415 => "unsupported_media_type",
        429 => "rate_limited",
        503 => "unavailable",
        s if s >= 500 => "internal_error",
        _ => "bad_request",
    }
}

/// Request body as a JSON object. The body must be `application/json`; an empty body is an empty object.
pub fn json_body(headers: &HeaderMap, body: &Bytes) -> Result<Map<String, Value>, ApiError> {
    if body.is_empty() {
        return Ok(Map::new());
    }
    let content_type = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    let mime = content_type.split(';').next().unwrap_or("").trim();
    if !mime.eq_ignore_ascii_case("application/json") {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "Send the body as application/json.",
        ));
    }
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err(ApiError::new(StatusCode::BAD_REQUEST, "invalid_json", "The body must be a JSON object.")),
        Err(e) => {
            Err(ApiError::new(StatusCode::BAD_REQUEST, "invalid_json", format!("The body is not valid JSON: {e}")))
        }
    }
}

/// Typed access to the fields of a request object. Unknown fields are rejected by [`Fields::finish`].
pub struct Fields<'a> {
    map: &'a Map<String, Value>,
    seen: Vec<&'a str>,
}

impl<'a> Fields<'a> {
    pub fn new(map: &'a Map<String, Value>) -> Fields<'a> {
        Fields { map, seen: Vec::new() }
    }

    fn get(&mut self, name: &'a str) -> Option<&'a Value> {
        self.seen.push(name);
        match self.map.get(name) {
            None | Some(Value::Null) => None,
            Some(v) => Some(v),
        }
    }

    /// Optional string, trimmed.
    pub fn string(&mut self, name: &'a str) -> Result<Option<String>, ApiError> {
        match self.get(name) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.trim().to_string())),
            Some(_) => Err(ApiError::field(name, "invalid_type", "Must be a string.")),
        }
    }

    /// Required, non-blank string, trimmed.
    pub fn required_string(&mut self, name: &'a str) -> Result<String, ApiError> {
        match self.string(name)? {
            None => Err(ApiError::field(name, "required", "This field is required.")),
            Some(s) if s.is_empty() => Err(ApiError::field(name, "blank", "This field may not be blank.")),
            Some(s) => Ok(s),
        }
    }

    pub fn boolean(&mut self, name: &'a str) -> Result<Option<bool>, ApiError> {
        match self.get(name) {
            None => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(_) => Err(ApiError::field(name, "invalid_type", "Must be true or false.")),
        }
    }

    pub fn integer(&mut self, name: &'a str, min: i64, max: i64) -> Result<Option<i64>, ApiError> {
        match self.get(name) {
            None => Ok(None),
            Some(Value::Number(n)) => match n.as_i64() {
                Some(v) if (min..=max).contains(&v) => Ok(Some(v)),
                Some(_) => Err(ApiError::field(name, "out_of_range", format!("Must be between {min} and {max}."))),
                None => Err(ApiError::field(name, "invalid_type", "Must be an integer.")),
            },
            Some(_) => Err(ApiError::field(name, "invalid_type", "Must be an integer.")),
        }
    }

    /// Fails on any field that was not read.
    pub fn finish(self) -> Result<(), ApiError> {
        match self.map.keys().find(|k| !self.seen.contains(&k.as_str())) {
            Some(k) => Err(ApiError::field(k, "unknown_field", "Unknown field.")),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn json_headers() -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json; charset=utf-8"));
        h
    }

    #[test]
    fn parsing_and_fields() {
        let m = json_body(&json_headers(), &Bytes::from_static(br#"{"a": " x ", "n": 7, "b": true}"#)).unwrap();
        let mut f = Fields::new(&m);
        assert_eq!(f.required_string("a").unwrap(), "x");
        assert_eq!(f.integer("n", 0, 10).unwrap(), Some(7));
        assert_eq!(f.boolean("b").unwrap(), Some(true));
        assert!(f.finish().is_ok());

        let mut f = Fields::new(&m);
        f.string("a").unwrap();
        let e = f.finish().unwrap_err();
        assert_eq!((e.code, e.details[0].code), ("validation_failed", "unknown_field"));

        let e = Fields::new(&m).required_string("missing").unwrap_err();
        assert_eq!(
            e.details[0],
            Detail { field: "missing".into(), code: "required", message: "This field is required.".into() }
        );
        assert_eq!(Fields::new(&m).integer("n", 8, 9).unwrap_err().details[0].code, "out_of_range");

        assert_eq!(json_body(&json_headers(), &Bytes::from_static(b"[1]")).unwrap_err().code, "invalid_json");
        assert_eq!(
            json_body(&HeaderMap::new(), &Bytes::from_static(b"a=1")).unwrap_err().status,
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
        assert!(json_body(&HeaderMap::new(), &Bytes::new()).unwrap().is_empty());
    }
}
