//! Бот открывает Mini App, выдаёт MP3 через inline-режим и публикует итоги.
//! Сервер проверяет подпись initData перед привязкой сессии к пользователю.

use crate::{
    App,
    catalog::{Kind, SearchQuery, Source},
    stats::Period,
};
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::{collections::BTreeMap, sync::Arc};
use teloxide::{
    prelude::*,
    types::{
        AllowedUpdate, InlineKeyboardButton, InlineKeyboardMarkup, InlineQueryResult,
        InlineQueryResultAudio, Seconds, UpdateKind, WebAppInfo,
    },
};
use url::Url;

type HmacSha256 = Hmac<Sha256>;
type HandlerError = Box<dyn std::error::Error + Send + Sync>;
pub(crate) const WEBHOOK_PATH: &str = "/telegram/webhook";

pub(crate) struct Settings {
    pub token: String,
    pub url: Url,
    webhook: bool,
    bot: Bot,
}

impl Settings {
    pub(crate) fn new(token: String, url: Url, webhook: bool) -> Self {
        let bot = Bot::new(&token);
        Self {
            token,
            url,
            webhook,
            bot,
        }
    }
}

pub(crate) fn identity(input: &str, token: &str, now: u64) -> Option<u64> {
    if input.len() > 16_384 {
        return None;
    }
    let mut fields = BTreeMap::new();
    for (key, value) in url::form_urlencoded::parse(input.as_bytes()) {
        if fields
            .insert(key.into_owned(), value.into_owned())
            .is_some()
        {
            return None;
        }
    }
    let received = hex::decode(fields.remove("hash")?).ok()?;
    let time: u64 = fields.get("auth_date")?.parse().ok()?;
    if time > now.saturating_add(60) || now.saturating_sub(time) > 12 * 3600 {
        return None;
    }
    let data = fields
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut key = HmacSha256::new_from_slice(b"WebAppData").ok()?;
    key.update(token.as_bytes());
    let secret = key.finalize().into_bytes();
    let mut signature = HmacSha256::new_from_slice(&secret).ok()?;
    signature.update(data.as_bytes());
    signature.verify_slice(&received).ok()?;
    let user: serde_json::Value = serde_json::from_str(fields.get("user")?).ok()?;
    user["id"].as_u64().filter(|id| *id > 0)
}

pub(crate) async fn run(settings: Arc<Settings>, app: App) {
    if settings.webhook {
        register_webhook(&settings).await;
        return;
    }
    let dispatcher = dptree::entry()
        .branch(Update::filter_message().endpoint(message))
        .branch(Update::filter_inline_query().endpoint(inline));
    Dispatcher::builder(settings.bot.clone(), dispatcher)
        .dependencies(dptree::deps![app, settings])
        .build()
        .dispatch()
        .await;
}

fn webhook_mac(settings: &Settings) -> HmacSha256 {
    let mut mac = HmacSha256::new_from_slice(settings.token.as_bytes())
        .expect("HMAC принимает ключ любой длины");
    // Секрет webhook остаётся прежним после перезапуска и привязан к адресу PUMP.
    mac.update(b"pump/telegram-webhook/v1\0");
    mac.update(settings.url.as_str().as_bytes());
    mac
}

async fn register_webhook(settings: &Settings) {
    let mut url = settings.url.clone();
    url.set_path(WEBHOOK_PATH);
    let secret = hex::encode(webhook_mac(settings).finalize().into_bytes());
    for delay in [0, 3, 15] {
        if delay > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
        }
        let result = settings
            .bot
            .set_webhook(url.clone())
            .allowed_updates([AllowedUpdate::Message, AllowedUpdate::InlineQuery])
            .secret_token(secret.clone())
            .await;
        if result.is_ok() {
            tracing::info!("Telegram webhook зарегистрирован");
            return;
        }
    }
    tracing::error!(
        "Не удалось зарегистрировать Telegram webhook. Проверь токен бота и публичный адрес."
    );
}

pub(crate) async fn webhook(State(app): State<App>, headers: HeaderMap, body: Bytes) -> StatusCode {
    let Some(settings) = app.telegram.clone().filter(|settings| settings.webhook) else {
        return StatusCode::NOT_FOUND;
    };
    if !valid_webhook_token(&settings, &headers) {
        return StatusCode::UNAUTHORIZED;
    }
    let Ok(update) = serde_json::from_slice::<Update>(&body) else {
        return StatusCode::BAD_REQUEST;
    };
    let bot = settings.bot.clone();
    let result = match update.kind {
        UpdateKind::Message(input) => message(bot, input, app, settings).await,
        UpdateKind::InlineQuery(input) => inline(bot, input, app, settings).await,
        _ => Ok(()),
    };
    if result.is_ok() {
        StatusCode::OK
    } else {
        tracing::warn!("Не удалось обработать обновление Telegram");
        StatusCode::SERVICE_UNAVAILABLE
    }
}

