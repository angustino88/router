use std::fmt;
use std::sync::Arc;

use serde_json::Value;

/// Turns an application error code into the wire value (default: snake_case).
pub type CodeResolver = Arc<dyn Fn(&str) -> String + Send + Sync>;
/// Builds the human readable problem message from `(detail, code, status)`.
pub type ProblemResolver = Arc<dyn Fn(Option<&Value>, Option<&str>, u16) -> String + Send + Sync>;

/// `NotFound`, `not-found`, `NOT FOUND` -> `not_found`.
pub fn default_code_value(code: &str) -> String {
    let chars: Vec<char> = code.chars().collect();
    // Step 1: split camelCase.
    let mut spaced = String::with_capacity(code.len() + 4);
    for (i, &c) in chars.iter().enumerate() {
        if i > 0 && c.is_ascii_uppercase() && (chars[i - 1].is_ascii_lowercase() || chars[i - 1].is_ascii_digit()) {
            spaced.push('_');
        }
        spaced.push(c);
    }
    // Step 2: runs of non [A-Za-z0-9] -> "_", strip "_" at both ends, lowercase.
    let mut out = String::with_capacity(spaced.len());
    let mut pending_sep = false;
    for c in spaced.chars() {
        if c.is_ascii_alphanumeric() {
            if pending_sep && !out.is_empty() {
                out.push('_');
            }
            pending_sep = false;
            out.push(c.to_ascii_lowercase());
        } else {
            pending_sep = true;
        }
    }
    out
}

/// Truthiness of JSON values (`null`, `false`, `0`, `""`, `[]`, `{}` are falsy).
pub(crate) fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_none_or(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Text of a value: raw text for strings, JSON text otherwise.
pub(crate) fn value_to_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

pub fn default_problem_detail(detail: Option<&Value>, code: Option<&str>, status_code: u16) -> String {
    if let Some(d) = detail.filter(|d| is_truthy(d)) {
        if let Value::Object(map) = d {
            for key in ["message", "error", "detail"] {
                if let Some(v) = map.get(key).filter(|v| is_truthy(v)) {
                    return value_to_text(v);
                }
            }
        }
        return value_to_text(d);
    }
    if let Some(code) = code.filter(|c| !c.is_empty()) {
        let value = default_code_value(code);
        return match value.as_str() {
            "not_found" => "The requested resource was not found.".to_string(),
            "validation_failed" => "One or more fields are invalid.".to_string(),
            "invalid_query" => "One or more query parameters are invalid.".to_string(),
            _ => value,
        };
    }
    http_phrase(status_code).to_string()
}

/// Reason phrase of an HTTP status code.
/// Unknown codes fall back to "Unknown Status" instead of raising.
pub(crate) fn http_phrase(code: u16) -> &'static str {
    match code {
        100 => "Continue",
        101 => "Switching Protocols",
        102 => "Processing",
        103 => "Early Hints",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        203 => "Non-Authoritative Information",
        204 => "No Content",
        205 => "Reset Content",
        206 => "Partial Content",
        207 => "Multi-Status",
        208 => "Already Reported",
        226 => "IM Used",
        300 => "Multiple Choices",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        305 => "Use Proxy",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        407 => "Proxy Authentication Required",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        411 => "Length Required",
        412 => "Precondition Failed",
        413 => "Content Too Large",
        414 => "URI Too Long",
        415 => "Unsupported Media Type",
        416 => "Range Not Satisfiable",
        417 => "Expectation Failed",
        418 => "I'm a Teapot",
        421 => "Misdirected Request",
        422 => "Unprocessable Content",
        423 => "Locked",
        424 => "Failed Dependency",
        425 => "Too Early",
        426 => "Upgrade Required",
        428 => "Precondition Required",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        451 => "Unavailable For Legal Reasons",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        506 => "Variant Also Negotiates",
        507 => "Insufficient Storage",
        508 => "Loop Detected",
        510 => "Not Extended",
        511 => "Network Authentication Required",
        _ => "Unknown Status",
    }
}

/// Wire conventions of an application. Cheap to clone (resolvers are `Arc`).
#[derive(Clone)]
pub struct ContractSettings {
    pub success_key: String,
    pub pagination_meta_key: String,
    pub code_resolver: CodeResolver,
    pub problem_detail_resolver: ProblemResolver,
}

impl Default for ContractSettings {
    fn default() -> Self {
        Self {
            success_key: "data".to_string(),
            pagination_meta_key: "meta".to_string(),
            code_resolver: Arc::new(default_code_value),
            problem_detail_resolver: Arc::new(default_problem_detail),
        }
    }
}

impl fmt::Debug for ContractSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ContractSettings")
            .field("success_key", &self.success_key)
            .field("pagination_meta_key", &self.pagination_meta_key)
            .finish_non_exhaustive()
    }
}
