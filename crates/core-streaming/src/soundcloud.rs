//! Поиск и разрешение публичных записей SoundCloud через api-v2.
//!
//! Протокол повторяет поведение веб-плеера SoundCloud и описан в открытом
//! каталоге https://github.com/KlugesBuch1/soundcloud-protocol (MIT) и в
//! документации Resona API: каждый запрос несёт `client_id`, который лежит
//! открытым текстом в JS-бандлах soundcloud.com и периодически ротируется.
//! Поэтому ID не хранится в коде: он извлекается при запуске и обновляется
//! после первого ответа 401. Переменная `SOUNDCLOUD_CLIENT_ID` задаёт
//! готовый ID вручную — полезно, когда сам сайт недоступен, а api-v2 открыт.
//!
//! Честные ограничения: прямые ссылки выдаются только для транскодинга
//! `progressive`. HLS-потоки и записи с политикой, отличной от ALLOW,
//! помечаются недоступными — обход платных ограничений не выполняется.
//! Временные CDN-ссылки не кэшируются: они получаются перед воспроизведением.
use crate::ApiError;
use serde_json::Value;
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};
use thiserror::Error;

const API: &str = "https://api-v2.soundcloud.com";
const HOME: &str = "https://soundcloud.com/";
const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";
const ID_TTL: Duration = Duration::from_secs(24 * 3600);
const MAX_HOME_BYTES: usize = 2 * 1024 * 1024;
const MAX_ASSET_BYTES: usize = 4 * 1024 * 1024;
const MAX_ASSETS: usize = 8;
const MAX_API_BYTES: usize = 1024 * 1024;

#[derive(Debug, Error)]
pub enum SourceError {
    #[error("некорректный запрос SoundCloud")]
    Invalid,
    #[error("запись SoundCloud не найдена")]
    NotFound,
    #[error("SoundCloud недоступен")]
    Unavailable,
    #[error("для записи нет открытого аудиопотока")]
    NoStream,
}

impl From<SourceError> for ApiError {
    fn from(error: SourceError) -> Self {
        match error {
            SourceError::Invalid => ApiError::invalid("Некорректный запрос SoundCloud."),
            SourceError::NotFound => ApiError::missing("Запись SoundCloud не найдена."),
            SourceError::Unavailable => {
                ApiError::unavailable("SoundCloud недоступен. Проверь сеть или прокси.")
            }
            SourceError::NoStream => {
                ApiError::unavailable("Для записи нет открытого аудиопотока SoundCloud.")
            }
        }
    }
}

/// Доступность воспроизведения без обхода ограничений источника.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Playback {
    Full,
    HlsOnly,
    Blocked,
}

#[derive(Clone, Debug)]
pub struct SourceTrack {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub artwork: Option<String>,
    pub duration_ms: u64,
    pub playback: Playback,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Protocol {
    Progressive,
    Hls,
}

#[derive(Clone, Debug)]
pub struct SourceStream {
    pub url: String,
    pub protocol: Protocol,
    pub mime_type: String,
}

struct CachedId {
    value: String,
    at: Instant,
}

pub struct SoundCloudClient {
    http: reqwest::Client,
    fixed: Option<String>,
    id: Mutex<Option<CachedId>>,
}

impl SoundCloudClient {
    pub fn new(http: reqwest::Client) -> Self {
        Self {
            http,
            fixed: None,
            id: Mutex::default(),
        }
    }

    /// Готовый ID из настроек вместо автоматического извлечения.
    pub fn from_env(http: reqwest::Client) -> Self {
        let fixed = std::env::var("SOUNDCLOUD_CLIENT_ID")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .and_then(|v| valid_client_id(&v));
        Self {
            http,
            fixed,
            id: Mutex::default(),
        }
    }

    async fn client_id(&self) -> Result<String, SourceError> {
        if let Some(fixed) = &self.fixed {
            return Ok(fixed.clone());
        }
        if let Some(cached) = self
            .id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .filter(|id| id.at.elapsed() < ID_TTL)
        {
            return Ok(cached.value.clone());
        }
        let id = discover_id(&self.http).await?;
        self.id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(CachedId {
                value: id.clone(),
                at: Instant::now(),
            });
        Ok(id)
    }

    fn forget_id(&self) {
        self.id.lock().unwrap_or_else(|e| e.into_inner()).take();
    }

