//! NAS-серверы настраиваются администратором. Браузер получает билет на один файл.
use crate::{ApiError, App, now_ms, stats};
#[cfg(feature = "smb")]
use axum::body::Bytes;
use axum::{
    Json,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use library_storage::{NetworkMode, nas::WebDav};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::Mutex;
use uuid::Uuid;
#[derive(Clone, Deserialize)]
#[serde(tag = "protocol", rename_all = "snake_case")]
enum Configuration {
    Webdav {
        id: String,
        name: String,
        base: url::Url,
        username: Option<String>,
        password: Option<String>,
        owners: Vec<String>,
    },
    #[cfg(feature = "smb")]
    Smb {
        id: String,
        name: String,
        server: String,
        share: String,
        username: String,
        password: String,
        owners: Vec<String>,
    },
}
impl Configuration {
    fn info(&self) -> (&str, &str, &[String]) {
        match self {
            Self::Webdav {
                id, name, owners, ..
            } => (id, name, owners),
            #[cfg(feature = "smb")]
            Self::Smb {
                id, name, owners, ..
            } => (id, name, owners),
        }
    }
}
#[derive(Clone)]
struct Ticket {
    server: String,
    path: String,
    expires: u64,
}
#[derive(Clone, Default)]
pub(crate) struct NetworkStorage {
    servers: Arc<HashMap<String, Configuration>>,
    tickets: Arc<Mutex<HashMap<String, Ticket>>>,
}
impl NetworkStorage {
    pub fn load(path: Option<std::path::PathBuf>) -> anyhow::Result<Self> {
        let Some(path) = path else {
            return Ok(Self::default());
        };
        let config: Vec<Configuration> = serde_json::from_slice(&std::fs::read(path)?)?;
        let mut servers = HashMap::new();
        for server in config {
            let id = server.info().0.to_owned();
            anyhow::ensure!(
                !id.is_empty() && id.len() <= 64 && !servers.contains_key(&id),
                "ID NAS должен быть уникальным"
            );
            servers.insert(id, server);
        }
        Ok(Self {
            servers: Arc::new(servers),
            tickets: Arc::default(),
        })
    }
}
pub(crate) async fn list(
    State(app): State<App>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let owner = stats::owner(&app, &headers).await?;
    let servers: Vec<_> = app
        .nas
        .servers
        .values()
        .filter(|server| server.info().2.contains(&owner))
        .map(|server| serde_json::json!({"id":server.info().0,"name":server.info().1}))
        .collect();
    Ok(Json(servers))
}
#[derive(Deserialize)]
pub(crate) struct TicketRequest {
    path: String,
}
pub(crate) async fn ticket(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(input): Json<TicketRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let owner = stats::owner(&app, &headers).await?;
    let server = app
        .nas
        .servers
        .get(&id)
        .filter(|server| server.info().2.contains(&owner))
        .ok_or_else(|| ApiError::missing("NAS не найден."))?;
    let _ = server;
    validate_path(&input.path)?;
    let mut tickets = app.nas.tickets.lock().await;
    let now = now_ms();
    tickets.retain(|_, ticket| ticket.expires > now);
    if tickets.len() >= 4096 {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "Слишком много активных NAS-потоков.",
        ));
    }
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    tickets.insert(
        token.clone(),
        Ticket {
            server: id.clone(),
            path: input.path,
            expires: now + 6 * 3600000,
        },
    );
    let media_url = format!("/nas/stream/{token}");
    let path = tickets
        .get(&token)
        .map(|ticket| ticket.path.clone())
        .unwrap_or_default();
    drop(tickets);
    let track = core_streaming::Track {
        id: format!(
            "nas-{}",
            hex::encode(&Sha256::digest(format!("{id}\0{path}").as_bytes())[..16])
        ),
        title: path.rsplit(['/', '\\']).next().unwrap_or(&path).into(),
        artist: String::new(),
        kind: core_streaming::Kind::Full,
        duration_ms: 0,
        audio_url: Some(media_url.clone()),
        artwork: None,
        video_id: None,
        source: "NAS",
        attribution: None,
        provider: None,
    };
    app.catalog.register_track(track.clone()).await?;
    Ok(Json(serde_json::json!({"url":media_url,"track":track})))
}
fn validate_path(path: &str) -> Result<(), ApiError> {
    if path.is_empty()
        || path.len() > 4096
        || path.starts_with(['\\', '/'])
        || path
            .split(['\\', '/'])
            .any(|part| part == ".." || part.is_empty())
        || path.chars().any(char::is_control)
    {
        return Err(ApiError::invalid("Неверный путь NAS."));
    }
    Ok(())
}
/// Тело WebDAV с простоем не дольше 30 с: зависший NAS не держит
/// соединение и слот семафора вечно. Таймаут рвёт стрим целиком.
/// Переиспользуется прокси SoundCloud: угроза висящего upstream та же.
pub(crate) fn nas_body<S>(
    stream: S,
) -> impl futures_util::Stream<Item = Result<axum::body::Bytes, std::io::Error>>
where
    S: futures_util::Stream<Item = Result<axum::body::Bytes, reqwest::Error>> + Unpin,
{
    use futures_util::StreamExt as _;
    futures_util::stream::unfold((stream, false), |(mut stream, mut done)| async move {
        if done {
            return None;
        }
        match tokio::time::timeout(Duration::from_secs(30), stream.next()).await {
            Err(_) => {
                done = true;
                Some((
                    Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "простой потока NAS",
                    )),
                    (stream, done),
                ))
            }
            Ok(None) => None,
            Ok(Some(item)) => {
                let item =
                    item.map_err(|error| std::io::Error::new(std::io::ErrorKind::Other, error));
                let failed = item.is_err();
                Some((item, (stream, done || failed)))
            }
        }
    })
}

