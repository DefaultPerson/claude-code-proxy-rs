//! Application error types with Anthropic-format JSON responses.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("Invalid request: {0}")]
    BadRequest(String),

    #[error("Internal error: {0}")]
    #[allow(dead_code)]
    Internal(String),

    #[error("Subprocess error: {0}")]
    Subprocess(String),

    #[error("Upstream error ({0}): {1}")]
    Upstream(u16, String),

    #[error("Token error: {0}")]
    TokenError(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, error_type, message) = match &self {
            AppError::BadRequest(msg) => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                msg.clone(),
            ),
            AppError::Internal(msg) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "api_error", msg.clone())
            }
            AppError::Subprocess(msg) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "api_error", msg.clone())
            }
            AppError::Upstream(status, msg) => {
                let sc = StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY);
                (sc, "api_error", msg.clone())
            }
            AppError::TokenError(msg) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "authentication_error",
                msg.clone(),
            ),
        };

        let body = json!({
            "type": "error",
            "error": {
                "type": error_type,
                "message": message,
            }
        });

        (status, axum::Json(body)).into_response()
    }
}
