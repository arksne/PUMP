//! Ошибки каталога и транспорта с преобразованием в ответ API.
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;
#[derive(Debug, thiserror::Error)]
#[error("{status}: {message}")]
pub struct ApiError {
    status: StatusCode,
    message: &'static str,
}

impl ApiError {
    pub fn new(status: StatusCode, message: &'static str) -> Self {
        Self { status, message }
    }

    pub fn invalid(message: &'static str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn missing(message: &'static str) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    pub fn unavailable(message: &'static str) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, message)
    }

    pub fn storage(error: impl std::fmt::Display) -> Self {
        tracing::error!(%error, "Ошибка базы PUMP");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Не удалось сохранить данные.",
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({"error": self.message}))).into_response()
    }
}