fn valid_webhook_token(settings: &Settings, headers: &HeaderMap) -> bool {
    headers
        .get("x-telegram-bot-api-secret-token")
        .filter(|value| value.as_bytes().len() == 64)
        .and_then(|value| hex::decode(value.as_bytes()).ok())
        .is_some_and(|signature| webhook_mac(settings).verify_slice(&signature).is_ok())
}

async fn message(
    bot: Bot,
    message: Message,
    app: App,
    settings: Arc<Settings>,
) -> Result<(), HandlerError> {
    if message.chat.is_private() {
        let media = message
            .voice()
            .map(|voice| &voice.file)
            .or_else(|| message.video_note().map(|note| &note.file));
        if let Some(media) = media {
            if media.size > 20 * 1024 * 1024 {
                bot.send_message(message.chat.id, "Запись должна быть меньше 20 МБ.")
                    .await?;
                return Ok(());
            }
            if crate::setting("AUDD_TOKEN").is_none() {
                bot.send_message(
                    message.chat.id,
                    "Для распознавания нужно настроить AUDD_TOKEN на сервере PUMP.",
                )
                .await?;
                return Ok(());
            }
            use teloxide::net::Download;
            let file = bot.get_file(media.id.clone()).await?;
            let mut bytes = Vec::new();
            bot.download_file(&file.path, &mut bytes).await?;
            let answer = match crate::recognition::identify(bytes).await {
                Ok(result) if !result.is_null() => format!(
                    "{} — {}\n{}",
                    result["artist"].as_str().unwrap_or(""),
                    result["title"].as_str().unwrap_or(""),
                    result["song_link"].as_str().unwrap_or("")
                ),
                Ok(_) => "Совпадение не найдено.".into(),
                Err(_) => "Распознавание сейчас недоступно.".into(),
            };
            bot.send_message(message.chat.id, answer).await?;
            return Ok(());
        }
    }
    let command = message
        .text()
        .unwrap_or("")
        .split_whitespace()
        .next()
        .unwrap_or("")
        .split('@')
        .next()
        .unwrap_or("");
    if command == "/start" {
        if !message.chat.is_private() {
            return Ok(());
        }
        let mut url = settings.url.clone();
        url.query_pairs_mut().append_pair("telegram", "1");
        let button = InlineKeyboardButton::web_app("Открыть PUMP", WebAppInfo { url });
        bot.send_message(message.chat.id, "Открой PUMP, чтобы послушать музыку.")
            .reply_markup(InlineKeyboardMarkup::new([[button]]))
            .await?;
    } else if command == "/stats"
        && message.chat.is_private()
        && let Some(user) = &message.from
    {
        let period = if message.text().unwrap_or("").split_whitespace().nth(1) == Some("month") {
            Period::Month
        } else {
            Period::Week
        };
        let stats = app
            .stats
            .read(&format!("telegram:{}", user.id.0), period)
            .await?;
        let seconds = stats.milliseconds.max(0) / 1000;
        let mut report = format!(
            "Итоги прослушиваний\n{} мин {} сек · треков: {}",
            seconds / 60,
            seconds % 60,
            stats.tracks
        );
        for (index, rank) in stats.top_tracks.iter().enumerate() {
            report.push_str(&format!(
                "\n{}. {} — {}",
                index + 1,
                rank.artist,
                rank.title
            ));
        }
        let png = crate::cards::render(&stats)?;
        bot.send_photo(
            message.chat.id,
            teloxide::types::InputFile::memory(png).file_name("pump-stats.png"),
        )
        .caption(report.chars().take(1000).collect::<String>())
        .await?;
    }
    if message.chat.is_private()
        && let Some(user) = &message.from
    {
        let argument = message
            .text()
            .unwrap_or("")
            .split_once(char::is_whitespace)
            .map(|(_, value)| value.trim())
            .unwrap_or("");
        if let Some(action) = match command {
            "/play" => Some("play"),
            "/pause" => Some("pause"),
            "/next" => Some("next"),
            "/previous" => Some("previous"),
            "/seek" => Some("seek"),
            "/queue" => Some("queue"),
            "/eq" => Some("eq"),
            _ => None,
        } {
            let mut control = crate::controls::Command::new(action);
            if matches!(action, "play" | "queue") && !argument.is_empty() {
                let found = app
                    .catalog
                    .search(SearchQuery {
                        q: argument.chars().take(140).collect(),
                        source: Source::Full,
                    })
                    .await?;
                let Some(track) = found.tracks.first() else {
                    bot.send_message(message.chat.id, "Трек не найден в медиатеке.")
                        .await?;
                    return Ok(());
                };
                control.track_id = Some(track.id.clone());
            }
            if action == "seek" {
                let seconds = argument
                    .parse::<f64>()
                    .ok()
                    .filter(|value| value.is_finite() && (0.0..=86400.0).contains(value));
                let Some(seconds) = seconds else {
                    bot.send_message(
                        message.chat.id,
                        "Укажи позицию в секундах, например /seek 90",
                    )
                    .await?;
                    return Ok(());
                };
                control.position_ms = Some((seconds * 1000.0) as u64);
            }
            if action == "eq" {
                let gains = argument
                    .split_whitespace()
                    .map(str::parse::<f32>)
                    .collect::<Result<Vec<_>, _>>();
                let Ok(gains) = gains else {
                    bot.send_message(
                        message.chat.id,
                        "Для /eq нужны 10 значений усиления от −12 до 12 дБ.",
                    )
                    .await?;
                    return Ok(());
                };
                if gains.len() != 10
                    || gains
                        .iter()
                        .any(|gain| !gain.is_finite() || !(-12.0..=12.0).contains(gain))
                {
                    bot.send_message(
                        message.chat.id,
                        "Для /eq нужны 10 значений усиления от −12 до 12 дБ.",
                    )
                    .await?;
                    return Ok(());
                }
                control.bands = Some(gains);
            }
            let delivered = app
                .controls
                .send(&format!("telegram:{}", user.id.0), control)
                .await;
            bot.send_message(
                message.chat.id,
                if delivered {
                    "Команда отправлена в плеер."
                } else {
                    "Открой PUMP через кнопку бота, чтобы подключить плеер."
                },
            )
            .await?;
        }
    }
    Ok(())
}

