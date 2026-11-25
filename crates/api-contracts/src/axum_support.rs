use axum::body::Body;
use axum::http::{header, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::response::ContractResponse;

/// Lets handlers return `Result<ContractResponse, _>` directly.
impl IntoResponse for ContractResponse {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut builder = Response::builder().status(status);
        let body = match &self.body {
            Some(value) => {
                builder = builder.header(header::CONTENT_TYPE, "application/json");
                Body::from(value.to_string())
            }
            None => Body::empty(),
        };
        for (name, value) in &self.headers {
            if let (Ok(name), Ok(value)) = (HeaderName::try_from(name.as_str()), HeaderValue::try_from(value.as_str()))
            {
                builder = builder.header(name, value);
            }
        }
        builder.body(body).unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    }
}
