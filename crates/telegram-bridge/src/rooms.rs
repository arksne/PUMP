//! Комнаты передают состояние плеера через WebSocket с отметкой серверного времени.
//! Ведущий управляет воспроизведением, а слушатели синхронизируют плееры.

use crate::{ApiError, App, MAX_TRACK_MS, catalog::Kind, now_ms};
use axum::{
    Json,
    extract::{
        Path, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{
        HeaderMap, StatusCode,
        header::{HOST, ORIGIN},
    },
    response::Response,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, broadcast};
use uuid::Uuid;

const LIFETIME: u64 = 6 * 60 * 60 * 1000;

#[derive(Clone, Default)]
pub(crate) struct Rooms(Arc<Mutex<HashMap<String, Arc<Room>>>>);

struct Room {
    owner: String,
    state: Mutex<Playback>,
    activity: AtomicU64,
    hosts: AtomicUsize,
    updates: broadcast::Sender<Playback>,
}

#[derive(Clone, Serialize)]
struct Playback {
    track_id: Option<String>,
    position_ms: u64,
    playing: bool,
    rate: f64,
    at_ms: u64,
    revision: u64,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Input {
    Join {
        token: String,
    },
    Ping {
        sent_ms: u64,
    },
    Playback {
        track_id: Option<String>,
        position_ms: u64,
        playing: bool,
        rate: f64,
        #[serde(default)]
        sampled_at_ms: Option<u64>,
        #[serde(default)]
        start_at_ms: Option<u64>,
    },
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Output {
    Joined {
        host: bool,
        playback: Playback,
        server_ms: u64,
    },
    Playback {
        playback: Playback,
    },
    Pong {
        sent_ms: u64,
        server_ms: u64,
    },
    Error {
        message: &'static str,
    },
}

#[derive(Serialize)]
pub(crate) struct Created {
    code: String,
}

pub(crate) async fn create(
    State(app): State<App>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<Created>), ApiError> {
    let owner = crate::stats::owner(&app, &headers).await?;
    let mut rooms = app.rooms.0.lock().await;
    let now = now_ms();
    rooms.retain(|_, room| now.saturating_sub(room.activity.load(Ordering::Relaxed)) < LIFETIME);
    if rooms.values().filter(|r| r.owner == owner).count() >= 8 || rooms.len() >= 256 {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "Достигнут лимит открытых комнат.",
        ));
    }
    let code = Uuid::new_v4().simple().to_string()[..12].to_owned();
    let (updates, _) = broadcast::channel(32);
    rooms.insert(
        code.clone(),
        Arc::new(Room {
            owner,
            state: Mutex::new(Playback {
                track_id: None,
                position_ms: 0,
                playing: false,
                rate: 1.0,
                at_ms: now,
                revision: 0,
            }),
            activity: AtomicU64::new(now),
            hosts: AtomicUsize::new(0),
            updates,
        }),
    );
    Ok((StatusCode::CREATED, Json(Created { code })))
}

pub(crate) async fn upgrade(
    State(app): State<App>,
    Path(code): Path<String>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    if let Some(origin) = headers.get(ORIGIN) {
        let origin = origin
            .to_str()
            .ok()
            .and_then(|value| url::Url::parse(value).ok());
        let same = origin.as_ref().is_some_and(|url| {
            if let Some(settings) = &app.telegram {
                return settings.url.origin() == url.origin();
            }
            let authority = match (url.host_str(), url.port()) {
                (Some(host), Some(port)) => format!("{host}:{port}"),
                (Some(host), None) => host.to_owned(),
                _ => String::new(),
            };
            headers.get(HOST).and_then(|value| value.to_str().ok()) == Some(authority.as_str())
        });
        if !same {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "Комната доступна со страницы PUMP.",
            ));
        }
    }
    let room = app
        .rooms
        .0
        .lock()
        .await
        .get(&code.to_ascii_lowercase())
        .cloned()
        .filter(|room| now_ms().saturating_sub(room.activity.load(Ordering::Relaxed)) < LIFETIME)
        .ok_or_else(|| ApiError::missing("Комната не найдена."))?;
    Ok(ws
        .max_message_size(4096)
        .max_frame_size(4096)
        .on_upgrade(move |socket| serve(socket, app, room)))
}

