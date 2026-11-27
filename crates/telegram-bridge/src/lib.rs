//! Сервер раздаёт плеер и API с того же адреса, на котором открывается Mini App.

use core_streaming as catalog;
use core_streaming::ApiError;
mod analysis_api;
mod cards;
mod controls;
mod imports;
mod library;
mod nas_api;
mod plugin_media;
mod plugins;
mod recognition;
mod rooms;
mod scrobbling;
mod stats;
mod telegram;

use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use catalog::{Catalog, SearchQuery};
use serde_json::json;
use social_integrations::spotify_app::SpotifyAppClient;
use std::{env, net::SocketAddr, path::PathBuf, sync::Arc};
use tower_http::trace::TraceLayer;
use url::Url;

pub(crate) const MAX_TRACK_MS: u64 = 24 * 60 * 60 * 1000;

#[derive(Clone)]
pub(crate) struct App {
    catalog: Arc<Catalog>,
    analysis: analysis_api::AnalysisCache,
    http: reqwest::Client,
    stats: stats::Store,
    rooms: rooms::Rooms,
    controls: controls::Controls,
    telegram: Option<Arc<telegram::Settings>>,
    lyrics: lyrics_engine::LyricsClient,
    spotify_app: Option<std::sync::Arc<SpotifyAppClient>>,
    library: library::Libraries,
    nas: nas_api::NetworkStorage,
    plugins: Arc<plugin_spine::PluginRegistry>,
    plugin_media: plugin_media::PluginMedia,
    imports: imports::Imports,
    scrobbles: scrobbling::Scrobbles,
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn setting(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn public_url(value: &str) -> Result<Url> {
    let url = Url::parse(value).context("Неверный WEBAPP_URL")?;
    if url.scheme() != "https"
        || !url.has_host()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!(
            "WEBAPP_URL должен быть публичным HTTPS-адресом без пути, параметров и учётных данных."
        );
    }
    Ok(url)
}

pub async fn run() -> Result<()> {
    // Значения из окружения хостинга имеют приоритет над локальным .env.
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt().with_target(false).init();
    let render_url = setting("RENDER_EXTERNAL_URL");
    let host = setting("BIND_ADDR").unwrap_or_else(|| {
        if render_url.is_some() {
            "0.0.0.0"
        } else {
            "127.0.0.1"
        }
        .into()
    });
    let port: u16 = setting("PORT")
        .unwrap_or_else(|| "8080".into())
        .parse()
        .context("PORT должен быть номером порта")?;
    let addr: SocketAddr = format!("{host}:{port}")
        .parse()
        .context("Неверный адрес сервера")?;
    let data = PathBuf::from(setting("PUMP_DATA_DIR").unwrap_or_else(|| "data".into()));
    tokio::fs::create_dir_all(&data)
        .await
        .context("Не удалось создать каталог данных")?;
    let stats = stats::Store::open(&data.join("pump.sqlite")).await?;

    let telegram = match setting("TELOXIDE_TOKEN").or_else(|| setting("TELEGRAM_BOT_TOKEN")) {
        Some(token) => {
            let url = setting("WEBAPP_URL")
                .or_else(|| render_url.clone())
                .context("Для Telegram укажи WEBAPP_URL")?;
            Some(Arc::new(telegram::Settings::new(
                token,
                public_url(&url)?,
                render_url.is_some(),
            )))
        }
        None => None,
    };
    let http = catalog::network::client_builder()?.build()?;
    let media = setting("PUMP_MEDIA_DIR").map(PathBuf::from);
    let catalog = Arc::new(
        Catalog::load(media, http.clone(), setting("YOUTUBE_API_KEY"))
            .await?
            .with_session(setting("PUMP_YOUTUBE_COOKIES"))?,
    );
    let plugins = Arc::new(plugins::load(
        setting("PUMP_PLUGIN_DIR").map(PathBuf::from),
    )?);
    let nas = nas_api::NetworkStorage::load(setting("PUMP_NAS_CONFIG").map(PathBuf::from))?;
    let spotify_app = SpotifyAppClient::from_env(http.clone()).map(std::sync::Arc::new);
    let app = App {
        nas,
        http: http.clone(),
        plugins,
        plugin_media: plugin_media::PluginMedia::default(),
        imports: imports::Imports::default(),
        scrobbles: scrobbling::Scrobbles::new(data.join("lastfm")),
        catalog,
        analysis: analysis_api::AnalysisCache::default(),
        stats,
        rooms: rooms::Rooms::default(),
        controls: controls::Controls::default(),
        telegram,
        lyrics: lyrics_engine::LyricsClient::new(http),
        spotify_app,
        library: library::Libraries::new(
            data.join("downloads"),
            url::Url::parse(
                &setting("WEBAPP_URL")
                    .or_else(|| render_url.clone())
                    .unwrap_or_else(|| format!("http://127.0.0.1:{port}")),
            )?,
        ),
    };
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .context("Не удалось открыть порт")?;
    tracing::info!(%addr, local_tracks = app.catalog.local_count(), "PUMP запущен");
    if let Some(settings) = app.telegram.clone() {
        tokio::spawn(telegram::run(settings, app.clone()));
    }
    axum::serve(listener, router(app))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

fn router(app: App) -> Router {
    Router::new()
        .route("/", get(player))
        .route(
            "/assets/app.js",
            get(|| async {
                (
                    [(
                        axum::http::header::CONTENT_TYPE,
                        "application/javascript; charset=utf-8",
                    )],
                    ui_liquid_glass::APP_JS,
                )
            }),
        )
        .route(
            "/assets/ui.js",
            get(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "application/javascript")],
                    ui_liquid_glass::UI_JS,
                )
            }),
        )
        .route(
            "/assets/ui.css",
            get(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "text/css")],
                    ui_liquid_glass::UI_CSS,
                )
            }),
        )
        .route(
            "/sw.js",
            get(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "application/javascript")],
                    ui_liquid_glass::SERVICE_WORKER_JS,
                )
            }),
        )
        .route(
            "/assets/ui-liquid-glass/pkg/ui_liquid_glass.js",
            get(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "application/javascript")],
                    ui_liquid_glass::WASM_JS,
                )
            }),
        )
        .route(
            "/assets/ui-liquid-glass/pkg/ui_liquid_glass_bg.wasm",
            get(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "application/wasm")],
                    ui_liquid_glass::WASM_BYTES,
                )
            }),
        )
        .route("/healthz", get(|| async { Json(json!({"ok": true})) }))
        .route(
            "/assets/dsp-client.js",
            get(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "application/javascript")],
                    ui_liquid_glass::DSP_CLIENT_JS,
                )
            }),
        )
        .route(
            "/assets/dsp-worklet.js",
            get(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "application/javascript")],
                    ui_liquid_glass::DSP_WORKLET_JS,
                )
            }),
        )
        .route(
            "/assets/ui-liquid-glass/pkg/pump_dsp.wasm",
            get(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "application/wasm")],
                    ui_liquid_glass::DSP_BYTES,
                )
            }),
        )
        .route("/api/search", get(search))
        .route("/api/analysis/{id}", get(analysis_api::track))
        .route("/source/sc/{id}", get(source_sc))
        .route("/source/au/{id}", get(source_au))
        .route("/api/nas", get(nas_api::list))
        .route("/api/nas/{id}/ticket", post(nas_api::ticket))
        .route("/nas/stream/{token}", get(nas_api::stream))
        .route("/api/lastfm/connect", post(scrobbling::connect))
        .route("/api/lastfm/scrobble", post(scrobbling::record))
        .route(
            "/api/recognize",
            post(recognition::upload).layer(axum::extract::DefaultBodyLimit::max(20 * 1024 * 1024)),
        )
        .route("/api/spotify/authorize", post(imports::authorize))
        .route("/api/spotify/callback", get(imports::callback))
        .route("/api/import", post(imports::playlist))
        .route("/api/import/{id}", get(imports::status))
        .route(
            "/api/import/csv",
            post(imports::csv).layer(axum::extract::DefaultBodyLimit::max(16 * 1024 * 1024)),
        )
        .route(
            "/addon/stream/{id}/{lease}/{index}",
            get(plugin_media::stream),
        )
        .route("/api/providers", get(plugins::list))
        .route("/api/providers/{id}/search", get(plugins::search))
        .route("/api/providers/{id}/resolve", post(plugins::resolve))
        .route("/api/downloads", get(library::list))
        .route("/api/downloads/{id}", post(library::download))
        .route("/api/downloads/{id}/file", get(library::file))
        .route("/api/eq/profile", post(eq_profile))
        .route("/ws/player", get(controls::upgrade))
        .route("/api/streams/{id}", get(streams))
        .route("/stream/{id}/{format}", get(stream))
        .route("/preview/{id}", get(preview_audio))
        .route("/api/tracks/{id}", get(track))
        .route("/api/lyrics/{id}", get(lyrics))
        .route("/media/{id}", get(media))
        .route("/covers/{id}", get(cover))
        .route("/api/session", post(stats::create_session))
        .route("/api/listens", post(stats::record))
        .route("/api/stats", get(stats::summary))
        .route("/api/rooms", post(rooms::create))
        .route("/ws/room/{code}", get(rooms::upgrade))
        .route(
            telegram::WEBHOOK_PATH,
            post(telegram::webhook).layer(axum::extract::DefaultBodyLimit::max(256 * 1024)),
        )
        .with_state(app)
        .layer(axum::extract::DefaultBodyLimit::max(24 * 1024))
        .layer(TraceLayer::new_for_http())
}