async fn inline(
    bot: Bot,
    input: InlineQuery,
    app: App,
    settings: Arc<Settings>,
) -> Result<(), HandlerError> {
    let query = SearchQuery {
        q: input.query.chars().take(140).collect(),
        source: Source::Full,
    };
    let search = app.catalog.search(query).await?;
    let results = audio_results(search.tracks, &settings.url);
    bot.answer_inline_query(input.id, results)
        .cache_time(60)
        .await?;
    Ok(())
}

fn audio_results(tracks: Vec<crate::catalog::Track>, origin: &Url) -> Vec<InlineQueryResult> {
    tracks
        .into_iter()
        .filter_map(|track| {
            if track.kind != Kind::Full {
                return None;
            }
            let url = origin.join(track.audio_url.as_deref()?).ok()?;
            // Для InlineQueryResultAudio Telegram принимает ссылки на MP3.
            if track.source == "Медиатека" && !track.id.starts_with("mp3-") {
                return None;
            }
            let mut result = InlineQueryResultAudio::new(track.id, url, track.title);
            if !track.artist.is_empty() {
                result = result.performer(track.artist);
            }
            if track.duration_ms >= 1000 {
                result =
                    result.audio_duration(Seconds::from_seconds((track.duration_ms / 1000) as u32));
            }
            if let Some(credit) = track.attribution {
                result = result.caption(credit);
            }
            Some(result.into())
        })
        .take(20)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webhook_rejects_missing_token_and_tokens_for_other_hosts() {
        let token = "123:test";
        let settings = Settings::new(
            token.into(),
            Url::parse("https://pump.example.com/").unwrap(),
            true,
        );
        let other = Settings::new(
            token.into(),
            Url::parse("https://other.example.com/").unwrap(),
            true,
        );
        let mut headers = HeaderMap::new();
        assert!(!valid_webhook_token(&settings, &headers));
        let signature = hex::encode(webhook_mac(&settings).finalize().into_bytes());
        headers.insert(
            "x-telegram-bot-api-secret-token",
            signature.parse().unwrap(),
        );
        assert!(valid_webhook_token(&settings, &headers));
        assert!(!valid_webhook_token(&other, &headers));
    }

    #[tokio::test]
    async fn inline_query_supplies_mp3_as_telegram_audio() {
        let catalog = crate::catalog::Catalog::load(None, reqwest::Client::new(), None)
            .await
            .unwrap();
        let source = catalog
            .search(SearchQuery {
                q: "Song 1".into(),
                source: Source::Full,
            })
            .await
            .unwrap();
        let mut preview = source.tracks[0].clone();
        preview.kind = Kind::Preview;
        let mut tracks = source.tracks;
        tracks.push(preview);
        let result = audio_results(tracks, &Url::parse("https://pump.example.com/").unwrap());
        assert_eq!(result.len(), 1);
        let payload = serde_json::to_value(&result[0]).unwrap();
        assert_eq!(payload["type"], "audio");
        assert_eq!(payload["performer"], "SoundHelix");
        assert!(payload["audio_url"].as_str().unwrap().ends_with(".mp3"));
    }

    #[test]
    fn signed_data_rejects_changes_and_expired_timestamps() {
        let data = "auth_date=1700000000\nuser={\"id\":42}";
        let mut key = HmacSha256::new_from_slice(b"WebAppData").unwrap();
        key.update(b"123:test");
        let mut signature = HmacSha256::new_from_slice(&key.finalize().into_bytes()).unwrap();
        signature.update(data.as_bytes());
        let hash = hex::encode(signature.finalize().into_bytes());
        let original = format!("user=%7B%22id%22%3A42%7D&hash={hash}&auth_date=1700000000");
        assert_eq!(identity(&original, "123:test", 1_700_000_003), Some(42));
        assert_eq!(
            identity(&original.replace("42", "43"), "123:test", 1_700_000_003),
            None
        );
        assert_eq!(
            identity(&original, "123:test", 1_700_000_000 + 43_201),
            None
        );
    }
}
