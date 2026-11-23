//! Типизированные ошибки ввода, транспорта и хранения.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("ошибка файловой системы: {0}")]
    Io(#[from] std::io::Error),
    #[error("ошибка JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("ошибка HTTP: {0}")]
    Http(#[from] reqwest::Error),
    #[error("некорректные данные: {0}")]
    Invalid(String),
    #[error("сетевой доступ запрещён локальным режимом")]
    LocalOnly,
    #[error("операция отменена")]
    Cancelled,
    #[error("хранилище или очередь уже заняты")]
    Busy,
    #[error("объект не найден: {0}")]
    NotFound(String),
    #[error("объект уже существует: {0}")]
    AlreadyExists(String),
    #[error("превышен лимит: {0}")]
    Limit(String),
    #[error("ошибка тегов: {0}")]
    Tags(#[from] lofty::error::LoftyError),
    #[error("ошибка изображения: {0}")]
    Image(#[from] image::ImageError),
    #[error("ошибка фоновой задачи: {0}")]
    Join(#[from] tokio::task::JoinError),
}

pub type Result<T> = std::result::Result<T, Error>;
