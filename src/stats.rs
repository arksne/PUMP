//! Сессии и история прослушиваний хранятся в SQLite.
//! UUID события защищает статистику от двойного учёта при повторной отправке.

use crate::{ApiError, App, authorization, now_ms};
use anyhow::{Context, Result};
use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use std::{path::Path, time::Duration};
use uuid::Uuid;

const DAY_MS: u64 = 86_400_000;

#[derive(Clone)]
pub(crate) struct Store(SqlitePool);

impl Store {
    pub(crate) async fn open(path: &Path) -> Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .context("Не удалось открыть SQLite PUMP")?;
        sqlx::raw_sql(
            "
            CREATE TABLE IF NOT EXISTS sessions (
              token_hash TEXT PRIMARY KEY, owner TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS listens (
              owner TEXT NOT NULL, event TEXT NOT NULL, track TEXT NOT NULL,
              title TEXT NOT NULL, artist TEXT NOT NULL, timestamp INTEGER NOT NULL,
              milliseconds INTEGER NOT NULL CHECK(milliseconds BETWEEN 100 AND 30000),
              PRIMARY KEY(owner,event)
            );
            CREATE INDEX IF NOT EXISTS listens_period ON listens(owner, timestamp);
        ",
        )
        .execute(&pool)
        .await?;
        Ok(Self(pool))
    }

    pub(crate) async fn owner(&self, token: &str) -> Result<Option<String>, sqlx::Error> {
        if !valid_token(token) {
            return Ok(None);
        }
        sqlx::query_scalar("SELECT owner FROM sessions WHERE token_hash = ?")
            .bind(hash(token))
            .fetch_optional(&self.0)
            .await
    }

    async fn new_session(&self, owner: &str) -> Result<String, sqlx::Error> {
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        sqlx::query("INSERT INTO sessions(token_hash, owner) VALUES(?,?)")
            .bind(hash(&token))
            .bind(owner)
            .execute(&self.0)
            .await?;
        Ok(token)
    }

    pub(crate) async fn read(&self, owner: &str, period: Period) -> Result<Summary, sqlx::Error> {
        let time = now_ms().saturating_sub(DAY_MS * period.days()) as i64;
        let since = |statement| sqlx::query(statement).bind(owner).bind(time);
        let totals = since("SELECT COALESCE(SUM(milliseconds),0) AS ms, COUNT(DISTINCT track) AS tracks FROM listens WHERE owner=? AND timestamp>=?")
            .fetch_one(&self.0).await?;
        let rows = since("SELECT MAX(title) AS name, MAX(artist) AS artist, SUM(milliseconds) AS ms FROM listens WHERE owner=? AND timestamp>=? GROUP BY track ORDER BY ms DESC, name LIMIT 5")
            .fetch_all(&self.0).await?;
        let artists = since("SELECT artist AS name, SUM(milliseconds) AS ms FROM listens WHERE owner=? AND timestamp>=? AND artist<>'' GROUP BY artist ORDER BY ms DESC, name LIMIT 5")
            .fetch_all(&self.0).await?;
        Ok(Summary {
            period,
            milliseconds: totals.get("ms"),
            tracks: totals.get("tracks"),
            top_tracks: rows
                .into_iter()
                .map(|row| Rank {
                    title: row.get("name"),
                    artist: row.get("artist"),
                    milliseconds: row.get("ms"),
                })
                .collect(),
            top_artists: artists
                .into_iter()
                .map(|row| Rank {
                    title: row.get("name"),
                    artist: String::new(),
                    milliseconds: row.get("ms"),
                })
                .collect(),
        })
    }
}

fn hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}
pub(crate) fn valid_token(token: &str) -> bool {
    token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit())
}