async fn player() -> impl IntoResponse {
    use axum::http::header::{CONTENT_SECURITY_POLICY, REFERRER_POLICY};
    let policy = concat!(
        "default-src 'self'; script-src 'self' 'wasm-unsafe-eval' https://telegram.org; style-src 'self' 'unsafe-inline'; ",
        "img-src 'self' https:; font-src 'self' blob:; media-src 'self' blob: https://*.mzstatic.com https://*.itunes.apple.com; ",
        "frame-src https://www.youtube-nocookie.com; connect-src 'self' ws: wss:; ",
        "frame-ancestors 'self' https://web.telegram.org https://*.telegram.org; ",
        "object-src 'none'; base-uri 'none'; form-action 'self'"
    );
    (
        [
            (CONTENT_SECURITY_POLICY, policy),
            (REFERRER_POLICY, "strict-origin-when-cross-origin"),
        ],
        Html(ui_liquid_glass::INDEX_HTML),
    )
}

async fn search(
    State(app): State<App>,
    Query(query): Query<SearchQuery>,
) -> Result<impl IntoResponse, ApiError> {
    if matches!(query.source, catalog::Source::Spotify) {
        if query.q.trim().is_empty() {
            return Ok(Json(catalog::Search {
                tracks: Vec::new(),
                notice: Some("Введи название или имя артиста.".into()),
            }));
        }
        return Ok(Json(spotify_search(&app, &query.q).await));
    }
    if !matches!(query.source, catalog::Source::All) || query.q.trim().is_empty() {
        let result = app.catalog.search(query).await?;
        return Ok(Json(result));
    }
    // Catalog::All already queries all public providers in parallel.
    // Spotify only contributes matched records, not a second audio stream.
    let term = query.q.clone();
    let spotify = async {
        if app.spotify_app.is_some() {
            Some(
                tokio::time::timeout(
                    std::time::Duration::from_secs(8),
                    spotify_matched(&app, &term),
                )
                .await,
            )
        } else {
            None
        }
    };
    let (base, found) = tokio::join!(app.catalog.search(query), spotify);
    let mut result = base?;
    let mut notices: Vec<String> = result.notice.take().into_iter().collect();
    match found {
        Some(Ok((mut tracks, total))) => {
            let shown = tracks.len();
            result.tracks.append(&mut tracks);
            notices.push(format!(
                "Spotify: показаны совпадения с каталогом PUMP: {shown} из {total}."
            ));
        }
        Some(Err(_)) => notices.push("Spotify не ответил вовремя.".into()),
        None => {}
    }
    let mut seen = std::collections::HashSet::new();
    result.tracks.retain(|track| seen.insert(track.id.clone()));
    result
        .tracks
        .sort_by_key(|track| track.kind != catalog::Kind::Full);
    result.tracks.truncate(64);
    if !notices.is_empty() {
        notices.sort_unstable();
        notices.dedup();
        result.notice = Some(notices.join(" "));
    }
    Ok(Json(result))
}