    async fn get_json(&self, url: &str) -> Result<Value, SourceError> {
        let id = self.client_id().await?;
        let response = self
            .http
            .get(url)
            .query(&[("client_id", id.as_str())])
            .header("Accept", "application/json")
            .header("User-Agent", BROWSER_UA)
            .send()
            .await
            .map_err(|_| SourceError::Unavailable)?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED && self.fixed.is_none() {
            self.forget_id();
            let id = self.client_id().await?;
            let retry = self
                .http
                .get(url)
                .query(&[("client_id", id.as_str())])
                .header("Accept", "application/json")
                .header("User-Agent", BROWSER_UA)
                .send()
                .await
                .map_err(|_| SourceError::Unavailable)?;
            return bounded_json(retry).await;
        }
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(SourceError::NotFound);
        }
        bounded_json(
            response
                .error_for_status()
                .map_err(|_| SourceError::Unavailable)?,
        )
        .await
    }

    /// Поиск треков. `limit` ограничен сверху для защиты источника.
    pub async fn search(&self, query: &str, limit: u8) -> Result<Vec<SourceTrack>, SourceError> {
        let query = query.trim();
        if query.is_empty() || query.chars().count() > 140 {
            return Err(SourceError::Invalid);
        }
        let limit = limit.clamp(1, 16);
        let data = self
            .get_json(&format!(
                "{API}/search?q={}&kind=track&limit={limit}",
                urlencoding(query)
            ))
            .await?;
        parse_search(&data)
    }

    /// Разрешение трека в прямой поток. Принимает числовой ID.
    pub async fn resolve(&self, id: &str) -> Result<SourceStream, SourceError> {
        let track_id: u64 = id.parse().map_err(|_| SourceError::Invalid)?;
        let data = self.get_json(&format!("{API}/tracks/{track_id}")).await?;
        self.stream_from_track(&data).await
    }

    /// Прямая ссылка вида soundcloud.com/артист/трек: сначала resolve в ID.
    /// Возвращает метаданные и свежий поток.
    pub async fn resolve_url(
        &self,
        link: &str,
    ) -> Result<(SourceTrack, SourceStream), SourceError> {
        let link = canonical_track_link(link).ok_or(SourceError::Invalid)?;
        let data = self
            .get_json(&format!("{API}/resolve?url={}", urlencoding(&link)))
            .await?;
        if data["kind"] != "track" {
            return Err(SourceError::NotFound);
        }
        let track = parse_track(&data).ok_or(SourceError::NotFound)?;
        let stream = self.stream_from_track(&data).await?;
        Ok((track, stream))
    }

    /// Метаданные трека по числовому ID без получения потока.
    pub async fn track(&self, id: &str) -> Result<SourceTrack, SourceError> {
        let track_id: u64 = id.parse().map_err(|_| SourceError::Invalid)?;
        let data = self.get_json(&format!("{API}/tracks/{track_id}")).await?;
        parse_track(&data).ok_or(SourceError::NotFound)
    }

    async fn stream_from_track(&self, data: &Value) -> Result<SourceStream, SourceError> {
        let track = parse_track(data).ok_or(SourceError::NotFound)?;
        if track.playback == Playback::Blocked {
            return Err(SourceError::NoStream);
        }
        let endpoint = progressive_url(data).ok_or(SourceError::NoStream)?;
        let id = self.client_id().await?;
        let data: Value = self
            .http
            .get(&endpoint)
            .query(&[("client_id", id.as_str())])
            .header("Accept", "application/json")
            .header("User-Agent", BROWSER_UA)
            .send()
            .await
            .map_err(|_| SourceError::Unavailable)?
            .error_for_status()
            .map_err(|_| SourceError::Unavailable)?
            .json()
            .await
            .map_err(|_| SourceError::Unavailable)?;
        let url = data["url"].as_str().ok_or(SourceError::NoStream)?;
        if !valid_stream_url(url) {
            return Err(SourceError::NoStream);
        }
        Ok(SourceStream {
            url: url.into(),
            protocol: Protocol::Progressive,
            mime_type: "audio/mpeg".into(),
        })
    }
}

fn valid_client_id(value: &str) -> Option<String> {
    let value = value.trim().to_owned();
    (value.len() == 32 && value.bytes().all(|b| b.is_ascii_alphanumeric())).then_some(value)
}

