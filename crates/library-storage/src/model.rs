//! Типы данных и сериализация публичного контракта модуля.
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};
use url::Url;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkMode {
    #[default]
    LocalOnly,
    Online,
}

impl NetworkMode {
    pub(crate) fn require_online(self) -> crate::Result<()> {
        if self == Self::LocalOnly {
            Err(crate::Error::LocalOnly)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lyrics {
    pub plain: Option<String>,
    pub synced: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackMetadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub lyrics: Option<Lyrics>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DownloadRequest {
    pub id: String,
    pub url: Url,
    pub extension: String,
    #[serde(default)]
    pub cover_url: Option<Url>,
    #[serde(default)]
    pub metadata: TrackMetadata,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Pending,
    Downloading,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DownloadJob {
    pub request: DownloadRequest,
    pub status: JobStatus,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DownloadProgress {
    pub id: String,
    pub status: JobStatus,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
}

impl From<&DownloadJob> for DownloadProgress {
    fn from(job: &DownloadJob) -> Self {
        Self {
            id: job.request.id.clone(),
            status: job.status,
            downloaded_bytes: job.downloaded_bytes,
            total_bytes: job.total_bytes,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LibraryTrack {
    pub id: String,
    pub audio_path: PathBuf,
    pub bytes: u64,
    pub metadata: TrackMetadata,
    pub cover: Option<crate::StoredCover>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OfflinePlaylist {
    pub id: String,
    pub title: String,
    pub track_ids: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OfflineBundle {
    pub id: String,
    pub title: String,
    pub tracks: Vec<LibraryTrack>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Catalog {
    pub version: u32,
    pub jobs: BTreeMap<String, DownloadJob>,
    pub tracks: BTreeMap<String, LibraryTrack>,
    pub playlists: BTreeMap<String, OfflinePlaylist>,
}

impl Default for Catalog {
    fn default() -> Self {
        Self {
            version: 1,
            jobs: BTreeMap::new(),
            tracks: BTreeMap::new(),
            playlists: BTreeMap::new(),
        }
    }
}
