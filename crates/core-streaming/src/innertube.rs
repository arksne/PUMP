//! Сессионная авторизация InnerTube и выбор доступных аудио/видеопотоков.
//! Cookies передаются только домену YouTube, ссылки на CDN проверяются отдельно.
use crate::ApiError;
use reqwest::{
    Client,
    header::{HeaderMap, HeaderValue},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;

#[derive(Clone)]
pub struct SessionAuth {
    cookies: String,
    sapisid: String,
}

impl SessionAuth {
    pub fn from_cookies(cookies: String) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !cookies.contains(['\r', '\n']),
            "Cookies содержат перевод строки"
        );
        let sapisid = cookies
            .split(';')
            .filter_map(|part| part.trim().split_once('='))
            .find(|(key, value)| {
                matches!(*key, "SAPISID" | "__Secure-3PAPISID") && !value.is_empty()
            })
            .map(|(_, value)| value.to_owned())
            .ok_or_else(|| anyhow::anyhow!("В cookies отсутствует SAPISID"))?;
        HeaderValue::from_str(&cookies)?;
        Ok(Self { cookies, sapisid })
    }
    pub fn authorization_at(&self, seconds: u64) -> String {
        let digest = Sha1::digest(format!(
            "{seconds} {} https://music.youtube.com",
            self.sapisid
        ));
        format!("SAPISIDHASH {seconds}_{}", hex::encode(digest))
    }
    pub fn headers(&self) -> anyhow::Result<HeaderMap> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let mut headers = HeaderMap::new();
        headers.insert("cookie", HeaderValue::from_str(&self.cookies)?);
        headers.insert(
            "authorization",
            HeaderValue::from_str(&self.authorization_at(now))?,
        );
        headers.insert(
            "x-origin",
            HeaderValue::from_static("https://music.youtube.com"),
        );
        headers.insert("x-goog-authuser", HeaderValue::from_static("0"));
        Ok(headers)
    }
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaMode {
    #[default]
    Audio,
    Video,
}
#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    High,
    #[default]
    Balanced,
    Low,
}
#[derive(Clone, Deserialize, Default)]
pub struct StreamOptions {
    #[serde(default)]
    pub mode: MediaMode,
    #[serde(default)]
    pub quality: Quality,
}
#[derive(Clone, Serialize)]
pub struct Stream {
    pub format_id: u64,
    pub url: String,
    pub mime_type: String,
    pub bitrate: u64,
    pub sample_rate: Option<u32>,
    pub height: Option<u32>,
    pub has_audio: bool,
    pub has_video: bool,
    #[serde(skip)]
    pub upstream: String,
}
#[derive(Clone, Serialize)]
pub struct Streams {
    pub streams: Vec<Stream>,
    pub expires_at: u64,
}

pub fn valid_media_url(value: &str) -> bool {
    Url::parse(value).ok().is_some_and(|url| {
        url.scheme() == "https"
            && url
                .host_str()
                .is_some_and(|host| host.ends_with(".googlevideo.com"))
            && url.username().is_empty()
            && url.password().is_none()
    })
}

pub async fn player(
    http: &Client,
    video_id: &str,
    auth: Option<&SessionAuth>,
) -> Result<Streams, ApiError> {
    if !crate::valid_video(video_id) {
        return Err(ApiError::invalid("Некорректный ID видео."));
    }
    let clients = [("WEB_REMIX", "1.20260608.01.00"), ("ANDROID_VR", "1.60.19")];
    for (name, version) in clients {
        let mut request = http.post("https://music.youtube.com/youtubei/v1/player?prettyPrint=false")
            .header("Origin", "https://music.youtube.com")
            .json(&json!({"videoId":video_id,"context":{"client":{"clientName":name,"clientVersion":version,"hl":"ru"}},"contentCheckOk":true,"racyCheckOk":true}));
        if let Some(auth) = auth {
            request = request.headers(
                auth.headers()
                    .map_err(|_| ApiError::invalid("Неверная сессия YouTube."))?,
            );
        }
        let data = match request.send().await {
            Ok(response) if response.status().is_success() => response.json::<Value>().await.ok(),
            _ => None,
        };
        let Some(data) = data else {
            continue;
        };
        if data["playabilityStatus"]["status"].as_str() != Some("OK") {
            continue;
        }
        let formats = data["streamingData"]["adaptiveFormats"]
            .as_array()
            .into_iter()
            .flatten()
            .chain(
                data["streamingData"]["formats"]
                    .as_array()
                    .into_iter()
                    .flatten(),
            );
        let mut streams = Vec::new();
        for item in formats {
            let Some(upstream) = item["url"].as_str().filter(|url| valid_media_url(url)) else {
                continue;
            };
            let Some(format_id) = item["itag"].as_u64() else {
                continue;
            };
            let Some(mime) = item["mimeType"].as_str() else {
                continue;
            };
            let has_video = mime.starts_with("video/");
            let has_audio = mime.starts_with("audio/") || item["audioQuality"].is_string();
            if !has_audio && !has_video {
                continue;
            }
            streams.push(Stream {
                format_id,
                upstream: upstream.into(),
                url: format!("/stream/yt-{video_id}/{format_id}"),
                mime_type: mime.into(),
                bitrate: item["bitrate"].as_u64().unwrap_or(0),
                sample_rate: item["audioSampleRate"]
                    .as_str()
                    .and_then(|n| n.parse().ok()),
                height: item["height"].as_u64().and_then(|n| u32::try_from(n).ok()),
                has_audio,
                has_video,
            });
        }
        if !streams.is_empty() {
            let ttl = data["streamingData"]["expiresInSeconds"]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(3600)
                .min(21600);
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            return Ok(Streams {
                streams,
                expires_at: now + ttl.saturating_sub(60),
            });
        }
    }
    Err(ApiError::unavailable(
        "YouTube не предоставил прямой поток. Возможны ограничения источника, запрос сессии или зашифрованной подписи. Можно открыть встроенное видео.",
    ))
}

impl Streams {
    pub fn select(&self, options: &StreamOptions) -> Vec<Stream> {
        let mut streams: Vec<_> = self
            .streams
            .iter()
            .filter(|s| match options.mode {
                MediaMode::Audio => s.has_audio && !s.has_video,
                MediaMode::Video => s.has_audio && s.has_video,
            })
            .cloned()
            .collect();
        streams.sort_by_key(|s| match options.quality {
            Quality::High => u64::MAX - s.bitrate,
            Quality::Low => s.bitrate,
            Quality::Balanced => s.bitrate.abs_diff(match options.mode {
                MediaMode::Audio => 128_000,
                MediaMode::Video => 1_500_000,
            }),
        });
        streams
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn credentials_and_stream_hosts_are_validated() {
        assert!(SessionAuth::from_cookies("SAPISID=abc\n".into()).is_err());
        let auth = SessionAuth::from_cookies("SAPISID=abc; SID=hidden".into()).unwrap();
        let expected = hex::encode(Sha1::digest(b"100 abc https://music.youtube.com"));
        assert_eq!(
            auth.authorization_at(100),
            format!("SAPISIDHASH 100_{expected}")
        );
        assert!(valid_media_url(
            "https://rr1.googlevideo.com/videoplayback?id=a"
        ));
        assert!(!valid_media_url("https://googlevideo.com.evil.test/video"));
        assert!(!valid_media_url("http://127.0.0.1/video"));
    }
}
