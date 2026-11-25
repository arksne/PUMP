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
    collections::HashMap,
    time::{Duration, Instant},
};
use thiserror::Error;
use tokio::sync::Mutex;

const DEFAULT_API: &str = "https://api-v2.soundcloud.com";
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

/// Причина отрицательного кэша: только стабильные вердикты.
/// Временные сетевые сбои НЕ кэшируются, иначе один сбой на 5 минут
/// превратит живой трек в «не найден» для комнат и статистики.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NegReason {
    Missing,
    NoStream,
}

pub struct SoundCloudClient {
    http: reqwest::Client,
    fixed: Option<String>,
    api: String,
    id: Mutex<Option<CachedId>>,
    neg: Mutex<HashMap<String, (NegReason, Instant)>>,
}

const NEG_TTL: Duration = Duration::from_secs(5 * 60);
const NEG_CAP: usize = 512;

impl SoundCloudClient {
    pub fn new(http: reqwest::Client) -> Self {
        Self {
            http,
            fixed: None,
            api: api_base(),
            id: Mutex::default(),
            neg: Mutex::default(),
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
            api: api_base(),
            id: Mutex::default(),
            neg: Mutex::default(),
        }
    }

    async fn client_id(&self) -> Result<String, SourceError> {
        if let Some(fixed) = &self.fixed {
            return Ok(fixed.clone());
        }
        // Single-flight: мьютекс удерживается и на время discovery,
        // иначе толпа параллельных запросов устроит 50 независимых извлечений.
        let mut slot = self.id.lock().await;
        if let Some(cached) = slot.as_ref().filter(|id| id.at.elapsed() < ID_TTL) {
            return Ok(cached.value.clone());
        }
        // Суммарный лимит извлечения: страница + до 8 бандлов под 9-секундными
        // таймаутами клиента могли бы занять больше минуты без него.
        let id = tokio::time::timeout(Duration::from_secs(15), discover_id(&self.http))
            .await
            .map_err(|_| SourceError::Unavailable)??;
        slot.replace(CachedId {
            value: id.clone(),
            at: Instant::now(),
        });
        Ok(id)
    }

    async fn forget_id(&self) {
        self.id.lock().await.take();
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
            self.forget_id().await;
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
            if retry.status() == reqwest::StatusCode::NOT_FOUND {
                return Err(SourceError::NotFound);
            }
            return bounded_json(
                retry
                    .error_for_status()
                    .map_err(|_| SourceError::Unavailable)?,
            )
            .await;
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
                "{}/search?q={}&kind=track&limit={limit}",
                self.api,
                urlencoding(query)
            ))
            .await?;
        parse_search(&data)
    }

    async fn remember_negative(&self, id: &str, reason: NegReason) {
        let mut neg = self.neg.lock().await;
        neg.retain(|_, (_, at)| at.elapsed() < NEG_TTL);
        if neg.len() >= NEG_CAP
            && let Some(oldest) = neg
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(k, _)| k.clone())
        {
            neg.remove(&oldest);
        }
        neg.insert(id.into(), (reason, Instant::now()));
    }

    async fn negative(&self, id: &str) -> Option<NegReason> {
        self.neg
            .lock()
            .await
            .get(id)
            .filter(|(_, at)| at.elapsed() < NEG_TTL)
            .map(|(reason, _)| *reason)
    }

    /// Разрешение трека в прямой поток. Принимает числовой ID.
    pub async fn resolve(&self, id: &str) -> Result<SourceStream, SourceError> {
        if !valid_id(id) {
            return Err(SourceError::Invalid);
        }
        match self.negative(id).await {
            Some(NegReason::Missing) => return Err(SourceError::NotFound),
            Some(NegReason::NoStream) => return Err(SourceError::NoStream),
            None => {}
        }
        let track_id: u64 = id.parse().map_err(|_| SourceError::Invalid)?;
        let data = match self
            .get_json(&format!("{}/tracks/{track_id}", self.api))
            .await
        {
            Ok(data) => data,
            Err(SourceError::NotFound) => {
                self.remember_negative(id, NegReason::Missing).await;
                return Err(SourceError::NotFound);
            }
            Err(error) => return Err(error),
        };
        match self.stream_from_track(&data).await {
            Ok(stream) => Ok(stream),
            Err(SourceError::Unavailable) => Err(SourceError::Unavailable),
            Err(error) => {
                self.remember_negative(id, NegReason::NoStream).await;
                Err(error)
            }
        }
    }

    /// Прямая ссылка вида soundcloud.com/артист/трек: сначала resolve в ID.
    /// Возвращает метаданные и свежий поток.
    pub async fn resolve_url(
        &self,
        link: &str,
    ) -> Result<(SourceTrack, SourceStream), SourceError> {
        let link = canonical_track_link(link).ok_or(SourceError::Invalid)?;
        let data = self
            .get_json(&format!("{}/resolve?url={}", self.api, urlencoding(&link)))
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
        if !valid_id(id) {
            return Err(SourceError::Invalid);
        }
        match self.negative(id).await {
            Some(NegReason::Missing) => return Err(SourceError::NotFound),
            Some(NegReason::NoStream) => return Err(SourceError::NoStream),
            None => {}
        }
        let track_id: u64 = id.parse().map_err(|_| SourceError::Invalid)?;
        let data = self
            .get_json(&format!("{}/tracks/{track_id}", self.api))
            .await?;
        // Временные сетевые сбои не кэшируем — только итог «трека нет».
        match parse_track(&data) {
            Some(track) if track.playback != Playback::Blocked => Ok(track),
            _ => {
                self.remember_negative(id, NegReason::Missing).await;
                Err(SourceError::NotFound)
            }
        }
    }

    async fn stream_from_track(&self, data: &Value) -> Result<SourceStream, SourceError> {
        let track = parse_track(data).ok_or(SourceError::NotFound)?;
        if track.playback == Playback::Blocked {
            return Err(SourceError::NoStream);
        }
        let endpoint = progressive_url(data, &self.api).ok_or(SourceError::NoStream)?;
        let id = self.client_id().await?;
        let response = self
            .http
            .get(&endpoint)
            .query(&[("client_id", id.as_str())])
            .header("Accept", "application/json")
            .header("User-Agent", BROWSER_UA)
            .send()
            .await
            .map_err(|_| SourceError::Unavailable)?
            .error_for_status()
            .map_err(|_| SourceError::Unavailable)?;
        // Ответ resolve короткий; большой ответ отклоняется до парсинга.
        let mut bytes = Vec::new();
        let mut response = response;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| SourceError::Unavailable)?
        {
            if bytes.len() + chunk.len() > 65536 {
                return Err(SourceError::Unavailable);
            }
            bytes.extend_from_slice(&chunk);
        }
        let data: Value = serde_json::from_slice(&bytes).map_err(|_| SourceError::Unavailable)?;
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

