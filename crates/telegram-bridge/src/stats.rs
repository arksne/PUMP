//! Сессии и история прослушиваний хранятся в SQLite.
//! UUID события защищает статистику от двойного учёта при повторной отправке.

use crate::{ApiError, App, authorization, now_ms};
use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
};
pub(crate) use library_storage::sessions::{Period, Store, Summary, valid_token};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

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
    app.stats
        .record(
            &owner,
            &event.event_id,
            &track.id,
            &track.title,
            &track.artist,
            event.milliseconds,
        )
        .await
        .map_err(ApiError::storage)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Default, Deserialize)]
pub(crate) struct Filter {
    #[serde(default)]
    period: Period,
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