/// Поиск Spotify с сопоставлением: воспроизводятся только совпадения каталога.
async fn spotify_search(app: &App, term: &str) -> catalog::Search {
    let Some(client) = &app.spotify_app else {
        return catalog::Search {
            tracks: Vec::new(),
            notice: Some(
                "Для поиска Spotify задай SPOTIFY_CLIENT_ID и SPOTIFY_CLIENT_SECRET.".into(),
            ),
        };
    };
    let found = match client.search_tracks(term, 12).await {
        Ok(found) => found,
        Err(_) => {
            return catalog::Search {
                tracks: Vec::new(),
                notice: Some("Поиск Spotify недоступен.".into()),
            };
        }
    };
    let (tracks, total) = spotify_matched_inner(app, &found).await;
    let shown = tracks.len();
    catalog::Search {
        tracks,
        notice: Some(format!(
            "Показаны совпадения с каталогом PUMP: {shown} из {total}."
        )),
    }
}

async fn spotify_matched(app: &App, term: &str) -> (Vec<catalog::Track>, usize) {
    let Some(client) = &app.spotify_app else {
        return (Vec::new(), 0);
    };
    let Ok(found) = client.search_tracks(term, 12).await else {
        return (Vec::new(), 0);
    };
    spotify_matched_inner(app, &found).await
}