/// Извлечение client_id из главной страницы и JS-бандлов плеера.
async fn discover_id(http: &reqwest::Client) -> Result<String, SourceError> {
    let mut home = http
        .get(HOME)
        .header("User-Agent", BROWSER_UA)
        .send()
        .await
        .map_err(|_| SourceError::Unavailable)?
        .error_for_status()
        .map_err(|_| SourceError::Unavailable)?;
    let mut html = Vec::new();
    while let Some(chunk) = home.chunk().await.map_err(|_| SourceError::Unavailable)? {
        if html.len() + chunk.len() > MAX_HOME_BYTES {
            return Err(SourceError::Unavailable);
        }
        html.extend_from_slice(&chunk);
    }
    let html = String::from_utf8(html).map_err(|_| SourceError::Unavailable)?;
    let mut assets = Vec::new();
    let mut rest = html.as_str();
    while let Some(start) = rest.find("https://a-v2.sndcdn.com/assets/") {
        let url = &rest[start..];
        let end = url
            .find(".js")
            .map(|i| i + 3)
            .filter(|end| {
                url.get(..*end)
                    .is_some_and(|u| !u.contains(['"', '\'', ' ', '<']))
            })
            .unwrap_or(0);
        if end > 0 {
            assets.push(url[..end].to_owned());
        }
        rest = &url[1.min(url.len())..];
        if assets.len() >= MAX_ASSETS {
            break;
        }
    }
    for asset in assets.iter().rev() {
        let mut response = http
            .get(asset)
            .header("User-Agent", BROWSER_UA)
            .send()
            .await
            .map_err(|_| SourceError::Unavailable)?;
        let mut code = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| SourceError::Unavailable)?
        {
            if code.len() + chunk.len() > MAX_ASSET_BYTES {
                break;
            }
            code.extend_from_slice(&chunk);
        }
        if let Some(id) = extract_client_id(&code) {
            return Ok(id);
        }
    }
    Err(SourceError::Unavailable)
}

fn extract_client_id(code: &[u8]) -> Option<String> {
    let code = std::str::from_utf8(code).ok()?;
    let mut rest = code;
    while let Some(i) = rest.find("client_id") {
        let after = &rest[i + "client_id".len()..];
        let value = after
            .trim_start_matches([' ', '\t', '=', ':'])
            .trim_start_matches([' ', '\t', '"', '\'']);
        if value.len() >= 32 {
            let id: String = value.chars().take(32).collect();
            if id.bytes().all(|b| b.is_ascii_alphanumeric()) {
                return Some(id);
            }
        }
        rest = &after[1.min(after.len())..];
    }
    None
}

async fn bounded_json(response: reqwest::Response) -> Result<Value, SourceError> {
    let mut bytes = Vec::new();
    let mut response = response;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| SourceError::Unavailable)?
    {
        if bytes.len() + chunk.len() > MAX_API_BYTES {
            return Err(SourceError::Unavailable);
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| SourceError::Unavailable)
}

fn parse_search(data: &Value) -> Result<Vec<SourceTrack>, SourceError> {
    let items = data["collection"]
        .as_array()
        .ok_or(SourceError::Unavailable)?;
    Ok(items.iter().filter_map(parse_track).collect())
}

fn parse_track(item: &Value) -> Option<SourceTrack> {
    let id = item["id"].as_u64()?;
    let title = item["title"].as_str().filter(|s| !s.is_empty())?;
    let artist = item["user"]["username"].as_str().unwrap_or("").to_owned();
    let artwork = item["artwork_url"]
        .as_str()
        .filter(|url| valid_media_url(url))
        .map(str::to_owned);
    let duration_ms = item["full_duration"]
        .as_u64()
        .or_else(|| item["duration"].as_u64())
        .unwrap_or(0);
    let playback = match item["policy"].as_str().unwrap_or("ALLOW") {
        "ALLOW" => {
            if has_progressive(item) {
                Playback::Full
            } else if has_hls(item) {
                Playback::HlsOnly
            } else {
                Playback::Full
            }
        }
        _ => Playback::Blocked,
    };
    Some(SourceTrack {
        id: id.to_string(),
        title: title.into(),
        artist,
        artwork,
        duration_ms,
        playback,
    })
}

fn has_progressive(item: &Value) -> bool {
    item["media"]["transcodings"]
        .as_array()
        .is_some_and(|t| t.iter().any(|t| t["format"]["protocol"] == "progressive"))
}

fn has_hls(item: &Value) -> bool {
    item["media"]["transcodings"]
        .as_array()
        .is_some_and(|t| t.iter().any(|t| t["format"]["protocol"] == "hls"))
}

fn progressive_url(item: &Value) -> Option<String> {
    item["media"]["transcodings"]
        .as_array()?
        .iter()
        .find_map(|t| {
            let url = t["url"].as_str()?;
            (t["format"]["protocol"] == "progressive" && valid_api_url(url))
                .then_some(url.to_owned())
        })
}