async fn serve(mut socket: WebSocket, app: App, room: Arc<Room>) {
    let join = tokio::time::timeout(Duration::from_secs(5), socket.recv()).await;
    let token = match join {
        Ok(Some(Ok(Message::Text(text)))) => match serde_json::from_str(&text) {
            Ok(Input::Join { token }) => token,
            _ => return,
        },
        _ => return,
    };
    let Ok(Some(owner)) = app.stats.owner(&token).await else {
        let _ = send(
            &mut socket,
            Output::Error {
                message: "Сессия комнаты недействительна.",
            },
        )
        .await;
        return;
    };
    let host = owner == room.owner;
    let mut updates = room.updates.subscribe();
    let first = {
        let current = room.state.lock().await;
        if host {
            room.hosts.fetch_add(1, Ordering::Relaxed);
        }
        current.clone()
    };
    if !send(
        &mut socket,
        Output::Joined {
            host,
            playback: first,
            server_ms: now_ms(),
        },
    )
    .await
    {
        if host {
            disconnect_host(&room).await;
        }
        return;
    }
    let mut last_write = 0;
    let mut last_read = tokio::time::Instant::now();
    let interval = Duration::from_secs(20);
    let mut heartbeat = tokio::time::interval_at(last_read + interval, interval);
    loop {
        tokio::select! {
            input = socket.recv() => {
                let Some(Ok(input)) = input else { break };
                last_read = tokio::time::Instant::now();
                let Message::Text(text) = input else {
                    if matches!(input, Message::Close(_)) { break; }
                    continue;
                };
                let now = now_ms();
                room.activity.store(now, Ordering::Relaxed);
                match serde_json::from_str(&text) {
                    Ok(Input::Ping { sent_ms }) => {
                        if !send(&mut socket, Output::Pong { sent_ms, server_ms: now }).await { break; }
                    }
                    Ok(Input::Playback { track_id, position_ms, playing, rate, sampled_at_ms, start_at_ms }) => {
                        if !host {
                            if !send(&mut socket, Output::Error { message: "Только ведущий управляет комнатой." }).await { break; }
                            continue;
                        }
                        if now.saturating_sub(last_write) < 45 { continue; }
                        last_write = now;
                        if position_ms > MAX_TRACK_MS || !(0.5..=2.0).contains(&rate) || (track_id.is_none() && playing) { continue; }
                        if let Some(id) = track_id.as_deref()
                            && (id.len() > 64 || !app.catalog.track(id).await.is_some_and(|track| track.kind != Kind::Youtube)) {
                            if !send(&mut socket, Output::Error { message: "Для комнаты нужен трек с аудиопотоком." }).await { break; }
                            continue;
                        }
                        let mut current = room.state.lock().await;
                        let revision = current.revision.saturating_add(1);
                        // Keep the host's sample time rather than the later catalog lookup time.
                        // Bound untrusted client clocks and retain compatibility with older clients.
                        let at_ms = playback_anchor(now, playing, sampled_at_ms, start_at_ms);
                        *current = Playback { track_id, position_ms, playing, rate, at_ms, revision };
                        let _ = room.updates.send(current.clone());
                    }
                    Err(_) => {
                        if !send(&mut socket, Output::Error { message: "Неверная команда комнаты." }).await { break; }
                    }
                    Ok(Input::Join { .. }) => {}
                }
            }
            update = updates.recv() => {
                let playback = match update {
                    Ok(update) => update,
                    Err(broadcast::error::RecvError::Lagged(_)) => room.state.lock().await.clone(),
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                if !send(&mut socket, Output::Playback { playback }).await { break; }
            }
            _ = heartbeat.tick() => {
                if last_read.elapsed() >= Duration::from_secs(55)
                    || !matches!(tokio::time::timeout(Duration::from_secs(4), socket.send(Message::Ping(Default::default()))).await, Ok(Ok(()))) {
                    break;
                }
            }
        }
    }
    if host {
        disconnect_host(&room).await;
    }
}

/// A future anchor is an announced start, not a sample from a future clock.
fn playback_anchor(
    now: u64,
    playing: bool,
    sampled_at_ms: Option<u64>,
    start_at_ms: Option<u64>,
) -> u64 {
    if playing
        && let Some(start) = start_at_ms
        && start <= now.saturating_add(4_000)
        && now.saturating_sub(start) <= 5_000
    {
        return start;
    }
    sampled_at_ms
        .filter(|at| *at <= now.saturating_add(100) && now.saturating_sub(*at) <= 5_000)
        .unwrap_or(now)
        .min(now)
}

async fn disconnect_host(room: &Room) {
    let mut current = room.state.lock().await;
    if room.hosts.fetch_sub(1, Ordering::Relaxed) == 1 && current.playing {
        let now = now_ms();
        let elapsed = (now.saturating_sub(current.at_ms) as f64 * current.rate) as u64;
        current.position_ms = current
            .position_ms
            .saturating_add(elapsed)
            .min(MAX_TRACK_MS);
        current.playing = false;
        current.at_ms = now;
        current.revision = current.revision.saturating_add(1);
        let _ = room.updates.send(current.clone());
    }
}

async fn send(socket: &mut WebSocket, message: Output) -> bool {
    let Ok(text) = serde_json::to_string(&message) else {
        return false;
    };
    matches!(
        tokio::time::timeout(
            Duration::from_secs(4),
            socket.send(Message::Text(text.into()))
        )
        .await,
        Ok(Ok(()))
    )
}

#[cfg(test)]
mod tests {
    use super::playback_anchor;

    #[test]
    fn scheduled_start_is_bounded_and_only_used_for_playing() {
        assert_eq!(
            playback_anchor(10_000, true, Some(9_900), Some(11_200)),
            11_200
        );
        assert_eq!(
            playback_anchor(10_000, true, Some(9_900), Some(15_000)),
            9_900
        );
        assert_eq!(
            playback_anchor(10_000, false, Some(9_900), Some(11_200)),
            9_900
        );
        assert_eq!(playback_anchor(10_000, true, Some(99_000), None), 10_000);
        assert_eq!(playback_anchor(10_000, true, Some(4_000), None), 10_000);
    }
}