async fn spotify_matched_inner(
    app: &App,
    found: &[social_integrations::TrackMetadata],
) -> (Vec<catalog::Track>, usize) {
    let total = found.len();
    let mut tracks = Vec::new();
    for meta in found {
        let q = format!("{} {}", meta.artists.join(" "), meta.title);
        let Ok(local) = app
            .catalog
            .search(SearchQuery {
                q: q.chars().take(140).collect(),
                source: catalog::Source::Full,
            })
            .await
        else {
            continue;
        };
        if let Some(hit) = local
            .tracks
            .iter()
            .find(|t| t.title.eq_ignore_ascii_case(&meta.title))
        {
            tracks.push(hit.clone());
        }
    }
    (tracks, total)
}

async fn streams(
    State(app): State<App>,
    Path(id): Path<String>,
    Query(options): Query<catalog::innertube::StreamOptions>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(Json(app.catalog.streams(&id, &options).await?))
}

async fn stream(
    State(app): State<App>,
    Path((id, format)): Path<(String, u64)>,
    request: axum::extract::Request,
) -> Response {
    app.catalog
        .stream(&id, format, request.method().clone(), request.headers())
        .await
        .into_response()
}

/// Прокси SoundCloud: свежая подписанная ссылка получается на каждый запрос,
/// байты идут через сервер (работает и там, где CDN источника недоступен клиенту).
/// Параллельные потоки SoundCloud ограничены: каждый держит исходящее
/// соединение до 120 с, без семафора скрипт перебора id положит сеть.
static SC_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(8);
static AU_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(8);

/// Один диапазон `bytes=start-end`, `bytes=start-` или `bytes=-suffix`.
/// Мультидиапазоны отклоняются: плеер их не запрашивает.
fn valid_range(value: Option<&axum::http::HeaderValue>) -> bool {
    let Some(value) = value.and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let Some(spec) = value.strip_prefix("bytes=") else {
        return false;
    };
    if spec.contains(',') {
        return false;
    }
    let (start, end) = match spec.split_once('-') {
        Some(pair) => pair,
        None => return false,
    };
    let number = |s: &str| s.is_empty() || s.bytes().all(|b| b.is_ascii_digit());
    number(start) && number(end) && !(start.is_empty() && end.is_empty())
}

