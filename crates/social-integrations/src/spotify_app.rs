//! Поиск Spotify по учёным данным приложения (Client Credentials).
//!
//! Пользовательский OAuth здесь не нужен: сервер сам получает токен по
//! `SPOTIFY_CLIENT_ID` + `SPOTIFY_CLIENT_SECRET` и ищет треки от своего имени.
//! Аудио Spotify так не отдаётся — результаты служат метаданными для
//! сопоставления с каталогом PUMP. Без настроенных ключей поиск недоступен,
//! и вызывающая сторона сообщает об этом статическим уведомлением.
use crate::{Error, Result, TrackMetadata};
use serde_json::Value;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

struct CachedToken {
    value: String,
    expires_at: Instant,
}

pub struct SpotifyAppClient {
    http: reqwest::Client,
    id: String,
    secret: String,
    token: Mutex<Option<CachedToken>>,
}

impl SpotifyAppClient {
    /// `None`, если ключи приложения не заданы. Пустые значения игнорируются.
    pub fn from_env(http: reqwest::Client) -> Option<Self> {
        let id = std::env::var("SPOTIFY_CLIENT_ID")
            .ok()
            .filter(|v| !v.trim().is_empty())?;
        let secret = std::env::var("SPOTIFY_CLIENT_SECRET")
            .ok()
            .filter(|v| !v.trim().is_empty())?;
        Some(Self {
            http,
            id,
            secret,
            token: Mutex::new(None),
        })
    }

    async fn bearer(&self) -> Result<String> {
        // Single-flight: мьютекс удерживается и на время запроса токена.
        let mut slot = self.token.lock().await;
        if let Some(cached) = slot.as_ref().filter(|t| {
            t.expires_at > Instant::now() + Duration::from_secs(30) && !t.value.is_empty()
        }) {
            return Ok(cached.value.clone());
        }
        let data: Value = self
            .http
            .post("https://accounts.spotify.com/api/token")
            .basic_auth(&self.id, Some(&self.secret))
            .form(&[("grant_type", "client_credentials")])
            .send()
            .await
            .map_err(|_| Error::Protocol("Spotify не выдал токен приложения".into()))?
            .error_for_status()
            .map_err(|_| Error::Protocol("Spotify отклонил ключи приложения".into()))?
            .json()
            .await
            .map_err(|_| Error::Protocol("Некорректный ответ Spotify".into()))?;
        let token = data["access_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Protocol("Spotify не вернул токен приложения".into()))?;
        let expires_in = data["expires_in"].as_u64().unwrap_or(3600).clamp(60, 3600);
        let value = token.to_owned();
        *slot = Some(CachedToken {
            value: value.clone(),
            expires_at: Instant::now() + Duration::from_secs(expires_in),
        });
        Ok(value)
    }

    async fn search_once(&self, query: &str, limit: u8) -> Result<reqwest::Response> {
        let token = self.bearer().await?;
        self.http
            .get("https://api.spotify.com/v1/search")
            .bearer_auth(token)
            .query(&[
                ("q", query),
                ("type", "track"),
                ("limit", &limit.clamp(1, 16).to_string()),
            ])
            .send()
            .await
            .map_err(|_| Error::Protocol("Поиск Spotify недоступен".into()))
    }

    /// Метаданные треков для сопоставления с каталогом. Звука здесь нет.
    pub async fn search_tracks(&self, query: &str, limit: u8) -> Result<Vec<TrackMetadata>> {
        let query = query.trim();
        if query.is_empty() || query.chars().count() > 140 {
            return Err(Error::Invalid("Поисковый запрос слишком длинный".into()));
        }
        let response = self.search_once(query, limit).await?;
        let response = if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            // Токен могли отозвать раньше expires_in: сбросить и повторить один раз.
            *self.token.lock().await = None;
            self.search_once(query, limit).await?
        } else {
            response
        };
        let data: Value = response
            .error_for_status()
            .map_err(|_| Error::Protocol("Поиск Spotify недоступен".into()))?
            .json()
            .await
            .map_err(|_| Error::Protocol("Некорректный ответ Spotify".into()))?;
        Ok(parse_tracks(&data))
    }
}

fn parse_tracks(data: &Value) -> Vec<TrackMetadata> {
    data["tracks"]["items"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let title = item["name"].as_str().filter(|s| !s.is_empty())?;
                    let artists: Vec<String> = item["artists"]
                        .as_array()?
                        .iter()
                        .filter_map(|a| a["name"].as_str())
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .collect();
                    if artists.is_empty() {
                        return None;
                    }
                    Some(TrackMetadata {
                        title: title.into(),
                        artists,
                        album: item["album"]["name"].as_str().map(str::to_owned),
                        duration_ms: item["duration_ms"].as_u64(),
                        isrc: item["external_ids"]["isrc"].as_str().map(str::to_owned),
                        source_url: item["id"]
                            .as_str()
                            .map(|id| format!("https://open.spotify.com/track/{id}")),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_track_search_results() {
        let data: Value = serde_json::from_str(
            r#"{"tracks":{"items":[
{"id":"abc","name":"Song","artists":[{"name":"Artist"}],"album":{"name":"Album"},"duration_ms":200000,"external_ids":{"isrc":"US123"}},
{"id":"bad","name":"","artists":[]}]}}"#,
        )
        .unwrap();
        let tracks = parse_tracks(&data);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].title, "Song");
        assert_eq!(tracks[0].artists, ["Artist"]);
        assert_eq!(
            tracks[0].source_url.as_deref(),
            Some("https://open.spotify.com/track/abc")
        );
    }
}
