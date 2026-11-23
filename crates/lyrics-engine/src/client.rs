//! Параллельный поиск синхронных и обычных текстов с явным результатом источника.
use crate::{Lyrics, ParseOptions, parse, parse_plain};
use serde::Serialize;
use serde_json::Value;
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
        let timed = async {
            let r = self
                .http
                .get("https://lrclib.net/api/get")
                .query(&[("artist_name", artist), ("track_name", title)])
                .send()
                .await?;
            r.error_for_status()?.json::<Value>().await
        };
        let plain = async {
            let url = format!(
                "https://api.lyrics.ovh/v1/{}/{}",
                component(artist),
                component(title)
            );
            let r = self.http.get(url).send().await?;
            r.error_for_status()?.json::<Value>().await
        };
        let (timed, plain) = tokio::join!(timed, plain);
        let mut result = LyricsResult {
            synced: None,
            plain: None,
            document: None,
            sources: Vec::new(),
        };
        if let Ok(value) = timed {
            result.synced = text(&value, "syncedLyrics");
            result.plain = text(&value, "plainLyrics");
            if result.synced.is_some() || result.plain.is_some() {
                result.sources.push("LRCLIB".into());
            }
        }
        if result.plain.is_none()
            && let Ok(value) = plain
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
fn text(value: &Value, key: &str) -> Option<String> {
    value[key]
        .as_str()
        .filter(|text| !text.is_empty() && text.len() <= 1024 * 1024)
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
