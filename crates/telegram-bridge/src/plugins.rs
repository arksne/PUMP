//! Администратор добавляет WASM-файлы в PUMP_PLUGIN_DIR до запуска сервера.
use crate::{ApiError, App, plugin_media};
use axum::{
    Json,
    extract::{Path, Query, State},
    response::IntoResponse,
};
use core_streaming::{Kind, ProviderRef, Track};
use plugin_spine::{
    PluginError, PluginRegistry, ResolveRequest, RuntimeLimits, SearchRequest, WasmRuntime,
};
use sha2::{Digest, Sha256};
use std::{path::Path as FsPath, sync::Arc};

pub(crate) const SOURCE: &str = "plugin";

// Длина ID укладывается в ограничение каталога; части хеша имеют однозначные границы.
pub(crate) fn track_id(provider: &str, provider_track: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"pump-addon-track-v1\0");
    for part in [provider, provider_track] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    format!("addon-{}", &hex::encode(hash.finalize())[..56])
}

#[derive(serde::Serialize)]
struct ProviderTrack {
    #[serde(flatten)]
    track: Track,
    provider_id: String,
    provider_track_id: String,
}

fn provider_error(error: PluginError) -> ApiError {
    match error {
        PluginError::ProviderNotFound(_) => ApiError::missing("Провайдер не найден."),
        PluginError::Request(_) => ApiError::invalid("Некорректный запрос провайдера."),
        _ => ApiError::unavailable("Провайдер не смог обработать запрос."),
    }
}
pub(crate) fn load(directory: Option<std::path::PathBuf>) -> anyhow::Result<PluginRegistry> {
    let mut registry = PluginRegistry::new();
    let Some(directory) = directory else {
        return Ok(registry);
    };
    let runtime = WasmRuntime::new(RuntimeLimits::default())?;
    let mut paths = std::fs::read_dir(directory)?.collect::<std::io::Result<Vec<_>>>()?;
    paths.sort_by_key(|entry| entry.file_name());
    for entry in paths {
        if entry.file_type()?.is_file()
            && entry.path().extension().and_then(|s| s.to_str()) == Some("wasm")
        {
            let id = FsPath::new(&entry.file_name())
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_owned();
            let provider = runtime.load_file(id, entry.path())?;
            registry.register(Arc::new(provider), 0)?;
        }
    }
    Ok(registry)
}
pub(crate) async fn list(State(app): State<App>) -> impl IntoResponse {
    Json(
        app.plugins
            .provider_ids()
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>(),
    )
}
#[derive(serde::Deserialize)]
pub(crate) struct Search {
    q: String,
}
pub(crate) async fn search(
    State(app): State<App>,
    Path(id): Path<String>,
    Query(query): Query<Search>,
) -> Result<impl IntoResponse, ApiError> {
    let registry = app.plugins.clone();
    let request = SearchRequest::new(query.q, 20);
    request.validate().map_err(provider_error)?;
    let response = tokio::task::spawn_blocking(move || registry.search(&request, Some(&id)))
        .await
        .map_err(|_| ApiError::unavailable("Провайдер завершился с ошибкой."))?
        .map_err(provider_error)?;
    let mut tracks = Vec::with_capacity(response.response.tracks.len());
    for item in response.response.tracks {
        let id = track_id(&response.provider_id, &item.id);
        // Повторный поиск не стирает уже разрешённый адрес воспроизведения.
        let audio_url = app
            .catalog
            .track(&id)
            .await
            .filter(|track| track.source == SOURCE)
            .and_then(|track| track.audio_url);
        let track = Track {
            id,
            title: item.title,
            artist: item.artist,
            kind: match item.kind {
                plugin_spine::TrackKind::Full => Kind::Full,
                plugin_spine::TrackKind::Preview => Kind::Preview,
            },
            duration_ms: item.duration_ms.unwrap_or(0).min(crate::MAX_TRACK_MS),
            audio_url,
            artwork: item.artwork.map(|url| url.to_string()),
            video_id: None,
            source: SOURCE,
            attribution: None,
            provider: Some(ProviderRef {
                provider_id: response.provider_id.clone(),
                track_id: item.id.clone(),
            }),
        };
        app.catalog.register_track(track.clone()).await?;
        tracks.push(ProviderTrack {
            track,
            provider_id: response.provider_id.clone(),
            provider_track_id: item.id,
        });
    }
    Ok(Json(serde_json::json!({
        "abi_version": response.response.abi_version,
        "provider_id": response.provider_id,
        "tracks": tracks,
        "notice": null,
    })))
}
pub(crate) async fn resolve(
    State(app): State<App>,
    Path(id): Path<String>,
    Json(request): Json<ResolveRequest>,
) -> Result<impl IntoResponse, ApiError> {
    request.validate().map_err(provider_error)?;
    app.plugin_media.ensure_configured()?;
    let provider_track_id = request.track_id.clone();
    let synthetic_id = track_id(&id, &provider_track_id);
    let mut track = app
        .catalog
        .track(&synthetic_id)
        .await
        .filter(|track| track.source == SOURCE)
        .ok_or_else(|| ApiError::missing("Сначала найдите трек через этого провайдера."))?;
    let registry = app.plugins.clone();
    let provider_id = id.clone();
    let response = tokio::task::spawn_blocking(move || registry.resolve(&provider_id, &request))
        .await
        .map_err(|_| ApiError::unavailable("Провайдер завершился с ошибкой."))?
        .map_err(provider_error)?;
    let candidates = app.plugin_media.allowed_candidates(
        std::iter::once(response.primary)
            .chain(response.fallbacks)
            .collect(),
    )?;
    let lease = app
        .plugin_media
        .remember(&synthetic_id, &id, &provider_track_id, candidates.clone())
        .await;
    let urls: Vec<_> = (0..candidates.len())
        .map(|index| plugin_media::stream_url(&synthetic_id, &lease, index))
        .collect();
    track.audio_url = Some(urls[0].clone());
    app.catalog.register_track(track.clone()).await?;
    // Совместимые поля кандидатов тоже содержат только адреса нашего прокси.
    let candidates: Vec<_> = candidates
        .iter()
        .zip(&urls)
        .map(|(candidate, url)| {
            serde_json::json!({
                "url": url,
                "quality": candidate.quality,
                "mime_type": candidate.mime_type,
                "bitrate_kbps": candidate.bitrate_kbps,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({
        "track": ProviderTrack { track, provider_id: id, provider_track_id },
        "urls": urls,
        "primary": candidates[0],
        "fallbacks": &candidates[1..],
    })))
}

#[cfg(test)]
mod tests {
    use super::track_id;

    #[test]
    fn catalogue_ids_are_stable_and_provider_scoped() {
        let id = track_id("provider", "track");
        assert_eq!(id, track_id("provider", "track"));
        assert!(id.len() <= 64);
        assert!(
            id.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        );
        assert_ne!(id, track_id("other", "track"));
        assert_ne!(track_id("ab", "c"), track_id("a", "bc"));
    }
}