async fn source_sc(
    State(app): State<App>,
    Path(id): Path<String>,
    request: axum::extract::Request,
) -> Response {
    use axum::http::header;
    if !core_streaming::soundcloud::valid_id(&id) {
        return ApiError::missing("Запись SoundCloud не найдена.").into_response();
    }
    if !valid_range(request.headers().get(header::RANGE)) {
        return ApiError::new(StatusCode::RANGE_NOT_SATISFIABLE, "Некорректный диапазон.")
            .into_response();
    }
    // Семафор берём ДО метаданных: перебор случайных id без него
    // устроит неограниченный поток запросов к api-v2.
    let permit = match SC_SLOTS.try_acquire() {
        Ok(permit) => permit,
        Err(_) => {
            return ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "Слишком много потоков. Повтори через несколько секунд.",
            )
            .into_response();
        }
    };
    // Холодный id различаем честно: 404 — нет записи, 409 — запись есть,
    // но открытого потока нет, 502 — источник временно недоступен.
    // Маскировка сбоя под «не найдено» ломала бы комнаты и статистику.
    match app.catalog.soundcloud_client().track(&id).await {
        Ok(meta) if meta.playback == core_streaming::soundcloud::Playback::Full => {
            let track = core_streaming::soundcloud_track(&meta);
            let _ = app.catalog.register_track(track).await;
        }
        Ok(_) => {
            return ApiError::new(
                StatusCode::CONFLICT,
                "Для этой записи нет открытого аудиопотока SoundCloud.",
            )
            .into_response();
        }
        Err(core_streaming::soundcloud::SourceError::NotFound) => {
            return ApiError::missing("Запись SoundCloud не найдена.").into_response();
        }
        Err(core_streaming::soundcloud::SourceError::NoStream) => {
            return ApiError::new(
                StatusCode::CONFLICT,
                "Для этой записи нет открытого аудиопотока SoundCloud.",
            )
            .into_response();
        }
        Err(_) => {
            return ApiError::unavailable("SoundCloud недоступен.").into_response();
        }
    };
    let stream = match app.catalog.soundcloud_client().resolve(&id).await {
        Ok(stream) => stream,
        Err(error) => return ApiError::from(error).into_response(),
    };
    let mut upstream_request = app
        .http
        .request(request.method().clone(), &stream.url)
        .timeout(std::time::Duration::from_secs(24 * 60 * 60));
    if let Some(range) = request.headers().get(header::RANGE) {
        upstream_request = upstream_request.header(header::RANGE, range);
    }
    // Заголовки — за 30 с; тело идёт с idle-таймаутом (см. nas_body),
    // общий 120-секундный дедлайн здесь вреден: он рвёт длинные треки.
    let upstream =
        match tokio::time::timeout(std::time::Duration::from_secs(30), upstream_request.send())
            .await
        {
            Ok(Ok(upstream)) => upstream,
            _ => return ApiError::unavailable("SoundCloud недоступен.").into_response(),
        };
    proxy_audio(upstream, permit)
}

async fn source_au(
    State(app): State<App>,
    Path(id): Path<String>,
    request: axum::extract::Request,
) -> Response {
    if !core_streaming::audius::valid_id(&id) {
        return ApiError::missing("Запись Audius не найдена.").into_response();
    }
    if !valid_range(request.headers().get(axum::http::header::RANGE)) {
        return ApiError::new(StatusCode::RANGE_NOT_SATISFIABLE, "Некорректный диапазон.")
            .into_response();
    }
    let Ok(permit) = AU_SLOTS.try_acquire() else {
        return ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "Слишком много потоков. Повтори через несколько секунд.",
        )
        .into_response();
    };
    match app
        .catalog
        .audius_client()
        .stream(&id, request.method().clone(), request.headers())
        .await
    {
        Ok(upstream) => proxy_audio(upstream, permit),
        Err(error) => error.into_response(),
    }
}

