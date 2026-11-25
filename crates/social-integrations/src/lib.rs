//! Независимые клиенты музыкальных сервисов и локальное сопоставление треков.
//! Сетевые операции выполняются только при явном вызове методов клиента.

pub mod apple;
#[cfg(feature = "discord")]
pub mod discord;
mod error;
pub mod fingerprint;
pub mod import;
pub mod lastfm;
pub mod model;
pub mod queue;
pub mod spotify;
pub mod spotify_app;

pub use error::{Error, Result};
pub use model::{ImportItem, TrackMetadata};

/// Клиент с ограничением времени запроса и без перенаправлений с секретами.
pub fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

pub mod spotify_public;
