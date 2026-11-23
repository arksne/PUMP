//! Загрузки из проверенного каталога разделены по сессиям пользователей.
use crate::{ApiError, App, stats};
use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
};
use library_storage::{
    CancellationToken, DownloadManager, DownloadRequest, NetworkMode, StorageConfig, TrackMetadata,
};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tokio::sync::Mutex;
#[derive(Clone)]
pub(crate) struct Libraries {
    root: PathBuf,
    base: url::Url,
    managers: Arc<Mutex<HashMap<String, DownloadManager>>>,
}
impl Libraries {
    pub fn new(root: PathBuf, base: url::Url) -> Self {
        Self {
            root,
            base,
            managers: Arc::default(),
        }
    }
    async fn user(&self, owner: &str) -> Result<DownloadManager, ApiError> {
        let mut managers = self.managers.lock().await;
        if let Some(manager) = managers.get(owner) {
            return Ok(manager.clone());
        }
        let key = hex::encode(Sha256::digest(owner.as_bytes()));
        let manager = DownloadManager::open(StorageConfig {
            root: self.root.join(key),
            concurrency: 2,
            max_bytes: 150 * 1024 * 1024,
            mode: NetworkMode::Online,
        })
        .await
        .map_err(|_| ApiError::unavailable("Не удалось открыть загрузки."))?;
        managers.insert(owner.into(), manager.clone());
        Ok(manager)
    }
}
pub(crate) async fn list(
    State(app): State<App>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let owner = stats::owner(&app, &headers).await?;
    let manager = app.library.user(&owner).await?;
    Ok(Json(
        serde_json::json!({"jobs":manager.jobs().await,"tracks":manager.tracks().await.iter().map(|track|serde_json::json!({"id":track.id,"metadata":track.metadata,"bytes":track.bytes})).collect::<Vec<_>>()}),
    ))
}
pub(crate) async fn download(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let owner = stats::owner(&app, &headers).await?;
    let track = app
        .catalog
        .track(&id)
        .await
        .ok_or_else(|| ApiError::missing("Трек не найден."))?;
    let audio = track
        .audio_url
        .ok_or_else(|| ApiError::invalid("Для видео доступна ссылка источника."))?;
    let url = app
        .library
        .base
        .join(&audio)
        .map_err(|_| ApiError::invalid("Некорректный источник."))?;
    let extension = if id.starts_with("mp3-") || id.starts_with("soundhelix-") {
        "mp3"
    } else if audio.contains(".m4a") {
        "m4a"
    } else {
        return Err(ApiError::invalid(
            "Скачивание этого формата ещё не подключено.",
        ));
    };
    let lyrics = app
        .lyrics
        .fetch(
            &track.artist,
            &track.title,
            Some(track.duration_ms).filter(|n| *n > 0),
        )
        .await
        .ok();
    let cover_url = track
        .artwork
        .as_deref()
        .and_then(|url| app.library.base.join(url).ok());
    let manager = app.library.user(&owner).await?;
    manager
        .enqueue(DownloadRequest {
            id: id.clone(),
            url,
            extension: extension.into(),
            cover_url,
            metadata: TrackMetadata {
                title: Some(track.title),
                artist: Some(track.artist),
                album: None,
                lyrics: lyrics.map(|text| library_storage::Lyrics {
                    plain: text.plain,
                    synced: text.synced,
                }),
            },
        })
        .await
        .map_err(|_| ApiError::invalid("Загрузка уже добавлена или источник недоступен."))?;
    tokio::spawn(async move {
        let _ = manager.run(&id, CancellationToken::new()).await;
    });
    Ok(axum::http::StatusCode::ACCEPTED)
}
pub(crate) async fn file(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    request: axum::extract::Request,
) -> Result<Response, ApiError> {
    use tower::ServiceExt;
    let owner = stats::owner(&app, &headers).await?;
    let manager = app.library.user(&owner).await?;
    let track = manager
        .tracks()
        .await
        .into_iter()
        .find(|track| track.id == id)
        .ok_or_else(|| ApiError::missing("Загрузка не найдена."))?;
    match tower_http::services::ServeFile::new(track.audio_path)
        .oneshot(request)
        .await
    {
        Ok(response) => Ok(response.into_response()),
        Err(e) => match e {},
    }
}