pub(crate) async fn stream(
    State(app): State<App>,
    Path(token): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let ticket = app
        .nas
        .tickets
        .lock()
        .await
        .get(&token)
        .filter(|ticket| ticket.expires > now_ms())
        .cloned()
        .ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "Билет NAS истёк."))?;
    let server = app
        .nas
        .servers
        .get(&ticket.server)
        .ok_or_else(|| ApiError::missing("NAS не найден."))?;
    match server {
        Configuration::Webdav {
            base,
            username,
            password,
            ..
        } => {
            let credentials = username
                .as_ref()
                .map(|name| (name.clone(), password.clone().unwrap_or_default()));
            let nas = WebDav::new(base.clone(), credentials)
                .map_err(|_| ApiError::invalid("Некорректная конфигурация WebDAV."))?;
            let range = headers
                .get(header::RANGE)
                .and_then(|value| value.to_str().ok());
            let response = nas
                .stream(&ticket.path, range, NetworkMode::Online)
                .await
                .map_err(|_| ApiError::unavailable("WebDAV не предоставил файл."))?;
            let mut builder = Response::builder()
                .status(response.status())
                .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
            for name in [
                header::CONTENT_TYPE,
                header::CONTENT_LENGTH,
                header::CONTENT_RANGE,
                header::ACCEPT_RANGES,
            ] {
                if let Some(value) = response.headers().get(&name) {
                    builder = builder.header(name, value);
                }
            }
            builder
                .body(Body::from_stream(nas_body(response.bytes_stream())))
                .map_err(|_| ApiError::unavailable("Некорректный ответ WebDAV."))
        }
        #[cfg(feature = "smb")]
        Configuration::Smb {
            server,
            share,
            username,
            password,
            ..
        } => {
            let storage = library_storage::smb_storage::SmbStorage::connect(
                server,
                share,
                username,
                password.clone(),
                NetworkMode::Online,
            )
            .await
            .map_err(|_| ApiError::unavailable("SMB недоступен."))?;
            let file = storage
                .open(&ticket.path, NetworkMode::Online)
                .await
                .map_err(|_| ApiError::missing("SMB-файл не найден."))?;
            let total = file
                .len()
                .await
                .map_err(|_| ApiError::unavailable("Размер SMB-файла недоступен."))?;
            let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
            let (start, end) = byte_range(range, total)?;
            let mut builder = Response::builder()
                .status(if range.is_some() {
                    StatusCode::PARTIAL_CONTENT
                } else {
                    StatusCode::OK
                })
                .header(header::CONTENT_TYPE, mime(&ticket.path))
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::CONTENT_LENGTH, end - start)
                .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
            if range.is_some() {
                builder = builder.header(
                    header::CONTENT_RANGE,
                    format!("bytes {start}-{}/{total}", end - 1),
                );
            }
            let body = futures_util::stream::try_unfold(
                (storage, file, start, end),
                |(storage, file, offset, end)| async move {
                    if offset >= end {
                        file.close().await?;
                        return Ok::<_, library_storage::Error>(None);
                    }
                    let bytes = tokio::time::timeout(
                        Duration::from_secs(30),
                        file.read_at(offset, (end - offset).min(256 * 1024) as usize),
                    )
                    .await
                    .map_err(|_| library_storage::Error::Limit("время ожидания SMB".into()))??;
                    if bytes.is_empty() {
                        return Err(library_storage::Error::Invalid(
                            "SMB EOF before expected range".into(),
                        ));
                    }
                    let next = offset + bytes.len() as u64;
                    Ok(Some((Bytes::from(bytes), (storage, file, next, end))))
                },
            );
            builder
                .body(Body::from_stream(body))
                .map_err(|_| ApiError::unavailable("Некорректный поток SMB."))
        }
    }
}
#[cfg(feature = "smb")]
fn mime(path: &str) -> &'static str {
    match path
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "m4a" | "mp4" => "audio/mp4",
        "ogg" | "opus" => "audio/ogg",
        _ => "application/octet-stream",
    }
}
#[cfg(any(feature = "smb", test))]
fn byte_range(range: Option<&str>, total: u64) -> Result<(u64, u64), ApiError> {
    let invalid = || ApiError::new(StatusCode::RANGE_NOT_SATISFIABLE, "Диапазон вне файла.");
    let Some(range) = range else {
        return Ok((0, total));
    };
    let (a, b) = range
        .strip_prefix("bytes=")
        .and_then(|v| v.split_once('-'))
        .ok_or_else(invalid)?;
    if a.is_empty() {
        let suffix = b.parse::<u64>().map_err(|_| invalid())?;
        if suffix == 0 || total == 0 {
            return Err(invalid());
        }
        return Ok((total.saturating_sub(suffix), total));
    }
    let start = a.parse::<u64>().map_err(|_| invalid())?;
    let end = if b.is_empty() {
        total
    } else {
        b.parse::<u64>()
            .map_err(|_| invalid())?
            .saturating_add(1)
            .min(total)
    };
    if start >= end || start >= total {
        return Err(invalid());
    }
    Ok((start, end))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ranges_and_paths() {
        assert_eq!(byte_range(Some("bytes=10-19"), 100).unwrap(), (10, 20));
        assert_eq!(byte_range(Some("bytes=-5"), 100).unwrap(), (95, 100));
        assert!(byte_range(Some("bytes=100-"), 100).is_err());
        assert!(validate_path("../private").is_err());
    }
}
