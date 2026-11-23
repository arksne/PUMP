//! Подпись запросов Last.fm и проверка результата скробблинга.
use crate::{Error, Result, error::checked};
use async_trait::async_trait;
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Подпись Last.fm: отсортированные ключи, UTF-8, без format/callback/api_sig.
pub fn api_signature(params: &BTreeMap<String, String>, secret: &str) -> String {
    let mut digest = Md5::new();
    for (key, value) in params {
        if matches!(key.as_str(), "format" | "callback" | "api_sig") {
            continue;
        }
        digest.update(key.as_bytes());
        digest.update(value.as_bytes());
    }
    digest.update(secret.as_bytes());
    format!("{:x}", digest.finalize())
}

/// API key, secret и session key предоставляет вызывающая сторона.
pub struct LastFmCredentials {
    pub api_key: String,
    pub shared_secret: String,
    pub session_key: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScrobbleTrack {
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    pub duration_ms: u64,
    /// Исходное время начала воспроизведения, не время отправки очереди.
    pub started_at_unix: u64,
}
impl ScrobbleTrack {
    pub fn validate(&self) -> Result<()> {
        if self.artist.trim().is_empty()
            || self.title.trim().is_empty()
            || self.duration_ms == 0
            || self.started_at_unix == 0
        {
            return Err(Error::Invalid(
                "scrobble needs artist, title, duration and original start timestamp".into(),
            ));
        }
        Ok(())
    }
    /// played_ms — реально прослушанное время, без пауз и перемотки.
    /// Порог строго 50%; правило сокращения до четырёх минут не применяется.
    pub fn eligible(&self, played_ms: u64) -> bool {
        self.duration_ms > 30_000 && played_ms >= self.duration_ms.div_ceil(2)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Submission {
    Accepted,
    Ignored { code: u32, message: String },
}

/// Позволяет тестировать очередь без сети и подставлять собственный транспорт.
#[async_trait]
pub trait ScrobbleSink: Sync {
    async fn submit(&self, track: &ScrobbleTrack) -> Result<Submission>;
}

pub struct LastFmClient {
    credentials: LastFmCredentials,
    http: reqwest::Client,
    endpoint: String,
}
impl LastFmClient {
    pub fn new(credentials: LastFmCredentials) -> Result<Self> {
        if credentials.api_key.is_empty()
            || credentials.shared_secret.is_empty()
            || credentials.session_key.is_empty()
        {
            return Err(Error::Invalid(
                "all Last.fm credentials are required".into(),
            ));
        }
        Ok(Self {
            credentials,
            http: crate::http_client()?,
            endpoint: "https://ws.audioscrobbler.com/2.0/".into(),
        })
    }

    async fn call(&self, method: &str, track: &ScrobbleTrack, timestamp: bool) -> Result<Value> {
        track.validate()?;
        let mut params = BTreeMap::from([
            ("method".into(), method.into()),
            ("api_key".into(), self.credentials.api_key.clone()),
            ("sk".into(), self.credentials.session_key.clone()),
            ("artist".into(), track.artist.clone()),
            ("track".into(), track.title.clone()),
            ("duration".into(), (track.duration_ms / 1000).to_string()),
        ]);
        if let Some(album) = &track.album {
            params.insert("album".into(), album.clone());
        }
        if timestamp {
            params.insert("timestamp".into(), track.started_at_unix.to_string());
        }
        params.insert(
            "api_sig".into(),
            api_signature(&params, &self.credentials.shared_secret),
        );
        params.insert("format".into(), "json".into());
        let response: Value = checked(self.http.post(&self.endpoint).form(&params).send().await?)
            .await?
            .json()
            .await?;
        if let Some(code) = response["error"].as_u64() {
            return Err(Error::Service {
                code: code as u32,
                message: response["message"]
                    .as_str()
                    .unwrap_or("Last.fm error")
                    .into(),
            });
        }
        Ok(response)
    }

    pub async fn now_playing(&self, track: &ScrobbleTrack) -> Result<Submission> {
        let value = self.call("track.updateNowPlaying", track, false).await?;
        parse_ignored(&value["nowplaying"]["ignoredMessage"])
    }
}

fn parse_ignored(value: &Value) -> Result<Submission> {
    let code = value["code"]
        .as_u64()
        .or_else(|| value["code"].as_str().and_then(|s| s.parse().ok()))
        .ok_or_else(|| Error::Protocol("Last.fm response lacks ignoredMessage code".into()))?;
    if code == 0 {
        Ok(Submission::Accepted)
    } else {
        Ok(Submission::Ignored {
            code: code as u32,
            message: value["#text"]
                .as_str()
                .unwrap_or("ignored by Last.fm")
                .into(),
        })
    }
}

#[async_trait]
impl ScrobbleSink for LastFmClient {
    async fn submit(&self, track: &ScrobbleTrack) -> Result<Submission> {
        let value = self.call("track.scrobble", track, true).await?;
        let scrobble = &value["scrobbles"]["scrobble"];
        let item = if let Some(items) = scrobble.as_array() {
            if items.len() != 1 {
                return Err(Error::Protocol("unexpected scrobble count".into()));
            }
            &items[0]
        } else {
            scrobble
        };
        parse_ignored(&item["ignoredMessage"])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn signature_sorted_and_excludes_transport_fields() {
        let mut params = BTreeMap::from([("b".into(), "2".into()), ("a".into(), "1".into())]);
        assert_eq!(
            api_signature(&params, "secret"),
            "670699129dd49818b5abd9e7c2fd6569"
        );
        params.insert("format".into(), "json".into());
        params.insert("callback".into(), "anything".into());
        params.insert("api_sig".into(), "old".into());
        assert_eq!(
            api_signature(&params, "secret"),
            "670699129dd49818b5abd9e7c2fd6569"
        );
    }
    #[test]
    fn ignored_is_not_success() {
        assert!(matches!(
            parse_ignored(&serde_json::json!({"code":"3", "#text":"too old"})).unwrap(),
            Submission::Ignored { code: 3, .. }
        ));
        assert!(parse_ignored(&Value::Null).is_err());
    }
}
