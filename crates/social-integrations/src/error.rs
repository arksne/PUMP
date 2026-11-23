//! Типизированные ошибки ввода, транспорта и хранения.
use std::time::Duration;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("HTTP transport failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("HTTP status {status}")]
    Http {
        status: u16,
        retry_after: Option<Duration>,
    },
    #[error("service error {code}: {message}")]
    Service { code: u32, message: String },
    #[error("invalid service response: {0}")]
    Protocol(String),
    #[error("queue is already locked")]
    QueueLocked,
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON: {0}")]
    Json(#[from] serde_json::Error),
}

pub(crate) async fn checked(response: reqwest::Response) -> Result<reqwest::Response> {
    if response.status().is_success() {
        return Ok(response);
    }
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs);
    // Тело ошибки не включаем: провайдер может отразить секреты запроса.
    Err(Error::Http {
        status: response.status().as_u16(),
        retry_after,
    })
}
