//! Public Audius REST provider. Never request gated or preview streams.
//! https://docs.audius.co/api/
use crate::{ApiError, Kind, MAX_TRACK_MS, Track};
use axum::http::{HeaderMap, Method, StatusCode, header};
use serde_json::Value;
use std::time::Duration;
use url::Url;

const API: &str = "https://api.audius.co/v1";
const MAX_JSON: usize = 1024 * 1024;

pub struct AudiusClient {
    http: reqwest::Client,
    api: String,
    key: Option<String>,
    media_hosts: Vec<String>,
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 32 && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn public_track(value: &Value) -> Option<Track> {
    if value["is_streamable"].as_bool() != Some(true)
        || value["is_stream_gated"].as_bool() != Some(false)
        || value["is_unlisted"].as_bool() == Some(true)
        || value["is_scheduled_release"].as_bool() == Some(true)
        || value["access"]["stream"].as_bool() == Some(false)
        || value["allowed_api_keys"]
            .as_array()
            .is_some_and(|keys| !keys.is_empty())
    {
        return None;
    }
    let id = value["id"].as_str().filter(|id| valid_id(id))?;
    let title = value["title"].as_str()?.trim();
    let artist = value["user"]["name"].as_str()?.trim();
    let duration_ms = value["duration"].as_u64()?.checked_mul(1000)?;
    if title.is_empty()
        || artist.is_empty()
        || duration_ms == 0
        || duration_ms > MAX_TRACK_MS
    {
        return None;
    }
    let artwork = value["artwork"]["480x480"]
        .as_str()
        .or_else(|| value["artwork"]["150x150"].as_str())
        .and_then(|url| Url::parse(url).ok())
        .filter(|url| {
            url.scheme() == "https"
                && url.username().is_empty()
                && url.password().is_none()
                && url.host_str().is_some()
        })
        .map(|url| url.to_string());
    Some(Track {
        id: format!("au-{id}"),
        title: title.chars().take(200).collect(),
        artist: artist.chars().take(200).collect(),
        kind: Kind::Full,
        duration_ms,
        audio_url: Some(format!("/source/au/{id}")),
        artwork,
        video_id: None,
        source: "Audius",
        attribution: Some("Audius — public streaming"),
        provider: None,
    })
}

fn fixture_base(value: &str) -> Option<Url> {
    let url = Url::parse(value).ok()?;
    (url.scheme() == "http"
        && url.host_str() == Some("127.0.0.1")
        && url.port().is_some()
        && url.path() == "/v1"
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none())
    .then_some(url)
}

fn safe_media_url(url: &Url, allowed: &[String]) -> bool {
    url.scheme() == "https"
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.host_str().is_some_and(|host| {
            host == "api.audius.co"
                || host == "discoveryprovider.audius.co"
                || allowed.iter().any(|item| item == host)
        })
        && matches!(url.host(), Some(url::Host::Domain(_)))
}

impl AudiusClient {
    pub fn from_env(http: reqwest::Client) -> Self {
        // Explicit, loopback-only fixture for deterministic end-to-end tests.
        let api = std::env::var("PUMP_AUDIUS_TEST_API_BASE")
            .ok()
            .filter(|value| fixture_base(value).is_some())
            .unwrap_or_else(|| API.to_owned());
        Self {
            http,
            api,
            key: std::env::var("PUMP_AUDIUS_API_KEY")
                .ok()
                .filter(|key| !key.trim().is_empty()),
            media_hosts: std::env::var("PUMP_AUDIUS_MEDIA_HOSTS")
                .unwrap_or_default()
                .split(',')
                .map(|host| host.trim().to_ascii_lowercase())
                .filter(|host| !host.is_empty())
                .take(32)
                .collect(),
        }
    }

    fn api_request(&self, method: Method, url: &str) -> reqwest::RequestBuilder {
        let request = self.http.request(method, url).query(&[("app_name", "PUMP")]);
        if self.api == API
            && let Some(key) = &self.key
        {
            request.header("x-api-key", key)
        } else {
            request
        }
    }

