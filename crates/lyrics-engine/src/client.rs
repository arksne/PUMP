//! Параллельный поиск синхронных и обычных текстов с явным результатом источника.
use crate::{Lyrics, ParseOptions, parse, parse_plain};
use serde::Serialize;
use serde_json::Value;
use std::time::Duration;
/// Резервный источник не задерживает быстрый ответ: каждый источник ограничен своим сроком.
const SOURCE_TIMEOUT: Duration = Duration::from_secs(6);
#[derive(Clone)]
pub struct LyricsClient {
    http: reqwest::Client,
}
#[derive(Clone, Serialize)]
pub struct LyricsResult {
    pub synced: Option<String>,
    pub plain: Option<String>,
    pub document: Option<Lyrics>,
    pub sources: Vec<String>,
}
impl LyricsClient {
    pub fn new(http: reqwest::Client) -> Self {
        Self { http }
    }
    pub async fn fetch(
        &self,
        artist: &str,
        title: &str,
        duration_ms: Option<u64>,
    ) -> Result<LyricsResult, reqwest::Error> {
        if artist.trim().is_empty() || title.trim().is_empty() {
            return Ok(LyricsResult {
                synced: None,
                plain: None,
                document: None,
                sources: Vec::new(),
            });
        }
        // Оба источника ограничены суммарно: медленный ответ не держит запрос.
        let timed = async {
            tokio::time::timeout(SOURCE_TIMEOUT, async {
                let r = self
                    .http
                    .get("https://lrclib.net/api/get")
                    .query(&[("artist_name", artist), ("track_name", title)])
                    .send()
                    .await?
                    .error_for_status()?
                    .json::<Value>()
                    .await?;
                Ok::<Value, reqwest::Error>(r)
            })
            .await
            .ok()?
            .ok()
        };
        let plain = async {
            let url = format!(
                "https://api.lyrics.ovh/v1/{}/{}",
                component(artist),
                component(title)
            );
            tokio::time::timeout(SOURCE_TIMEOUT, async {
                let r = self
                    .http
                    .get(url)
                    .send()
                    .await?
                    .error_for_status()?
                    .json::<Value>()
                    .await?;
                Ok::<Value, reqwest::Error>(r)
            })
            .await
            .ok()?
            .ok()
        };
        let (timed, plain) = tokio::join!(timed, plain);
        let mut result = LyricsResult {
            synced: None,
            plain: None,
            document: None,
            sources: Vec::new(),
        };
        if let Some(value) = timed {
            result.synced = text(&value, "syncedLyrics");
            result.plain = text(&value, "plainLyrics");
            if result.synced.is_some() || result.plain.is_some() {
                result.sources.push("LRCLIB".into());
            }
        }
        if result.plain.is_none()
            && let Some(value) = plain
        {
            result.plain = text(&value, "lyrics");
            if result.plain.is_some() {
                result.sources.push("lyrics.ovh".into());
            }
        }
        result.document = result
            .synced
            .as_deref()
            .and_then(|text| parse(text, ParseOptions { duration_ms }).ok())
            .or_else(|| result.plain.as_deref().map(parse_plain));
        Ok(result)
    }
}
/// Верхняя граница текста песни: большие ответы усекаются молча,
/// как и в остальном каталоге (64 КиБ).
const MAX_LYRICS_BYTES: usize = 64 * 1024;
fn text(value: &Value, key: &str) -> Option<String> {
    value[key]
        .as_str()
        .filter(|text| !text.is_empty() && text.len() <= MAX_LYRICS_BYTES)
        .map(str::to_owned)
}
fn component(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
