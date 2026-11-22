//! Сервер раздаёт плеер и API с того же адреса, на котором открывается Mini App.

mod catalog;
mod rooms;
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
    telegram: Option<Arc<telegram::Settings>>,
}

#[derive(Debug, thiserror::Error)]
#[error("{status}: {message}")]
pub(crate) struct ApiError {
    status: StatusCode,
    message: &'static str,
}

impl ApiError {
    fn new(status: StatusCode, message: &'static str) -> Self {
        Self { status, message }
    }

    fn invalid(message: &'static str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    fn missing(message: &'static str) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    fn unavailable(message: &'static str) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, message)
    }

    fn storage(error: sqlx::Error) -> Self {
        tracing::error!(%error, "Ошибка базы PUMP");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Не удалось сохранить данные.",
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({"error": self.message}))).into_response()
    }
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

#[tokio::main]
async fn main() -> Result<()> {
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
    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(4))
        .timeout(std::time::Duration::from_secs(9))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("PUMP/0.1 music player")
        .build()?;
    let media = setting("PUMP_MEDIA_DIR").map(PathBuf::from);
    let catalog = Arc::new(Catalog::load(media, http, setting("YOUTUBE_API_KEY")).await?);
    let app = App {
        catalog,
        stats,
        rooms: rooms::Rooms::default(),
        telegram,
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
                    include_str!("../web/app.js"),
                )
            }),
        )
        .route("/healthz", get(|| async { Json(json!({"ok": true})) }))
        .route("/api/search", get(search))
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
        "default-src 'self'; script-src 'self' https://telegram.org; style-src 'self' 'unsafe-inline'; ",
        "img-src 'self' https:; media-src 'self' https://*.mzstatic.com https://*.itunes.apple.com; ",
        "frame-src https://www.youtube-nocookie.com; connect-src 'self' ws: wss:; ",
        "frame-ancestors 'self' https://web.telegram.org https://*.telegram.org; ",
        "object-src 'none'; base-uri 'none'; form-action 'self'"
    );
    (
        [
            (CONTENT_SECURITY_POLICY, policy),
            (REFERRER_POLICY, "strict-origin-when-cross-origin"),
        ],
        Html(include_str!("../index.html")),
    )
}

async fn search(
    State(app): State<App>,
    Query(query): Query<SearchQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let result = app.catalog.search(query).await?;
    Ok(Json(result))
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
    Ok(Json(app.catalog.lyrics(&track).await?))
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