    async fn json(&self, request: reqwest::RequestBuilder) -> Result<Value, ApiError> {
        let mut response = request.send().await.map_err(|_| unavailable())?;
        if response.status() == StatusCode::NOT_FOUND {
            return Err(ApiError::missing("Запись Audius не найдена."));
        }
        if !response.status().is_success()
            || response.content_length().is_some_and(|len| len > MAX_JSON as u64)
        {
            return Err(unavailable());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| unavailable())? {
            if bytes.len().saturating_add(chunk.len()) > MAX_JSON {
                return Err(unavailable());
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| unavailable())
    }

    pub async fn search(&self, term: &str) -> Result<Vec<Track>, ApiError> {
        let value = self
            .json(
                self.api_request(Method::GET, &format!("{}/tracks/search", self.api))
                    .query(&[("query", term), ("limit", "16")]),
            )
            .await?;
        Ok(value["data"]
            .as_array()
            .ok_or_else(unavailable)?
            .iter()
            .filter_map(public_track)
            .take(16)
            .collect())
    }

    pub async fn track(&self, id: &str) -> Result<Track, ApiError> {
        if !valid_id(id) {
            return Err(ApiError::missing("Запись Audius не найдена."));
        }
        let value = self
            .json(self.api_request(Method::GET, &format!("{}/tracks/{id}", self.api)))
            .await?;
        public_track(&value["data"])
            .filter(|track| track.id == format!("au-{id}"))
            .ok_or_else(|| ApiError::new(StatusCode::CONFLICT, "Нет открытого потока Audius."))
    }

    pub async fn stream(
        &self,
        id: &str,
        method: Method,
        headers: &HeaderMap,
    ) -> Result<reqwest::Response, ApiError> {
        // Recheck permissions before every new media request.
        self.track(id).await?;
        let mut url = Url::parse(&format!("{}/tracks/{id}/stream", self.api))
            .map_err(|_| unavailable())?;
        url.query_pairs_mut()
            .append_pair("preview", "false")
            .append_pair("app_name", "PUMP");
        for hop in 0..=4 {
            if !safe_media_url(&url, &self.media_hosts)
                && !fixture_base(&self.api).is_some_and(|base| base.origin() == url.origin())
            {
                return Err(ApiError::unavailable("Медиа-хост Audius не разрешён настройками."));
            }
            let mut request = self
                .http
                .request(method.clone(), url.clone())
                .timeout(Duration::from_secs(24 * 60 * 60));
            // API credentials must never follow redirects to a media host.
            if hop == 0
                && self.api == API
                && let Some(key) = &self.key
            {
                request = request.header("x-api-key", key);
            }
            if let Some(range) = headers.get(header::RANGE) {
                request = request.header(header::RANGE, range);
            }
            let response = tokio::time::timeout(Duration::from_secs(15), request.send())
                .await
                .map_err(|_| unavailable())?
                .map_err(|_| unavailable())?;
            if response.status().is_redirection() {
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(unavailable)?;
                url = url.join(location).map_err(|_| unavailable())?;
                continue;
            }
            if response.status().is_success()
                || response.status() == StatusCode::RANGE_NOT_SATISFIABLE
            {
                return Ok(response);
            }
            return Err(unavailable());
        }
        Err(unavailable())
    }
}

fn unavailable() -> ApiError {
    ApiError::unavailable("Audius недоступен или требует API-ключ.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn public_only_and_full_duration() {
        let mut value = json!({
            "id": "Ab12",
            "title": "Track",
            "user": {"name": "Artist"},
            "duration": 180,
            "is_streamable": true,
            "is_stream_gated": false
        });
        let track = public_track(&value).expect("public track");
        assert_eq!(track.id, "au-Ab12");
        assert_eq!(track.duration_ms, 180_000);
        assert!(track.kind == Kind::Full);
        value["is_stream_gated"] = json!(true);
        assert!(public_track(&value).is_none());
        value["is_stream_gated"] = json!(false);
        value["access"] = json!({"stream": false});
        assert!(public_track(&value).is_none());
        value["access"] = json!({"stream": true});
        value["id"] = json!("../private");
        assert!(public_track(&value).is_none());
    }

    #[test]
    fn redirects_are_explicitly_allowed() {
        assert!(fixture_base("http://127.0.0.1:9000/v1").is_some());
        assert!(fixture_base("http://127.0.0.1:9000/v1?x=1").is_none());
        assert!(fixture_base("http://192.168.1.1:9000/v1").is_none());
        let allowed = vec!["media.example.com".to_string()];
        for valid in ["https://api.audius.co/v1/tracks", "https://media.example.com/a"] {
            assert!(safe_media_url(&Url::parse(valid).unwrap(), &allowed));
        }
        for invalid in [
            "http://media.example.com/a",
            "https://media.example.com.evil.test/a",
            "https://127.0.0.1/a",
            "https://user@media.example.com/a",
            "https://media.example.com:8443/a",
        ] {
            assert!(!safe_media_url(&Url::parse(invalid).unwrap(), &allowed));
        }
    }
}
