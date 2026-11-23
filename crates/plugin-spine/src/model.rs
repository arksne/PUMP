//! Типы данных и сериализация публичного контракта модуля.
use std::{cmp::Reverse, fmt};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::{PluginError, Result};

pub const ABI_VERSION: u32 = 1;
pub const MAX_SEARCH_RESULTS: u32 = 100;
pub const MAX_CANDIDATES: usize = 100;

/// Абсолютный HTTP(S) URL; поле закрыто, чтобы сохранить проверенный инвариант.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HttpUrl(Url);

impl HttpUrl {
    pub fn parse(value: &str) -> Result<Self> {
        if !(value.starts_with("https://") || value.starts_with("http://"))
            || value.chars().any(|c| c.is_control() || c.is_whitespace())
            || value.contains('\\')
        {
            return Err(PluginError::Response(
                "ожидался абсолютный HTTP(S) URL".into(),
            ));
        }
        let url = Url::parse(value).map_err(|e| PluginError::Response(e.to_string()))?;
        if url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {
            return Err(PluginError::Response(
                "URL без хоста или с учётными данными".into(),
            ));
        }
        Ok(Self(url))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl TryFrom<String> for HttpUrl {
    type Error = PluginError;

    fn try_from(value: String) -> Result<Self> {
        Self::parse(&value)
    }
}

impl From<HttpUrl> for String {
    fn from(value: HttpUrl) -> Self {
        value.0.into()
    }
}

impl fmt::Display for HttpUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchRequest {
    pub abi_version: u32,
    pub query: String,
    pub limit: u32,
}

impl SearchRequest {
    pub fn new(query: impl Into<String>, limit: u32) -> Self {
        Self {
            abi_version: ABI_VERSION,
            query: query.into(),
            limit,
        }
    }

    pub fn validate(&self) -> Result<()> {
        check_version(self.abi_version)?;
        if self.query.trim().is_empty()
            || self.query.len() > 4096
            || !(1..=MAX_SEARCH_RESULTS).contains(&self.limit)
        {
            return Err(PluginError::Request(
                "пустой/длинный запрос или limit вне 1..=100".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackKind {
    Full,
    Preview,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Track {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub kind: TrackKind,
    pub duration_ms: Option<u64>,
    pub artwork: Option<HttpUrl>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchResponse {
    pub abi_version: u32,
    pub tracks: Vec<Track>,
}

impl SearchResponse {
    pub fn validate(&self, request: &SearchRequest) -> Result<()> {
        check_version(self.abi_version)?;
        if self.tracks.len() > request.limit as usize {
            return Err(PluginError::Response("превышен limit результатов".into()));
        }
        let mut ids = std::collections::BTreeSet::new();
        for track in &self.tracks {
            if !valid_id(&track.id) || track.title.trim().is_empty() || !ids.insert(&track.id) {
                return Err(PluginError::Response(
                    "неверный или повторный идентификатор/пустое название".into(),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioQuality {
    Low,
    Standard,
    #[default]
    High,
    Lossless,
}

/// Сначала lossless (если запрошен), затем целевое качество, ниже, выше.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualityPreference {
    pub preferred: AudioQuality,
    pub prefer_lossless: bool,
    pub allow_fallback: bool,
}

impl Default for QualityPreference {
    fn default() -> Self {
        Self {
            preferred: AudioQuality::High,
            prefer_lossless: true,
            allow_fallback: true,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveRequest {
    pub abi_version: u32,
    pub track_id: String,
    pub quality: QualityPreference,
}

impl ResolveRequest {
    pub fn new(track_id: impl Into<String>, quality: QualityPreference) -> Self {
        Self {
            abi_version: ABI_VERSION,
            track_id: track_id.into(),
            quality,
        }
    }

    pub fn validate(&self) -> Result<()> {
        check_version(self.abi_version)?;
        if !valid_id(&self.track_id) {
            return Err(PluginError::Request("неверный идентификатор трека".into()));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioCandidate {
    pub url: HttpUrl,
    pub quality: AudioQuality,
    pub mime_type: String,
    pub bitrate_kbps: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveResponse {
    pub abi_version: u32,
    pub candidates: Vec<AudioCandidate>,
}

impl ResolveResponse {
    pub fn validate(&self) -> Result<()> {
        check_version(self.abi_version)?;
        if self.candidates.len() > MAX_CANDIDATES
            || self.candidates.iter().any(|c| {
                !c.mime_type.starts_with("audio/")
                    || c.mime_type.len() <= 6
                    || c.mime_type
                        .chars()
                        .any(|ch| ch.is_control() || ch.is_whitespace())
                    || c.bitrate_kbps == Some(0)
            })
        {
            return Err(PluginError::Response("неверные аудиокандидаты".into()));
        }
        Ok(())
    }

    /// Возвращает основной URL и следующие варианты в стабильном порядке.
    pub fn ranked(&self, preference: QualityPreference) -> Result<Vec<AudioCandidate>> {
        self.validate()?;
        let first = if preference.prefer_lossless {
            AudioQuality::Lossless
        } else {
            preference.preferred
        };
        let mut qualities = vec![first];
        if preference.allow_fallback {
            qualities.push(preference.preferred);
            qualities.extend(
                [
                    AudioQuality::Lossless,
                    AudioQuality::High,
                    AudioQuality::Standard,
                    AudioQuality::Low,
                ]
                .into_iter()
                .filter(|q| *q < preference.preferred),
            );
            qualities.extend(
                [
                    AudioQuality::Low,
                    AudioQuality::Standard,
                    AudioQuality::High,
                    AudioQuality::Lossless,
                ]
                .into_iter()
                .filter(|q| *q > preference.preferred),
            );
        }
        let mut candidates: Vec<_> = self
            .candidates
            .iter()
            .filter(|c| qualities.contains(&c.quality))
            .cloned()
            .collect();
        candidates.sort_by(|a, b| {
            let key = |c: &AudioCandidate| {
                (
                    qualities.iter().position(|q| *q == c.quality).unwrap(),
                    Reverse(c.bitrate_kbps.unwrap_or(0)),
                )
            };
            key(a)
                .cmp(&key(b))
                .then_with(|| a.url.as_str().cmp(b.url.as_str()))
                .then_with(|| a.mime_type.cmp(&b.mime_type))
        });
        candidates.dedup();
        if candidates.is_empty() {
            return Err(PluginError::NoCandidates);
        }
        Ok(candidates)
    }
}

pub(crate) fn check_version(version: u32) -> Result<()> {
    if version != ABI_VERSION {
        return Err(PluginError::Abi(format!(
            "версия {version}, ожидалась {ABI_VERSION}"
        )));
    }
    Ok(())
}

pub(crate) fn valid_id(id: &str) -> bool {
    !id.trim().is_empty() && id.len() <= 1024 && !id.chars().any(char::is_control)
}
