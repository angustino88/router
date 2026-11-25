//! Request parsing and error bodies. The shapes are those of the original service, so existing clients of the
//! old service keep working: `{"detail": "..."}` and `{"<field>": ["..."]}`.

use axum::{
    body::Bytes,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Map, Value};

/// `{"detail": message}` with `status`.
pub fn detail(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "detail": message.into() }))).into_response()
}

/// `400 {"<field>": [message]}`.
pub fn field_error(field: &str, message: impl Into<String>) -> Response {
    let mut m = Map::new();
    m.insert(field.to_string(), json!([message.into()]));
    (StatusCode::BAD_REQUEST, Json(Value::Object(m))).into_response()
}

/// 500 with a generic text; the cause goes to the log only.
pub fn server_error(e: impl std::fmt::Display) -> Response {
    tracing::error!("internal error: {e}");
    detail(StatusCode::INTERNAL_SERVER_ERROR, "A server error occurred.")
}

/// Request body as an object: JSON (`application/json`) or a form (`application/x-www-form-urlencoded`).
/// An empty body is an empty object. Other content types give 415, malformed JSON gives 400.
pub fn request_data(headers: &HeaderMap, body: &Bytes) -> Result<Map<String, Value>, Response> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(Map::new());
    }
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match content_type.as_str() {
        "application/json" | "" => match serde_json::from_slice::<Value>(body) {
            Ok(Value::Object(m)) => Ok(m),
            Ok(_) => Err(detail(StatusCode::BAD_REQUEST, "Invalid data. Expected a dictionary.")),
            Err(e) => Err(detail(StatusCode::BAD_REQUEST, format!("JSON parse error - {e}"))),
        },
        "application/x-www-form-urlencoded" => {
            let mut m = Map::new();
            for (k, v) in form_urlencoded_pairs(body) {
                m.insert(k, Value::String(v));
            }
            Ok(m)
        }
        other => {
            Err(detail(StatusCode::UNSUPPORTED_MEDIA_TYPE, format!("Unsupported media type \"{other}\" in request.")))
        }
    }
}

/// Minimal `application/x-www-form-urlencoded` decoder (last value of a repeated key wins).
fn form_urlencoded_pairs(body: &[u8]) -> Vec<(String, String)> {
    fn decode(s: &str) -> String {
        let bytes = s.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'+' => out.push(b' '),
                b'%' if i + 2 < bytes.len() => {
                    match u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("zz"), 16) {
                        Ok(b) => {
                            out.push(b);
                            i += 2;
                        }
                        Err(_) => out.push(b'%'),
                    }
                }
                b => out.push(b),
            }
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }
    String::from_utf8_lossy(body)
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (decode(k), decode(v)),
            None => (decode(p), String::new()),
        })
        .collect()
}

/// Required string field (trimmed): missing/null, blank and non-scalar values are field errors.
pub fn char_field(data: &Map<String, Value>, name: &str) -> Result<String, Response> {
    let raw = match data.get(name) {
        None => return Err(field_error(name, "This field is required.")),
        Some(Value::Null) => return Err(field_error(name, "This field may not be null.")),
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(_) => return Err(field_error(name, "Not a valid string.")),
    };
    if raw.is_empty() {
        return Err(field_error(name, "This field may not be blank."));
    }
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::to_bytes, http::HeaderValue};

    async fn body(r: Response) -> (StatusCode, Value) {
        let status = r.status();
        (status, serde_json::from_slice(&to_bytes(r.into_body(), usize::MAX).await.unwrap()).unwrap())
    }

    fn json_headers() -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
        h
    }

    #[tokio::test]
    async fn parsing() {
        let d = request_data(&json_headers(), &Bytes::from_static(b"{\"a\": \" x \"}")).unwrap();
        assert_eq!(char_field(&d, "a").unwrap(), "x");
        let (s, b) = body(char_field(&d, "b").unwrap_err()).await;
        assert_eq!((s, b), (StatusCode::BAD_REQUEST, json!({"b": ["This field is required."]})));
        let (s, _) = body(request_data(&json_headers(), &Bytes::from_static(b"{bad")).unwrap_err()).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);

        let mut form = HeaderMap::new();
        form.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/x-www-form-urlencoded; charset=utf-8"));
        let d = request_data(&form, &Bytes::from_static(b"agent_id=a%2Fb+c&x=")).unwrap();
        assert_eq!(d["agent_id"], "a/b c");

        let mut xml = HeaderMap::new();
        xml.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/xml"));
        let (s, _) = body(request_data(&xml, &Bytes::from_static(b"<a/>")).unwrap_err()).await;
        assert_eq!(s, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert!(request_data(&xml, &Bytes::new()).unwrap().is_empty());
    }
}
