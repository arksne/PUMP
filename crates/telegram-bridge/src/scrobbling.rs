//! Очередь Last.fm привязана к владельцу сессии. Секреты находятся в памяти.
use crate::{ApiError, App, now_ms, stats};
use axum::{Json, extract::State, http::HeaderMap, response::IntoResponse};
use sha2::{Digest, Sha256};
use social_integrations::{
    lastfm::{LastFmClient, LastFmCredentials, ScrobbleTrack},
    queue::ScrobbleQueue,
};
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tokio::sync::Mutex;
#[derive(Clone)]
pub(crate) struct Scrobbles {
    root: PathBuf,
    accounts: Arc<Mutex<HashMap<String, Arc<Mutex<Account>>>>>,
}
struct Account {
    session_hash: String,
    client: LastFmClient,
    queue: ScrobbleQueue,
}
impl Scrobbles {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            accounts: Arc::default(),
        }
    }
}
#[derive(serde::Deserialize)]
pub(crate) struct Credentials {
    session_key: String,
}
pub(crate) async fn connect(
    State(app): State<App>,
    headers: HeaderMap,
    Json(input): Json<Credentials>,
) -> Result<impl IntoResponse, ApiError> {
    let owner = stats::owner(&app, &headers).await?;
    if input.session_key.is_empty() || input.session_key.len() > 256 {
        return Err(ApiError::invalid("Некорректный session key Last.fm."));
    }
    let api_key = crate::setting("LASTFM_API_KEY")
        .ok_or_else(|| ApiError::invalid("Нужно настроить LASTFM_API_KEY."))?;
    let shared_secret = crate::setting("LASTFM_SHARED_SECRET")
        .ok_or_else(|| ApiError::invalid("Нужно настроить LASTFM_SHARED_SECRET."))?;
    let session_hash = hex::encode(Sha256::digest(
        format!("{owner}\0{}", input.session_key).as_bytes(),
    ));
    let client = LastFmClient::new(LastFmCredentials {
        api_key,
        shared_secret,
        session_key: input.session_key,
    })
    .map_err(|_| ApiError::invalid("Некорректные настройки Last.fm."))?;
    let mut accounts = app.scrobbles.accounts.lock().await;
    if let Some(account) = accounts.get(&owner) {
        let mut account = account.lock().await;
        if account.session_hash != session_hash {
            account.queue =
                ScrobbleQueue::open(app.scrobbles.root.join(format!("{session_hash}.json")))
                    .map_err(|_| ApiError::unavailable("Не удалось открыть очередь Last.fm."))?;
            account.session_hash = session_hash;
        }
        account.client = client;
    } else {
        let path = app.scrobbles.root.join(format!("{session_hash}.json"));
        let queue = ScrobbleQueue::open(path)
            .map_err(|_| ApiError::unavailable("Не удалось открыть очередь Last.fm."))?;
        accounts.insert(
            owner,
            Arc::new(Mutex::new(Account {
                session_hash,
                client,
                queue,
            })),
        );
    }
    Ok(axum::http::StatusCode::NO_CONTENT)
}
#[derive(serde::Deserialize)]
pub(crate) struct Playback {
    track_id: String,
    started_at_unix: u64,
    played_ms: u64,
    duration_ms: u64,
}
pub(crate) async fn record(
    State(app): State<App>,
    headers: HeaderMap,
    Json(input): Json<Playback>,
) -> Result<impl IntoResponse, ApiError> {
    let owner = stats::owner(&app, &headers).await?;
    let account = app
        .scrobbles
        .accounts
        .lock()
        .await
        .get(&owner)
        .cloned()
        .ok_or_else(|| ApiError::invalid("Подключи Last.fm в настройках."))?;
    if input.duration_ms > crate::MAX_TRACK_MS
        || input.played_ms > crate::MAX_TRACK_MS
        || input.started_at_unix > now_ms() / 1000 + 60
    {
        return Err(ApiError::invalid("Некорректное время прослушивания."));
    }
    let track = app
        .catalog
        .track(&input.track_id)
        .await
        .ok_or_else(|| ApiError::missing("Трек не найден."))?;
    let mut account = account.lock().await;
    let now = now_ms() / 1000;
    account
        .queue
        .enqueue(
            ScrobbleTrack {
                artist: track.artist,
                title: track.title,
                album: None,
                duration_ms: input.duration_ms,
                started_at_unix: input.started_at_unix,
            },
            input.played_ms,
            now,
        )
        .map_err(|_| ApiError::invalid("Событие Last.fm отклонено."))?;
    let Account { client, queue, .. } = &mut *account;
    let report = queue
        .flush(client, now, 10)
        .await
        .map_err(|_| ApiError::unavailable("Не удалось обновить очередь Last.fm."))?;
    Ok(Json(
        serde_json::json!({"accepted":report.accepted,"retried":report.retried,"rejected":report.rejected}),
    ))
}
