//! Типы данных и сериализация публичного контракта модуля.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackMetadata {
    pub title: String,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub duration_ms: Option<u64>,
    pub isrc: Option<String>,
    pub source_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportItem {
    pub source_id: Option<String>,
    pub metadata: Option<TrackMetadata>,
    /// Отсутствующие и недоступные записи сохраняют позицию в плейлисте.
    pub unavailable_reason: Option<String>,
}
