//! Поиск по файлам, полным демотрекам, превью iTunes и каталогу YouTube.
//! Тип источника хранится отдельно. Клиент не принимает превью за полную запись.

mod error;
pub mod innertube;
pub mod network;
pub use error::ApiError;
pub const MAX_TRACK_MS: u64 = 24 * 60 * 60 * 1000;
use anyhow::{Context, Result};
use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, Method, StatusCode, header},
    response::Response,
};
use lofty::{
    file::{AudioFile, TaggedFileExt},
    probe::Probe,
    tag::Accessor,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::sync::RwLock;
use url::Url;

// Обложки сохранены из исходного прототипа, чтобы не зависеть от доступа браузера к CDN.
const DEMO_COVERS: [&[u8]; 3] = [
    include_bytes!("../../../web/covers/demo-1.jpg"),
    include_bytes!("../../../web/covers/demo-2.jpg"),
    include_bytes!("../../../web/covers/demo-3.jpg"),
];

fn demo_number(id: &str) -> Option<usize> {
    match id {
        "soundhelix-1" => Some(1),
        "soundhelix-2" => Some(2),
        "soundhelix-3" => Some(3),
        _ => None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Full,
    Preview,
    Youtube,
}

/// Ссылка на исходную запись позволяет обновлять временный поток дополнения.
#[derive(Clone, Serialize)]
pub struct ProviderRef {
    pub provider_id: String,
    pub track_id: String,
}

#[derive(Clone, Serialize)]
pub struct Track {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub kind: Kind,
    pub duration_ms: u64,
    pub audio_url: Option<String>,
    pub artwork: Option<String>,
    pub video_id: Option<String>,
    pub source: &'static str,
    pub attribution: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderRef>,
}

#[derive(Clone)]
struct LocalFile {
    path: PathBuf,
    cover: Option<(&'static str, Bytes)>,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    #[default]
    All,
    Full,
    Preview,
    Youtube,
}

#[derive(Default, Deserialize)]
pub struct SearchQuery {
    #[serde(default)]
    pub q: String,
    #[serde(default)]
    pub source: Source,
}

#[derive(Clone, Serialize)]
pub struct Search {
    pub tracks: Vec<Track>,
    pub notice: Option<&'static str>,
}

#[derive(Default, Serialize)]
pub struct Lyrics {
    synced: Option<String>,
    plain: Option<String>,
}

type SearchCache = HashMap<(u8, String), (Instant, Vec<Track>)>;
type LookupCache = HashMap<String, (Instant, Track)>;

pub struct Catalog {
    full: Vec<Track>,
    local: HashMap<String, LocalFile>,
    fixed: HashMap<String, Track>,
    http: reqwest::Client,
    youtube_key: Option<String>,
    auth: Option<innertube::SessionAuth>,
    streams: RwLock<HashMap<String, innertube::Streams>>,
    queries: RwLock<SearchCache>,
    lookup: RwLock<LookupCache>,
}

impl Catalog {
    pub async fn load(
        dir: Option<PathBuf>,
        http: reqwest::Client,
        youtube_key: Option<String>,
    ) -> Result<Self> {
        let (mut full, local) = match dir {
            Some(dir) => tokio::task::spawn_blocking(move || scan(&dir)).await??,
            None => (Vec::new(), HashMap::new()),
        };
        full.extend((1..=DEMO_COVERS.len()).map(|n| Track {
            id: format!("soundhelix-{n}"),
            title: format!("SoundHelix Song {n}"),
            artist: "SoundHelix".into(),
            kind: Kind::Full,
            duration_ms: 0,
            audio_url: Some(format!("/media/soundhelix-{n}.mp3")),
            artwork: Some(format!("/covers/soundhelix-{n}")),
            video_id: None,
            source: "SoundHelix",
            attribution: Some("Music by SoundHelix — https://www.soundhelix.com"),
            provider: None,
        }));
        let fixed = full
            .iter()
            .map(|track| (track.id.clone(), track.clone()))
            .collect();
        Ok(Self {
            full,
            local,
            fixed,
            http,
            youtube_key,
            auth: None,
            streams: RwLock::default(),
            queries: RwLock::default(),
            lookup: RwLock::default(),
        })
    }

    pub fn with_session(mut self, cookies: Option<String>) -> anyhow::Result<Self> {
        self.auth = cookies
            .map(innertube::SessionAuth::from_cookies)
            .transpose()?;
        Ok(self)
    }

    pub async fn streams(
        &self,
        id: &str,
        options: &innertube::StreamOptions,
    ) -> Result<innertube::Streams, ApiError> {
        let video = id
            .strip_prefix("yt-")
            .filter(|id| valid_video(id))
            .ok_or_else(|| ApiError::invalid("Для потоков требуется видео YouTube."))?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let cached = self
            .streams
            .read()
            .await
            .get(id)
            .filter(|streams| streams.expires_at > now)
            .cloned();
        let all = match cached {
            Some(streams) => streams,
            None => {
                let streams = innertube::player(&self.http, video, self.auth.as_ref()).await?;
                let mut cache = self.streams.write().await;
                cache.retain(|_, value| value.expires_at > now);
                if cache.len() >= 256 {
                    cache.clear();
                }
                cache.insert(id.to_owned(), streams.clone());
                streams
            }
        };
        let streams = all.select(options);
        if streams.is_empty() {
            return Err(ApiError::unavailable(
                "Источник не вернул формат для выбранного режима.",
            ));
        }
        Ok(innertube::Streams {
            streams,
            expires_at: all.expires_at,
        })
    }

    pub async fn stream(
        &self,
        id: &str,
        format_id: u64,
        method: Method,
        headers: &HeaderMap,
    ) -> Result<Response, ApiError> {
        let _ = self.streams(id, &innertube::StreamOptions::default()).await;
        let stream = self
            .streams
            .read()
            .await
            .get(id)
            .and_then(|all| all.streams.iter().find(|item| item.format_id == format_id))
            .cloned()
            .ok_or_else(|| ApiError::missing("Поток не найден. Повтори получение форматов."))?;
        let mut request = self
            .http
            .request(method, &stream.upstream)
            .timeout(Duration::from_secs(120));
        if let Some(range) = headers.get(header::RANGE) {
            request = request.header(header::RANGE, range);
        }
        let upstream = request
            .send()
            .await
            .map_err(|_| ApiError::unavailable("Не удалось открыть поток."))?;
        let status = upstream.status();
        if !status.is_success() && status != StatusCode::RANGE_NOT_SATISFIABLE {
            self.streams.write().await.remove(id);
            return Err(ApiError::unavailable(
                "Формат недоступен. Выбери резервный поток.",
            ));
        }
        let mut response = Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, &stream.mime_type)
            .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
        for name in [
            header::CONTENT_LENGTH,
            header::CONTENT_RANGE,
            header::ACCEPT_RANGES,
        ] {
            if let Some(value) = upstream.headers().get(&name) {
                response = response.header(name, value);
            }
        }
        response
            .body(Body::from_stream(upstream.bytes_stream()))
            .map_err(|_| ApiError::unavailable("Некорректный ответ источника."))
    }

    /// Временные записи NAS и провайдеров доступны комнатам и статистике по стабильному ID.
    pub async fn register_track(&self, track: Track) -> Result<(), ApiError> {
        if track.id.is_empty()
            || track.id.len() > 64
            || track.title.len() > 2048
            || track.artist.len() > 2048
        {
            return Err(ApiError::invalid("Некорректные метаданные трека."));
        }
        let mut lookup = self.lookup.write().await;
        lookup.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(6 * 3600));
        if lookup.len() >= 8192 {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "Каталог временных записей заполнен.",
            ));
        }
        lookup.insert(track.id.clone(), (Instant::now(), track));
        Ok(())
    }

    pub fn local_count(&self) -> usize {
        self.local.len()
    }
    pub fn media(&self, id: &str) -> Option<PathBuf> {
        self.local.get(id).map(|item| item.path.clone())
    }
    pub fn cover(&self, id: &str) -> Option<(&'static str, Bytes)> {
        if let Some(n) = demo_number(id) {
            return Some(("image/jpeg", Bytes::from_static(DEMO_COVERS[n - 1])));
        }
        self.local.get(id)?.cover.clone()
    }

    pub async fn demo_audio(
        &self,
        id: &str,
        method: Method,
        headers: &HeaderMap,
    ) -> Result<Response, ApiError> {
        let n = demo_number(id).ok_or_else(|| ApiError::missing("Аудиофайл не найден."))?;
        let mut request = self
            .http
            .request(
                method,
                format!("https://www.soundhelix.com/examples/mp3/SoundHelix-Song-{n}.mp3"),
            )
            .timeout(Duration::from_secs(120));
        if let Some(range) = headers.get(header::RANGE) {
            request = request.header(header::RANGE, range);
        }
        let upstream = request
            .send()
            .await
            .map_err(|_| ApiError::unavailable("SoundHelix сейчас недоступен."))?;
        let status = upstream.status();
        if !status.is_success() && status != StatusCode::RANGE_NOT_SATISFIABLE {
            return Err(ApiError::unavailable("SoundHelix сейчас недоступен."));
        }
        let mut response = Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "audio/mpeg")
            .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
        for name in [
            header::CONTENT_LENGTH,
            header::CONTENT_RANGE,
            header::ACCEPT_RANGES,
        ] {
            if let Some(value) = upstream.headers().get(&name) {
                response = response.header(name, value);
            }
        }
        response
            .body(Body::from_stream(upstream.bytes_stream()))
            .map_err(|_| ApiError::unavailable("Некорректный ответ SoundHelix."))
    }
    pub async fn preview_audio(
        &self,
        id: &str,
        method: Method,
        headers: &HeaderMap,
    ) -> Result<Response, ApiError> {
        let track = self
            .track(id)
            .await
            .filter(|track| track.kind == Kind::Preview)
            .ok_or_else(|| ApiError::missing("Превью не найдено."))?;
        let url = track
            .audio_url
            .as_deref()
            .and_then(apple_asset)
            .ok_or_else(|| ApiError::invalid("Некорректный адрес превью."))?;
        let mut request = self
            .http
            .request(method, url)
            .timeout(Duration::from_secs(120));
        if let Some(range) = headers.get(header::RANGE) {
            request = request.header(header::RANGE, range);
        }
        let upstream = request
            .send()
            .await
            .map_err(|_| ApiError::unavailable("Превью недоступно."))?;
        if !upstream.status().is_success() && upstream.status() != StatusCode::RANGE_NOT_SATISFIABLE
        {
            return Err(ApiError::unavailable("Превью недоступно."));
        }
        let mut response = Response::builder()
            .status(upstream.status())
            .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
        for name in [
            header::CONTENT_TYPE,
            header::CONTENT_LENGTH,
            header::CONTENT_RANGE,
            header::ACCEPT_RANGES,
        ] {
            if let Some(value) = upstream.headers().get(&name) {
                response = response.header(name, value);
            }
        }
        response
            .body(Body::from_stream(upstream.bytes_stream()))
            .map_err(|_| ApiError::unavailable("Некорректный ответ превью."))
    }

    pub async fn track(&self, id: &str) -> Option<Track> {
        if let Some(item) = self.fixed.get(id) {
            return Some(item.clone());
        }
        let cache = self.lookup.read().await;
        if let Some((registered, item)) = cache.get(id)
            && registered.elapsed() < Duration::from_secs(6 * 3600)
        {
            return Some(item.clone());
        }
        if let Some(video) = id.strip_prefix("yt-").filter(|id| valid_video(id)) {
            return Some(youtube_track(video, "Видео YouTube", ""));
        }
        None
    }

    pub async fn search(&self, query: SearchQuery) -> Result<Search, ApiError> {
        let term = query.q.trim();
        if term.chars().count() > 140 {
            return Err(ApiError::invalid("Поисковый запрос слишком длинный."));
        }
        let mut tracks = Vec::new();
        if matches!(query.source, Source::All | Source::Full) {
            let tokens = term.to_lowercase();
            tracks.extend(
                self.full
                    .iter()
                    .filter(|track| {
                        let haystack = format!("{} {} {}", track.title, track.artist, track.source)
                            .to_lowercase();
                        tokens
                            .split_whitespace()
                            .all(|token| haystack.contains(token))
                    })
                    .take(40)
                    .cloned(),
            );
        }
        let mut notice = None;
        if !term.is_empty() {
            let remote = match query.source {
                Source::Youtube => Some((
                    Source::Youtube,
                    "YouTube сейчас недоступен. Можно открыть видео по ссылке.",
                )),
                Source::All | Source::Preview => {
                    Some((Source::Preview, "Каталог превью сейчас недоступен."))
                }
                Source::Full => None,
            };
            if let Some((provider, unavailable)) = remote {
                match self.remote(provider, term).await {
                    Ok(mut results) => tracks.append(&mut results),
                    Err(()) => notice = Some(unavailable),
                }
            }
        } else if matches!(query.source, Source::Preview | Source::Youtube) {
            notice = Some("Введи название или имя артиста.");
        }
        Ok(Search { tracks, notice })
    }

    async fn remote(&self, source: Source, term: &str) -> Result<Vec<Track>, ()> {
        let key = (
            match source {
                Source::Youtube => 1,
                _ => 0,
            },
            term.to_lowercase(),
        );
        if let Some(result) = self
            .queries
            .read()
            .await
            .get(&key)
            .filter(|(inserted, _)| inserted.elapsed() < Duration::from_secs(180))
            .map(|(_, results)| results.clone())
        {
            return Ok(result);
        }
        let tracks = match source {
            Source::Youtube => self.youtube(term).await?,
            _ => self.preview(term).await?,
        };
        let now = Instant::now();
        let mut cache = self.queries.write().await;
        cache.retain(|_, (inserted, _)| inserted.elapsed() < Duration::from_secs(180));
        if cache.len() >= 256 {
            cache.clear();
        }
        cache.insert(key, (now, tracks.clone()));
        drop(cache);
        let mut lookup = self.lookup.write().await;
        lookup.retain(|_, (inserted, _)| inserted.elapsed() < Duration::from_secs(6 * 3600));
        if lookup.len() >= 8192 {
            lookup.clear();
        }
        for track in &tracks {
            lookup.insert(track.id.clone(), (now, track.clone()));
        }
        Ok(tracks)
    }

    async fn preview(&self, term: &str) -> Result<Vec<Track>, ()> {
        let response = self
            .http
            .get("https://itunes.apple.com/search")
            .query(&[
                ("term", term),
                ("media", "music"),
                ("entity", "song"),
                ("limit", "16"),
            ])
            .send()
            .await
            .map_err(|_| ())?
            .error_for_status()
            .map_err(|_| ())?;
        let data: Value = response.json().await.map_err(|_| ())?;
        Ok(data
            .get("results")
            .and_then(Value::as_array)
            .ok_or(())?
            .iter()
            .filter_map(|item| {
                Some(Track {
                    id: format!("itunes-{}", item["trackId"].as_u64()?),
                    title: item["trackName"].as_str()?.into(),
                    artist: item["artistName"].as_str().unwrap_or("").into(),
                    kind: Kind::Preview,
                    duration_ms: 0,
                    audio_url: Some(apple_asset(item["previewUrl"].as_str()?)?),
                    artwork: item["artworkUrl100"].as_str().and_then(apple_asset),
                    video_id: None,
                    source: "iTunes",
                    attribution: None,
                    provider: None,
                })
            })
            .collect())
    }

    async fn youtube(&self, term: &str) -> Result<Vec<Track>, ()> {
        if let Some(id) = video_from_input(term) {
            return Ok(vec![youtube_track(&id, "Видео YouTube", "")]);
        }
        if let Some(key) = &self.youtube_key {
            let data: Value = self
                .http
                .get("https://www.googleapis.com/youtube/v3/search")
                .query(&[
                    ("part", "snippet"),
                    ("type", "video"),
                    ("videoCategoryId", "10"),
                    ("videoEmbeddable", "true"),
                    ("maxResults", "16"),
                    ("q", term),
                    ("key", key),
                ])
                .send()
                .await
                .map_err(|_| ())?
                .error_for_status()
                .map_err(|_| ())?
                .json()
                .await
                .map_err(|_| ())?;
            return Ok(data["items"]
                .as_array()
                .ok_or(())?
                .iter()
                .filter_map(|item| {
                    let id = item["id"]["videoId"]
                        .as_str()
                        .filter(|id| valid_video(id))?;
                    Some(youtube_track(
                        id,
                        item["snippet"]["title"].as_str()?,
                        item["snippet"]["channelTitle"].as_str().unwrap_or(""),
                    ))
                })
                .collect());
        }
        let mut request = self.http.post("https://music.youtube.com/youtubei/v1/search?prettyPrint=false")
            .header("Origin", "https://music.youtube.com")
            .json(&json!({
                "query": term,
                "context": {"client": {"clientName":"WEB_REMIX","clientVersion":"1.20260608.01.00","hl":"ru"}}
            }));
        if let Some(auth) = &self.auth {
            request = request.headers(auth.headers().map_err(|_| ())?);
        }
        let data: Value = request
            .send()
            .await
            .map_err(|_| ())?
            .error_for_status()
            .map_err(|_| ())?
            .json()
            .await
            .map_err(|_| ())?;
        let mut tracks = Vec::new();
        parse_innertube(&data, &mut tracks);
        Ok(tracks)
    }

    pub async fn lyrics(&self, track: &Track) -> Result<Lyrics, ApiError> {
        if track.artist.is_empty() {
            return Ok(Lyrics::default());
        }
        let response = self
            .http
            .get("https://lrclib.net/api/get")
            .query(&[("track_name", &track.title), ("artist_name", &track.artist)])
            .send()
            .await
            .map_err(|_| ApiError::unavailable("LRCLIB сейчас недоступен."))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(Lyrics::default());
        }
        let data: Value = response
            .error_for_status()
            .map_err(|_| ApiError::unavailable("LRCLIB сейчас недоступен."))?
            .json()
            .await
            .map_err(|_| ApiError::unavailable("LRCLIB вернул неверный ответ."))?;
        Ok(Lyrics {
            synced: data["syncedLyrics"]
                .as_str()
                .filter(|s| s.len() <= 65536)
                .map(str::to_owned),
            plain: data["plainLyrics"]
                .as_str()
                .filter(|s| s.len() <= 65536)
                .map(str::to_owned),
        })
    }
}