#[derive(Default, Deserialize)]
pub(crate) struct SessionRequest {
    init_data: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct Session {
    token: String,
    telegram: bool,
}

pub(crate) async fn create_session(
    State(app): State<App>,
    headers: HeaderMap,
    Json(input): Json<SessionRequest>,
) -> Result<Json<Session>, ApiError> {
    let old = authorization(&headers).ok();
    let (owner, telegram) = if let (Some(settings), Some(data)) = (
        app.telegram.as_ref(),
        input.init_data.as_deref().filter(|v| !v.is_empty()),
    ) {
        let id =
            crate::telegram::identity(data, &settings.token, now_ms() / 1000).ok_or_else(|| {
                ApiError::new(
                    StatusCode::UNAUTHORIZED,
                    "Сессия Telegram недействительна. Открой Mini App заново.",
                )
            })?;
        (format!("telegram:{id}"), true)
    } else {
        if let Some(token) = old
            && let Some(owner) = app.stats.owner(token).await.map_err(ApiError::storage)?
        {
            return Ok(Json(Session {
                token: token.into(),
                telegram: owner.starts_with("telegram:"),
            }));
        }
        (format!("guest:{}", Uuid::new_v4()), false)
    };
    if let Some(token) = old
        && app
            .stats
            .owner(token)
            .await
            .map_err(ApiError::storage)?
            .as_deref()
            == Some(owner.as_str())
    {
        return Ok(Json(Session {
            token: token.into(),
            telegram,
        }));
    }
    Ok(Json(Session {
        token: app
            .stats
            .new_session(&owner)
            .await
            .map_err(ApiError::storage)?,
        telegram,
    }))
}

#[derive(Deserialize)]
pub(crate) struct Listen {
    event_id: String,
    track_id: String,
    milliseconds: u64,
}

pub(crate) async fn owner(app: &App, headers: &HeaderMap) -> Result<String, ApiError> {
    let token = authorization(headers)?;
    app.stats
        .owner(token)
        .await
        .map_err(ApiError::storage)?
        .ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "Сессия PUMP не найдена."))
}

pub(crate) async fn record(
    State(app): State<App>,
    headers: HeaderMap,
    Json(event): Json<Listen>,
) -> Result<StatusCode, ApiError> {
    let owner = owner(&app, &headers).await?;
    if !(100..=30_000).contains(&event.milliseconds)
        || event.track_id.len() > 64
        || event.event_id.len() != 36
        || Uuid::parse_str(&event.event_id).is_err()
    {
        return Err(ApiError::invalid("Неверное событие прослушивания."));
    }
    let track = app
        .catalog
        .track(&event.track_id)
        .await
        .ok_or_else(|| ApiError::missing("Трек не найден."))?;
    if track.kind == crate::catalog::Kind::Youtube {
        return Err(ApiError::invalid(
            "События встроенного видео не учитываются.",
        ));
    }
    sqlx::query("INSERT INTO listens(owner,event,track,title,artist,timestamp,milliseconds) VALUES(?,?,?,?,?,?,?) ON CONFLICT(owner,event) DO NOTHING")
        .bind(owner).bind(event.event_id).bind(track.id).bind(track.title).bind(track.artist)
        .bind(now_ms() as i64).bind(event.milliseconds as i64).execute(&app.stats.0).await.map_err(ApiError::storage)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Period {
    #[default]
    Week,
    Month,
}
impl Period {
    fn days(self) -> u64 {
        match self {
            Self::Week => 7,
            Self::Month => 30,
        }
    }
}

#[derive(Default, Deserialize)]
pub(crate) struct Filter {
    #[serde(default)]
    period: Period,
}

#[derive(Serialize)]
pub(crate) struct Summary {
    period: Period,
    pub milliseconds: i64,
    pub tracks: i64,
    pub top_tracks: Vec<Rank>,
    top_artists: Vec<Rank>,
}

#[derive(Serialize)]
pub(crate) struct Rank {
    pub title: String,
    pub artist: String,
    milliseconds: i64,
}

pub(crate) async fn summary(
    State(app): State<App>,
    Query(filter): Query<Filter>,
    headers: HeaderMap,
) -> Result<Json<Summary>, ApiError> {
    let owner = owner(&app, &headers).await?;
    Ok(Json(
        app.stats
            .read(&owner, filter.period)
            .await
            .map_err(ApiError::storage)?,
    ))
}