/// База api-v2 с явной точкой переопределения для тестов и зеркал.
/// `SOUNDCLOUD_API_BASE` принимает только https либо http на loopback;
/// невалидное значение тихо заменяется адресом по умолчанию.
fn api_base() -> String {
    let raw = std::env::var("SOUNDCLOUD_API_BASE").unwrap_or_default();
    let raw = raw.trim();
    if raw.is_empty() {
        return DEFAULT_API.into();
    }
    let parsed = url::Url::parse(raw).ok().filter(|u| {
        u.username().is_empty()
            && u.password().is_none()
            && u.query().is_none()
            && u.fragment().is_none()
            && matches!(u.path(), "" | "/")
            && (u.scheme() == "https" || (u.scheme() == "http" && is_loopback(u)))
    });
    match parsed {
        Some(u) => u.to_string().trim_end_matches('/').to_owned(),
        None => {
            tracing::warn!("Некорректный SOUNDCLOUD_API_BASE, используется адрес по умолчанию");
            DEFAULT_API.into()
        }
    }
}

fn is_loopback(u: &url::Url) -> bool {
    matches!(
        u.host_str(),
        Some("localhost") | Some("127.0.0.1") | Some("::1")
    )
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
        // Строгий контекст: `client_id` + пробелы + `:`/`=` + пробелы/кавычки.
        let sep = after.trim_start_matches([' ', '\t']);
        let Some(sep) = sep.strip_prefix([':', '=']) else {
            rest = &after[1.min(after.len())..];
            continue;
        };
        let value = sep.trim_start_matches([' ', '\t', '"', '\'']);
        if value.len() >= 32 {
            let id: String = value.chars().take(32).collect();
            let terminator_ok = value
                .chars()
                .nth(32)
                .is_none_or(|c| !c.is_ascii_alphanumeric());
            if id.bytes().all(|b| b.is_ascii_alphanumeric()) && terminator_ok {
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
        "ALLOW" => match item.get("media").and_then(|m| m.get("transcodings")) {
            // Без списка транскодингов считаем запись полной —
            // окончательное слово за resolve (или его ошибкой).
            None => Playback::Full,
            Some(t) if has_protocol(t, "progressive") => Playback::Full,
            Some(t) if has_protocol(t, "hls") => Playback::HlsOnly,
            _ => Playback::Blocked,
        },
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

fn has_protocol(transcodings: &Value, protocol: &str) -> bool {
    transcodings
        .as_array()
        .is_some_and(|t| t.iter().any(|t| t["format"]["protocol"] == protocol))
}

fn progressive_url(item: &Value, base: &str) -> Option<String> {
    item["media"]["transcodings"]
        .as_array()?
        .iter()
        .find_map(|t| {
            let url = t["url"].as_str()?;
            (t["format"]["protocol"] == "progressive" && check_api_url(url, base))
                .then_some(url.to_owned())
        })
}

fn check_api_url(url: &str, base: &str) -> bool {
    let Ok(endpoint) = url::Url::parse(url) else {
        return false;
    };
    let Ok(origin) = url::Url::parse(base) else {
        return false;
    };
    endpoint.scheme() == origin.scheme()
        && endpoint.host_str() == origin.host_str()
        && endpoint.port_or_known_default() == origin.port_or_known_default()
        && endpoint.username().is_empty()
        && endpoint.password().is_none()
        && endpoint.query().is_none()
        && endpoint.fragment().is_none()
        && endpoint.path().starts_with("/media/")
}

/// Подписанные CDN-ссылки живут только на хостах SoundCloud
/// (http разрешён лишь на loopback для локальных стендов со своим
/// SOUNDCLOUD_API_BASE и в юнит-тестах; в проде без переопределения — нет).
pub fn valid_stream_url(url: &str) -> bool {
    url::Url::parse(url).ok().is_some_and(|u| {
        u.username().is_empty()
            && u.password().is_none()
            && ((u.scheme() == "https"
                && u.port().is_none()
                && u.host_str()
                    .is_some_and(|h| h == "sndcdn.com" || h.ends_with(".sndcdn.com")))
                || (u.scheme() == "http" && is_loopback(&u) && http_loopback_allowed()))
    })
}

fn http_loopback_allowed() -> bool {
    if cfg!(test) {
        return true;
    }
    !std::env::var("SOUNDCLOUD_API_BASE")
        .unwrap_or_default()
        .trim()
        .is_empty()
}

fn valid_media_url(url: &str) -> bool {
    url.len() <= 1024
        && url::Url::parse(url).ok().is_some_and(|u| {
            u.scheme() == "https"
                && u.username().is_empty()
                && u.password().is_none()
                && u.port().is_none()
                && u.fragment().is_none()
                && u.host_str()
                    .is_some_and(|h| h == "sndcdn.com" || h.ends_with(".sndcdn.com"))
        })
}

/// Числовой ID SoundCloud: до 19 цифр, парсится в u64.
/// Единая проверка для поиска, треков и прокси.
pub fn valid_id(numeric: &str) -> bool {
    !numeric.is_empty() && numeric.len() <= 19 && numeric.bytes().all(|b| b.is_ascii_digit())
}

/// Числовой ID трека. Полные ссылки обрабатывает `resolve_url`.
pub fn track_id_from_link(link: &str) -> Option<String> {
    let link = link.trim();
    valid_id(link).then_some(link.into())
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
        assert!(!valid_stream_url(
            "https://user:pass@cf-media.sndcdn.com/x.mp3"
        ));
        assert!(valid_stream_url("http://127.0.0.1:8080/cdn/x.mp3"));
        assert!(!valid_stream_url("http://127.0.0.1:8080.evil.test/x.mp3"));
        assert!(!valid_stream_url("https://127.0.0.1/x.mp3"));
        assert!(valid_id("424242"));
        assert!(!valid_id(""));
        assert!(!valid_id("12a34"));
        assert_eq!(track_id_from_link("123456789"), Some("123456789".into()));
        assert_eq!(
            track_id_from_link("1234567890123456789").as_deref(),
            Some("1234567890123456789")
        );
        assert!(track_id_from_link("12345678901234567890").is_none());
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
{"id":3,"title":"Blocked","user":{"username":"C"},"policy":"BLOCK","media":{}},
{"id":4,"title":"Sparse","user":{"username":"D"},"policy":"ALLOW"},
{"id":5,"title":"Empty","user":{"username":"E"},"policy":"ALLOW","media":{"transcodings":[]}}]}"#,
        )
        .unwrap();
        let tracks = parse_search(&data).unwrap();
        assert_eq!(tracks.len(), 5);
        assert_eq!(tracks[0].playback, Playback::Full);
        assert_eq!(
            tracks[0].artwork.as_deref(),
            Some("https://i1.sndcdn.com/art.jpg")
        );
        assert_eq!(tracks[1].playback, Playback::HlsOnly);
        assert_eq!(tracks[2].playback, Playback::Blocked);
        assert_eq!(tracks[3].playback, Playback::Full);
        assert_eq!(tracks[4].playback, Playback::Blocked);
        assert!(progressive_url(&data["collection"][0], DEFAULT_API).is_some());
        assert!(progressive_url(&data["collection"][1], DEFAULT_API).is_none());
    }
    #[test]
    fn api_base_and_hosts_are_constrained() {
        assert!(check_api_url(
            "https://api-v2.soundcloud.com/media/x/stream/progressive",
            DEFAULT_API
        ));
        assert!(!check_api_url(
            "https://api-v2.soundcloud.com:8443/media/x",
            DEFAULT_API
        ));
        assert!(!check_api_url(
            "https://user@api-v2.soundcloud.com/media/x",
            DEFAULT_API
        ));
        assert!(!check_api_url(
            "https://api-v2.soundcloud.com/other/x",
            DEFAULT_API
        ));
        assert!(!check_api_url(
            "https://api-v2.soundcloud.com/media/x?client_id=y#frag",
            DEFAULT_API
        ));
        assert!(valid_stream_url("http://127.0.0.1:8080/cdn/x.mp3"));
        assert!(!valid_stream_url("http://evil.test/x.mp3"));
        assert!(!valid_stream_url("https://127.0.0.1/x.mp3"));
        assert!(valid_media_url("https://i1.sndcdn.com/art.jpg"));
        assert!(!valid_media_url("http://i1.sndcdn.com/art.jpg"));
    }
}