fn apple_asset(raw: &str) -> Option<String> {
    let url = Url::parse(raw).ok()?;
    let host = url.host_str()?;
    if url.scheme() == "https"
        && (host.ends_with(".mzstatic.com") || host.ends_with(".itunes.apple.com"))
        && url.username().is_empty()
        && url.password().is_none()
    {
        Some(url.to_string())
    } else {
        None
    }
}

fn valid_video(id: &str) -> bool {
    id.len() == 11
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn video_from_input(term: &str) -> Option<String> {
    if valid_video(term) {
        return Some(term.into());
    }
    let url = Url::parse(term).ok()?;
    if url.scheme() != "https" {
        return None;
    }
    let id = match url.host_str()? {
        "youtu.be" => url.path().trim_matches('/').to_owned(),
        "youtube.com" | "www.youtube.com" | "music.youtube.com" => url
            .query_pairs()
            .find(|(name, _)| name == "v")?
            .1
            .into_owned(),
        _ => return None,
    };
    valid_video(&id).then_some(id)
}

fn youtube_track(id: &str, title: &str, artist: &str) -> Track {
    Track {
        id: format!("yt-{id}"),
        title: title.into(),
        artist: artist.into(),
        kind: Kind::Youtube,
        duration_ms: 0,
        audio_url: None,
        artwork: Some(format!("https://i.ytimg.com/vi/{id}/hqdefault.jpg")),
        video_id: Some(id.into()),
        source: "YouTube",
        attribution: None,
        provider: None,
    }
}

fn parse_innertube(node: &Value, tracks: &mut Vec<Track>) {
    if tracks.len() >= 20 {
        return;
    }
    if let Some(item) = node.get("musicResponsiveListItemRenderer") {
        let id = item["playlistItemData"]["videoId"].as_str();
        let column = |index: usize| {
            item["flexColumns"][index]["musicResponsiveListItemFlexColumnRenderer"]["text"]["runs"]
                [0]["text"]
                .as_str()
        };
        if let (Some(id), Some(title)) = (id.filter(|id| valid_video(id)), column(0))
            && !tracks
                .iter()
                .any(|track| track.video_id.as_deref() == Some(id))
        {
            tracks.push(youtube_track(id, title, column(1).unwrap_or("")));
        }
        return;
    }
    match node {
        Value::Array(children) => {
            for child in children {
                parse_innertube(child, tracks);
            }
        }
        Value::Object(children) => {
            for child in children.values() {
                parse_innertube(child, tracks);
            }
        }
        _ => {}
    }
}

type Scanned = (Vec<Track>, HashMap<String, LocalFile>);
fn scan(start: &Path) -> Result<Scanned> {
    let root = start.canonicalize().context("PUMP_MEDIA_DIR недоступна")?;
    anyhow::ensure!(root.is_dir(), "PUMP_MEDIA_DIR должна быть папкой");
    let mut stack = vec![root.clone()];
    let mut scanned = Vec::new();
    let mut files = HashMap::new();
    let mut visited = 0;
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(dir)?.flatten() {
            visited += 1;
            anyhow::ensure!(
                visited <= 100_000,
                "Лимит обхода PUMP_MEDIA_DIR — 100 000 элементов"
            );
            let path = entry.path();
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let kind = entry.file_type()?;
            if kind.is_dir() {
                stack.push(path);
                continue;
            }
            if !kind.is_file() || entry.metadata()?.len() == 0 {
                continue;
            }
            let ext = path
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if !matches!(
                ext.as_str(),
                "mp3" | "m4a" | "aac" | "ogg" | "opus" | "flac" | "wav" | "webm"
            ) {
                continue;
            }
            let relative = path.strip_prefix(&root)?;
            let prefix = if ext == "mp3" { "mp3" } else { "file" };
            let id = format!(
                "{prefix}-{}",
                hex::encode(&Sha256::digest(relative.as_os_str().as_encoded_bytes())[..16])
            );
            let mut title = path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let mut artist = String::new();
            let mut duration_ms = 0;
            let mut cover = None;
            if let Ok(tagged) = Probe::open(&path).and_then(Probe::read) {
                duration_ms = (tagged.properties().duration().as_millis() as u64).min(MAX_TRACK_MS);
                if let Some(tags) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
                    if let Some(name) = tags.title().filter(|name| !name.trim().is_empty()) {
                        title = name.into_owned();
                    }
                    artist = tags.artist().unwrap_or_default().into_owned();
                    cover = tags.pictures().iter().find_map(|picture| {
                        let mime = match picture.mime_type()?.as_str() {
                            "image/jpeg" => "image/jpeg",
                            "image/png" => "image/png",
                            "image/webp" => "image/webp",
                            "image/gif" => "image/gif",
                            _ => return None,
                        };
                        (picture.data().len() <= 2 * 1024 * 1024)
                            .then(|| (mime, Bytes::copy_from_slice(picture.data())))
                    });
                }
            }
            scanned.push(Track {
                id: id.clone(),
                title,
                artist,
                kind: Kind::Full,
                duration_ms,
                audio_url: Some(if ext == "mp3" {
                    format!("/media/{id}.mp3")
                } else {
                    format!("/media/{id}")
                }),
                artwork: cover.as_ref().map(|_| format!("/covers/{id}")),
                video_id: None,
                source: "Медиатека",
                attribution: None,
                provider: None,
            });
            files.insert(id, LocalFile { path, cover });
        }
    }
    scanned.sort_by_cached_key(|track| {
        (
            track.title.to_lowercase(),
            track.artist.to_lowercase(),
            track.id.clone(),
        )
    });
    Ok((scanned, files))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_public_video_links_and_labels_allowed_urls() {
        assert_eq!(
            video_from_input("https://youtu.be/5NV6Rdv1a3I?t=5").as_deref(),
            Some("5NV6Rdv1a3I")
        );
        assert!(video_from_input("https://fake.example/?v=5NV6Rdv1a3I").is_none());
        assert!(apple_asset("https://audio-ssl.itunes.apple.com/t.mp3").is_some());
        assert!(apple_asset("https://itunes.apple.com@127.0.0.1/n.mp3").is_none());
    }
}
