//! Сервер раздаёт плеер и API с того же адреса, на котором открывается Mini App.

use core_streaming as catalog;
use core_streaming::ApiError;
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
use std::{env, net::SocketAddr, path::PathBuf, sync::Arc};
use tower_http::trace::TraceLayer;
use url::Url;

pub(crate) const MAX_TRACK_MS: u64 = 24 * 60 * 60 * 1000;

#[derive(Clone)]
pub(crate) struct App {
    catalog: Arc<Catalog>,
    stats: stats::Store,
    rooms: rooms::Rooms,
    controls: controls::Controls,
    telegram: Option<Arc<telegram::Settings>>,
    lyrics: lyrics_engine::LyricsClient,
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
    let app = App {
        nas,
        plugins,
        plugin_media: plugin_media::PluginMedia::default(),
        imports: imports::Imports::default(),
        scrobbles: scrobbling::Scrobbles::new(data.join("lastfm")),
        catalog,
        stats,
        rooms: rooms::Rooms::default(),
        controls: controls::Controls::default(),
        telegram,
        lyrics: lyrics_engine::LyricsClient::new(http),
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
    let result = app.catalog.search(query).await?;
    Ok(Json(result))
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
