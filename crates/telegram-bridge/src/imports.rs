//! Импорт плейлистов через OAuth PKCE с привязкой к сессии Mini App.
use crate::{ApiError, App, catalog, stats};
use axum::{
    Json,
    extract::{Path, Query, State},
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
    jobs: Arc<Mutex<HashMap<String, ImportJob>>>,
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
        let local = self
            .0
            .search(catalog::SearchQuery {
                q: query.chars().take(140).collect(),
                source: catalog::Source::Full,
            })
            .await
            .map_err(|_| social_integrations::Error::Protocol("Каталог недоступен".into()))?;
        let result = if local
            .tracks
            .iter()
            .any(|candidate| candidate.title.to_lowercase() == track.title.to_lowercase())
        {
            local
        } else {
            self.0
                .search(catalog::SearchQuery {
                    q: query.chars().take(140).collect(),
                    source: catalog::Source::All,
                })
                .await
                .map_err(|_| social_integrations::Error::Protocol("Каталог недоступен".into()))?
        };
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

struct ImportJob {
    owner: String,
    created: Instant,
    total: usize,
    rows: Vec<serde_json::Value>,
    running: bool,
    notice: Option<String>,
}
pub(crate) async fn playlist(
    State(app): State<App>,
    headers: HeaderMap,
    Json(request): Json<ImportRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let owner = stats::owner(&app, &headers).await?;
    let (items, notice) = match request.source.as_str() {
        "apple" => (
            AppleMusicClient::new()
                .map_err(|_| ApiError::unavailable("Клиент Apple недоступен."))?
                .resolve_link(&request.link)
                .await
                .map_err(|_| {
                    ApiError::unavailable("Ссылка Apple Music недоступна или требует MusicKit.")
                })?,
            None,
        ),
        "spotify" => {
            let id = social_integrations::spotify_public::playlist_id(&request.link)
                .map_err(|_| ApiError::invalid("Нужна ссылка Spotify playlist."))?;
            let client = app.imports.clients.lock().await.get(&owner).cloned();
            let oauth = if let Some(client) = client {
                client.lock().await.playlist_tracks(&id).await.ok()
            } else {
                None
            };
            if let Some(items) = oauth {
                (items, None)
            } else {
                let playlist=social_integrations::spotify_public::PublicSpotify::new().map_err(|_|ApiError::unavailable("Клиент Spotify недоступен."))?.playlist(&request.link).await.map_err(|_|ApiError::unavailable("Spotify не предоставил открытый список. Можно загрузить CSV из Exportify."))?;
                let notice=playlist.possibly_truncated.then(||"Spotify Embed может показывать только первые 100 треков. Для полного списка загрузи CSV Exportify.".to_owned());
                (playlist.tracks, notice)
            }
        }
        _ => return Err(ApiError::invalid("Источник импорта неизвестен.")),
    };
    start_job(app, owner, items, notice).await
}
pub(crate) async fn csv(
    State(app): State<App>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let owner = stats::owner(&app, &headers).await?;
    let items = social_integrations::spotify_public::parse_exportify(&body)
        .map_err(|_| ApiError::invalid("CSV не соответствует формату Exportify."))?;
    start_job(app, owner, items, None).await
}
async fn start_job(
    app: App,
    owner: String,
    items: Vec<social_integrations::ImportItem>,
    notice: Option<String>,
) -> Result<(axum::http::StatusCode, Json<serde_json::Value>), ApiError> {
    if items.len() > 10000 {
        return Err(ApiError::invalid("Лимит импорта — 10 000 записей."));
    }
    let id = uuid::Uuid::new_v4().simple().to_string();
    let total = items.len();
    let mut jobs = app.imports.jobs.lock().await;
    jobs.retain(|_, job| job.running || job.created.elapsed() < Duration::from_secs(6 * 3600));
    if jobs.len() >= 128
        || jobs
            .values()
            .filter(|job| job.owner == owner && job.running)
            .count()
            >= 2
    {
        return Err(ApiError::new(
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            "Дождись завершения текущего импорта.",
        ));
    }
    jobs.insert(
        id.clone(),
        ImportJob {
            owner,
            created: Instant::now(),
            total,
            rows: Vec::new(),
            running: true,
            notice: notice.clone(),
        },
    );
    drop(jobs);
    let job_id = id.clone();
    tokio::spawn(async move {
        use futures_util::StreamExt;
        let jobs = app.imports.jobs.clone();
        let mut batches = futures_util::stream::iter(items)
            .map(|item| {
                let resolver = Resolver(app.catalog.clone());
                async move { import_tracks([item], &resolver).await }
            })
            .buffered(4);
        while let Some(batch) = batches.next().await {
            let mut jobs = jobs.lock().await;
            if let Some(job) = jobs.get_mut(&job_id) {
                for track in batch {
                    let resolution = match track.resolution {
                        Resolution::Matched { id, confidence } => {
                            serde_json::json!({"status":"matched","id":id,"confidence":confidence})
                        }
                        Resolution::Ambiguous { candidates } => {
                            serde_json::json!({"status":"ambiguous","candidates":candidates})
                        }
                        Resolution::Unavailable(reason) => {
                            serde_json::json!({"status":"unavailable","reason":reason})
                        }
                        Resolution::ResolverError(reason) => {
                            serde_json::json!({"status":"error","reason":reason})
                        }
                        Resolution::Unmatched => serde_json::json!({"status":"unmatched"}),
                    };
                    job.rows
                        .push(serde_json::json!({"source":track.source,"resolution":resolution}));
                }
            }
        }
        if let Some(job) = jobs.lock().await.get_mut(&job_id) {
            job.running = false;
        }
    });
    Ok((
        axum::http::StatusCode::ACCEPTED,
        Json(serde_json::json!({"job_id":id,"total":total,"notice":notice})),
    ))
}
pub(crate) async fn status(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let owner = stats::owner(&app, &headers).await?;
    let jobs = app.imports.jobs.lock().await;
    let job = jobs
        .get(&id)
        .filter(|job| job.owner == owner)
        .ok_or_else(|| ApiError::missing("Импорт не найден."))?;
    Ok(Json(
        serde_json::json!({"status":if job.running{"running"}else{"completed"},"completed":job.rows.len(),"total":job.total,"rows":if job.running{None}else{Some(&job.rows)},"notice":job.notice}),
    ))
}
