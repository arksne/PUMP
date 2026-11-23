//! Независимая медиатека: очередь загрузок, локальные наборы, теги и доступ к NAS.
//! Корень хранилища принадлежит приложению; идентификаторы пользователей задаёт сервер.

mod covers;
mod disk;
mod downloads;
mod error;
mod model;
pub mod nas;
pub mod scan;
pub mod sessions;
pub mod tags;

pub use covers::{CoverLimits, CoverStore, StoredCover};
pub use downloads::{DownloadManager, StorageConfig};
pub use error::{Error, Result};
pub use model::*;
pub use tokio_util::sync::CancellationToken;
pub use url::Url;

#[cfg(feature = "smb")]
pub mod smb_storage;