fn proxy_audio(
    upstream: reqwest::Response,
    permit: tokio::sync::SemaphorePermit<'static>,
) -> Response {
    use axum::http::header;
    let status = upstream.status();
    if !status.is_success() && status != StatusCode::RANGE_NOT_SATISFIABLE {
        return ApiError::unavailable("Аудиоисточник недоступен.").into_response();
    }
    let mime = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    if status != StatusCode::RANGE_NOT_SATISFIABLE && !mime.starts_with("audio/") {
        return ApiError::unavailable("Источник вернул не аудио.").into_response();
    }
    let mut response = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, mime)
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(header::CACHE_CONTROL, "private, no-store");
    for name in [
        header::CONTENT_LENGTH,
        header::CONTENT_RANGE,
        header::ACCEPT_RANGES,
    ] {
        if let Some(value) = upstream.headers().get(&name) {
            response = response.header(name, value);
        }
    }
    match response.body(axum::body::Body::from_stream(futures_util::stream::unfold(
        (upstream.bytes_stream(), Some(permit)),
        |(mut inner, permit)| async move {
            let permit = permit?;
            match tokio::time::timeout(
                std::time::Duration::from_secs(30),
                futures_util::StreamExt::next(&mut inner),
            )
            .await
            {
                Err(_) => {
                    drop(permit);
                    Some((
                        Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "простой аудиопотока",
                        )),
                        (inner, None),
                    ))
                }
                Ok(None) => {
                    drop(permit);
                    None
                }
                Ok(Some(item)) => {
                    let item = item.map_err(std::io::Error::other);
                    if item.is_err() {
                        drop(permit);
                        Some((item, (inner, None)))
                    } else {
                        Some((item, (inner, Some(permit))))
                    }
                }
            }
        },
    ))) {
        Ok(response) => response.into_response(),
        Err(_) => ApiError::unavailable("Некорректный ответ аудиоисточника.").into_response(),
    }
}

async fn track(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    app.catalog
        .track(&id)
        .await
        .map(Json)
        .ok_or_else(|| ApiError::missing("Трек не найден."))
}

async fn lyrics(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let track = app
        .catalog
        .track(&id)
        .await
        .ok_or_else(|| ApiError::missing("Трек не найден."))?;
    Ok(Json(
        app.lyrics
            .fetch(
                &track.artist,
                &track.title,
                Some(track.duration_ms).filter(|ms| *ms > 0),
            )
            .await
            .map_err(|_| ApiError::unavailable("Источники текстов недоступны."))?,
    ))
}

async fn media(
    State(app): State<App>,
    Path(id): Path<String>,
    request: axum::extract::Request,
) -> Response {
    use tower::ServiceExt;
    let id = id.strip_suffix(".mp3").unwrap_or(&id);
    let Some(file) = app.catalog.media(id) else {
        return app
            .catalog
            .demo_audio(id, request.method().clone(), request.headers())
            .await
            .into_response();
    };
    // ServeFile поддерживает HEAD и Range. Непосредственный путь клиенту недоступен.
    match tower_http::services::ServeFile::new(file)
        .oneshot(request)
        .await
    {
        Ok(response) => response.into_response(),
        Err(error) => match error {},
    }
}

async fn cover(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let Some((mime, bytes)) = app.catalog.cover(&id) else {
        return Err(ApiError::missing("Обложка не найдена."));
    };
    Ok((
        [
            (axum::http::header::CONTENT_TYPE, mime),
            (axum::http::header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        bytes,
    ))
}

pub(crate) fn authorization(headers: &HeaderMap) -> Result<&str, ApiError> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| stats::valid_token(token))
        .ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "Открой сессию PUMP повторно."))
}

#[derive(serde::Deserialize)]
struct EqProfile {
    text: String,
    sample_rate: u32,
}
async fn eq_profile(Json(profile): Json<EqProfile>) -> Result<impl IntoResponse, ApiError> {
    if profile.text.len() > 64 * 1024 {
        return Err(ApiError::invalid("Слишком большой профиль."));
    }
    let settings = audio_dsp_engine::eq::parse_autoeq(&profile.text, profile.sample_rate)
        .map_err(|_| ApiError::invalid("Некорректный профиль AutoEq."))?;
    Ok(Json(
        json!({"preamp":settings.preamp_db,"bands":settings.bands.iter().map(|band|json!({"frequency":band.frequency_hz,"gain":band.gain_db,"q":band.q,"enabled":band.enabled,"kind":format!("{:?}",band.kind)})).collect::<Vec<_>>()}),
    ))
}

async fn preview_audio(
    State(app): State<App>,
    Path(id): Path<String>,
    request: axum::extract::Request,
) -> Response {
    app.catalog
        .preview_audio(&id, request.method().clone(), request.headers())
        .await
        .into_response()
}