fn valid_api_url(url: &str) -> bool {
    url::Url::parse(url).ok().is_some_and(|u| {
        u.scheme() == "https"
            && u.host_str() == Some("api-v2.soundcloud.com")
            && u.path().starts_with("/media/")
    })
}

/// Подписанные CDN-ссылки живут только на хостах SoundCloud.
pub fn valid_stream_url(url: &str) -> bool {
    url::Url::parse(url).ok().is_some_and(|u| {
        u.scheme() == "https"
            && u.host_str()
                .is_some_and(|h| h == "sndcdn.com" || h.ends_with(".sndcdn.com"))
    })
}

fn valid_media_url(url: &str) -> bool {
    url.len() <= 1024 && valid_stream_url(url)
}

/// Числовой ID трека. Полные ссылки обрабатывает `resolve_url`.
pub fn track_id_from_link(link: &str) -> Option<String> {
    let link = link.trim();
    (!link.is_empty() && link.len() <= 12 && link.bytes().all(|b| b.is_ascii_digit()))
        .then_some(link.into())
}

/// Каноническая ссылка на трек для `/resolve`. Короткие ссылки
/// on.soundcloud.com не поддерживаются: клиент не следует редиректам
/// с секретами, а сервер не принимает произвольные URL.
pub fn canonical_track_link(link: &str) -> Option<String> {
    let url = url::Url::parse(link.trim()).ok()?;
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    match url.host_str()? {
        "soundcloud.com" | "www.soundcloud.com" | "m.soundcloud.com" => {
            let mut parts = url.path().split('/').filter(|s| !s.is_empty());
            if parts.next().is_none() || parts.next().is_none() || parts.next().is_some() {
                return None;
            }
            let mut clean = url.clone();
            clean.set_query(None);
            clean.set_fragment(None);
            Some(clean.to_string())
        }
        _ => None,
    }
}

fn urlencoding(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn extracts_client_id_from_bundle() {
        let code = b"var x={client_id:\"abcdefghijklmnopqrstuvwxyz012345\"};";
        assert_eq!(
            extract_client_id(code).as_deref(),
            Some("abcdefghijklmnopqrstuvwxyz012345")
        );
        assert!(extract_client_id(b"no id here").is_none());
    }
    #[test]
    fn validates_stream_and_link_hosts() {
        assert!(valid_stream_url(
            "https://cf-media.sndcdn.com/abc/123.mp3?Policy=x&Signature=y"
        ));
        assert!(!valid_stream_url("https://evil.test/x.mp3"));
        assert!(!valid_stream_url("http://cf-media.sndcdn.com/x.mp3"));
        assert_eq!(track_id_from_link("123456789"), Some("123456789".into()));
        assert_eq!(
            canonical_track_link("https://soundcloud.com/artist/track-name?si=abc").as_deref(),
            Some("https://soundcloud.com/artist/track-name")
        );
        assert!(canonical_track_link("https://soundcloud.com/artist").is_none());
        assert!(track_id_from_link("https://on.soundcloud.com/abc").is_none());
        assert!(track_id_from_link("https://evil.test/a/b").is_none());
    }
    #[test]
    fn parses_search_blocked_and_hls_tracks() {
        let data: Value = serde_json::from_str(
            r#"{"collection":[
{"id":1,"title":"Free","user":{"username":"A"},"artwork_url":"https://i1.sndcdn.com/art.jpg","full_duration":200000,"policy":"ALLOW","media":{"transcodings":[{"url":"https://api-v2.soundcloud.com/media/x/stream/progressive","format":{"protocol":"progressive"}}]}},
{"id":2,"title":"Go+","user":{"username":"B"},"policy":"ALLOW","media":{"transcodings":[{"url":"https://api-v2.soundcloud.com/media/y/stream/hls","format":{"protocol":"hls"}}]}},
{"id":3,"title":"Blocked","user":{"username":"C"},"policy":"BLOCK","media":{}}]}"#,
        )
        .unwrap();
        let tracks = parse_search(&data).unwrap();
        assert_eq!(tracks.len(), 3);
        assert_eq!(tracks[0].playback, Playback::Full);
        assert_eq!(
            tracks[0].artwork.as_deref(),
            Some("https://i1.sndcdn.com/art.jpg")
        );
        assert_eq!(tracks[1].playback, Playback::HlsOnly);
        assert_eq!(tracks[2].playback, Playback::Blocked);
        assert!(progressive_url(&data["collection"][0]).is_some());
        assert!(progressive_url(&data["collection"][1]).is_none());
    }
}
