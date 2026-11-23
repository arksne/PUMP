//! Импорт плейлистов через OAuth PKCE с привязкой к сессии Mini App.
use crate::{ApiError, App, catalog, stats};
use axum::{
    Json,
    extract::{Query, State},
    http::HeaderMap,
    response::{Html, IntoResponse},
};
use social_integrations::{
    TrackMetadata,
    apple::AppleMusicClient,
    import::{Candidate, Resolution, TrackResolver, import_tracks},
    spotify::{PkceAuthorization, SpotifyClient},
};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
#[derive(Clone, Default)]
pub(crate) struct Imports {
    pending: Arc<Mutex<HashMap<String, Pending>>>,
    clients: Arc<Mutex<HashMap<String, Arc<Mutex<SpotifyClient>>>>>,
}
struct Pending {
    owner: String,
    auth: PkceAuthorization,
    created: Instant,
}
pub(crate) async fn authorize(
    State(app): State<App>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let owner = stats::owner(&app, &headers).await?;
    let client = crate::setting("SPOTIFY_CLIENT_ID")
        .ok_or_else(|| ApiError::invalid("Администратор должен настроить SPOTIFY_CLIENT_ID."))?;
    let base = app
        .telegram
        .as_ref()
        .map(|s| s.url.clone())
        .ok_or_else(|| ApiError::invalid("Для OAuth требуется публичный адрес приложения."))?;
    let callback = base
        .join("/api/spotify/callback")
        .map_err(|_| ApiError::invalid("Неверный адрес callback."))?;
    let auth = PkceAuthorization::new(client, callback.as_str())
        .map_err(|_| ApiError::invalid("Неверные настройки Spotify."))?;
    let url = auth.authorization_url(&["playlist-read-private", "playlist-read-collaborative"]);
    let state = url
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, v)| v.to_string())
        .ok_or_else(|| ApiError::invalid("Не удалось создать OAuth state."))?;
    let mut pending = app.imports.pending.lock().await;
    pending.retain(|_, p| p.created.elapsed() < Duration::from_secs(600));
    if pending.len() > 1024 {
        return Err(ApiError::invalid("Слишком много ожидающих подключений."));
    }
    pending.insert(
        state,
        Pending {
            owner,
            auth,
            created: Instant::now(),
        },
    );
    Ok(Json(serde_json::json!({"url":url.as_str()})))
}
#[derive(serde::Deserialize)]
pub(crate) struct Callback {
    state: String,
    code: Option<String>,
    error: Option<String>,
}
pub(crate) async fn callback(
    State(app): State<App>,
    Query(query): Query<Callback>,
) -> Result<impl IntoResponse, ApiError> {
    let pending = app
        .imports
        .pending
        .lock()
        .await
        .remove(&query.state)
        .filter(|p| p.created.elapsed() < Duration::from_secs(600))
        .ok_or_else(|| ApiError::invalid("OAuth-сессия истекла."))?;
    let mut callback = app
        .telegram
        .as_ref()
        .ok_or_else(|| ApiError::invalid("Публичный адрес недоступен."))?
        .url
        .join("/api/spotify/callback")
        .map_err(|_| ApiError::invalid("Неверный callback."))?;
    callback
        .query_pairs_mut()
        .append_pair("state", &query.state);
    if let Some(code) = query.code {
        callback.query_pairs_mut().append_pair("code", &code);
    }
    if let Some(error) = query.error {
        callback.query_pairs_mut().append_pair("error", &error);
    }
    let client = pending
        .auth
        .exchange(callback.as_str())
        .await
        .map_err(|_| ApiError::unavailable("Spotify не подтвердил подключение."))?;
    app.imports
        .clients
        .lock()
        .await
        .insert(pending.owner, Arc::new(Mutex::new(client)));
    Ok(Html(
        "<!doctype html><html lang=ru><meta charset=utf-8><title>PUMP</title><p>Spotify подключён. Вернись в PUMP.</p></html>",
    ))
}
#[derive(serde::Deserialize)]
pub(crate) struct ImportRequest {
    source: String,
    link: String,
}
struct Resolver(Arc<catalog::Catalog>);
#[async_trait::async_trait]
impl TrackResolver<String> for Resolver {
    async fn candidates(
        &self,
        track: &TrackMetadata,
    ) -> social_integrations::Result<Vec<Candidate<String>>> {
        let query = format!("{} {}", track.artists.join(" "), track.title);
        let result = self
            .0
            .search(catalog::SearchQuery {
                q: query.chars().take(140).collect(),
                source: catalog::Source::All,
            })
            .await
            .map_err(|_| social_integrations::Error::Protocol("Каталог недоступен".into()))?;
        Ok(result
            .tracks
            .into_iter()
            .map(|t| Candidate {
                id: t.id,
                metadata: TrackMetadata {
                    title: t.title,
                    artists: vec![t.artist],
                    duration_ms: Some(t.duration_ms).filter(|n| *n > 0),
                    ..Default::default()
                },
            })
            .collect())
    }
}
pub(crate) async fn playlist(
    State(app): State<App>,
    headers: HeaderMap,
    Json(request): Json<ImportRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let owner = stats::owner(&app, &headers).await?;
    let items = match request.source.as_str() {
        "apple" => AppleMusicClient::new()
            .map_err(|_| ApiError::unavailable("Клиент Apple недоступен."))?
            .resolve_link(&request.link)
            .await
            .map_err(|_| {
                ApiError::unavailable("Ссылка Apple Music недоступна или требует MusicKit.")
            })?,
        "spotify" => {
            let client = app
                .imports
                .clients
                .lock()
                .await
                .get(&owner)
                .cloned()
                .ok_or_else(|| ApiError::invalid("Сначала подключи Spotify."))?;
            let id = url::Url::parse(&request.link)
                .ok()
                .filter(|u| u.host_str() == Some("open.spotify.com"))
                .and_then(|u| {
                    u.path_segments().and_then(|mut p| {
                        if p.next() == Some("playlist") {
                            p.next().map(str::to_owned)
                        } else {
                            None
                        }
                    })
                })
                .ok_or_else(|| ApiError::invalid("Нужна ссылка Spotify playlist."))?;
            client
                .lock()
                .await
                .playlist_tracks(&id)
                .await
                .map_err(|_| ApiError::unavailable("Плейлист Spotify недоступен."))?
        }
        _ => return Err(ApiError::invalid("Источник импорта неизвестен.")),
    };
    let imported = import_tracks(items, &Resolver(app.catalog.clone())).await;
    let rows = imported
        .into_iter()
        .map(|track| {
            let resolution = match track.resolution {
                Resolution::Matched { id, confidence } => {
                    serde_json::json!({"status":"matched","id":id,"confidence":confidence})
                }
                Resolution::Ambiguous { candidates } => {
                    serde_json::json!({"status":"ambiguous","candidates":candidates})
                }
                _ => serde_json::json!({"status":"unmatched"}),
            };
            serde_json::json!({"source":track.source,"resolution":resolution})
        })
        .collect::<Vec<_>>();
    Ok(Json(rows))
}
