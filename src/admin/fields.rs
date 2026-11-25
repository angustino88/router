//! Parsing and validation of admin request fields. Errors use the field-error shape `{"<field>": ["..."]}`.

use axum::{body::Bytes, http::HeaderMap, response::Response};
use serde_json::{Map, Value};

use crate::{
    http::api::{field_error, request_data},
    state::AppState,
};

/// Body of an admin request after the credentials were checked.
pub async fn authorized_body(s: &AppState, headers: &HeaderMap, body: &Bytes) -> Result<Map<String, Value>, Response> {
    let data = request_data(headers, body)?;
    super::auth::authorize(s, headers, Some(&data)).await?;
    Ok(data)
}

pub fn optional_bool(data: &Map<String, Value>, name: &str) -> Result<Option<bool>, Response> {
    match data.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "on" | "yes" => Ok(Some(true)),
            "false" | "0" | "off" | "no" => Ok(Some(false)),
            _ => Err(field_error(name, "Must be a valid boolean.")),
        },
        Some(_) => Err(field_error(name, "Must be a valid boolean.")),
    }
}

pub fn optional_int(data: &Map<String, Value>, name: &str, min: i64, max: i64) -> Result<Option<i64>, Response> {
    let v = match data.get(name) {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
        Some(_) => None,
    };
    let Some(v) = v else { return Err(field_error(name, "A valid integer is required.")) };
    if v < min {
        return Err(field_error(name, format!("Ensure this value is greater than or equal to {min}.")));
    }
    if v > max {
        return Err(field_error(name, format!("Ensure this value is less than or equal to {max}.")));
    }
    Ok(Some(v))
}

pub fn optional_string(data: &Map<String, Value>, name: &str) -> Result<Option<String>, Response> {
    match data.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.trim().to_string())),
        Some(_) => Err(field_error(name, "Not a valid string.")),
    }
}

pub fn backend_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("This field may not be blank.".into());
    }
    if name.chars().count() > 255 {
        return Err("Ensure this field has no more than 255 characters.".into());
    }
    Ok(name.to_string())
}

/// Logical backend id (its `BACKEND_ID`): empty, or letters, digits, `_`, `-` and `.`, at most 100 characters.
pub fn backend_id(raw: &str) -> Result<String, String> {
    let id = raw.trim();
    if id.len() > 100 || !id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')) {
        return Err("Use at most 100 letters, digits, '_', '-' or '.' (the backend's BACKEND_ID).".into());
    }
    Ok(id.to_string())
}

/// Upstream URL for the nginx map: empty (not routed) or `http(s)://host[:port]` with no path, query,
/// credentials or characters that would break the map file. Returned without a trailing slash.
pub fn upstream_url(raw: &str) -> Result<String, String> {
    let url = raw.trim().trim_end_matches('/');
    if url.is_empty() {
        return Ok(String::new());
    }
    if url.len() > 255 {
        return Err("Ensure this field has no more than 255 characters.".into());
    }
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .ok_or("Use http://host:port or https://host:port.")?;
    if rest.is_empty() {
        return Err("The URL has no host.".into());
    }
    if rest.contains(['/', '?', '#', '@']) {
        return Err("Only scheme://host[:port] is allowed (no path, query or credentials).".into());
    }
    if !rest.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']' | '_')) {
        return Err("The host contains characters that are not allowed.".into());
    }
    // `[v6]:port`, `host:port` or bare host.
    let (host, port) = if let Some(after) = rest.strip_prefix('[') {
        let (h, tail) = after.split_once(']').ok_or("Invalid IPv6 address.")?;
        match tail {
            "" => (h, None),
            t => (h, Some(t.strip_prefix(':').ok_or("Invalid IPv6 address.")?)),
        }
    } else {
        match rest.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (rest, None),
        }
    };
    if host.is_empty() || (!rest.starts_with('[') && host.contains(['[', ']'])) {
        return Err("The URL has no valid host.".into());
    }
    if let Some(p) = port {
        match p.parse::<u16>() {
            Ok(n) if n > 0 => {}
            _ => return Err("The port must be a number between 1 and 65535.".into()),
        }
    }
    Ok(url.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert_eq!(upstream_url(" https://Host.Example:8443/ ").unwrap(), "https://host.example:8443");
        assert_eq!(upstream_url("http://host.docker.internal").unwrap(), "http://host.docker.internal");
        assert_eq!(upstream_url("").unwrap(), "");
        assert!(upstream_url("ftp://x").is_err());
        assert!(upstream_url("http://x/api").is_err());
        assert!(upstream_url("http://u:p@x").is_err());
        assert!(upstream_url("http://x:99999").is_err());
        assert!(upstream_url("http://x;y").is_err());
        assert!(upstream_url("http://").is_err());
        assert_eq!(upstream_url("http://[::1]:8000").unwrap(), "http://[::1]:8000");
        assert!(upstream_url("http://a:1:2").is_err());
    }

    #[test]
    fn ids_and_names() {
        assert_eq!(backend_id(" dept_a ").unwrap(), "dept_a");
        assert!(backend_id("a b").is_err());
        assert!(backend_name("   ").is_err());
        assert_eq!(backend_name(" Lab A ").unwrap(), "Lab A");
    }
}
