//! OAuth Authorization Code + PKCE и чтение плейлистов Spotify Web API.
use crate::{Error, ImportItem, Result, TrackMetadata, error::checked};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    time::{SystemTime, UNIX_EPOCH},
};
use url::Url;

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn random_secret() -> String {
    let mut bytes = [0; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Не реализует Debug, чтобы не раскрывать verifier и state.
pub struct PkceAuthorization {
    client_id: String,
    redirect_uri: Url,
    verifier: String,
    state: String,
}

impl PkceAuthorization {
    pub fn new(client_id: impl Into<String>, redirect_uri: &str) -> Result<Self> {
        let redirect_uri = Url::parse(redirect_uri).map_err(|e| Error::Invalid(e.to_string()))?;
        if redirect_uri.fragment().is_some() || redirect_uri.query().is_some() {
            return Err(Error::Invalid(
                "redirect URI must not contain query or fragment".into(),
            ));
        }
        let client_id = client_id.into();
        if client_id.trim().is_empty() {
            return Err(Error::Invalid("empty Spotify client ID".into()));
        }
        Ok(Self {
            client_id,
            redirect_uri,
            verifier: random_secret(),
            state: random_secret(),
        })
    }

    pub fn authorization_url(&self, scopes: &[&str]) -> Url {
        let mut url = Url::parse("https://accounts.spotify.com/authorize").expect("static URL");
        url.query_pairs_mut().extend_pairs([
            ("client_id", self.client_id.as_str()),
            ("response_type", "code"),
            ("redirect_uri", self.redirect_uri.as_str()),
            ("state", &self.state),
            ("code_challenge_method", "S256"),
            ("code_challenge", &pkce_challenge(&self.verifier)),
            ("scope", &scopes.join(" ")),
        ]);
        url
    }

    fn code(&self, callback: &str) -> Result<String> {
        let callback = Url::parse(callback).map_err(|e| Error::Invalid(e.to_string()))?;
        let mut base = callback.clone();
        base.set_query(None);
        if base != self.redirect_uri {
            return Err(Error::Invalid("OAuth callback URI mismatch".into()));
        }
        let pairs: Vec<_> = callback.query_pairs().collect();
        let states: Vec<_> = pairs.iter().filter(|(k, _)| k == "state").collect();
        if states.len() != 1 || states[0].1 != self.state {
            return Err(Error::Invalid("OAuth state mismatch".into()));
        }
        if pairs.iter().any(|(k, _)| k == "error") {
            return Err(Error::Invalid("Spotify authorization denied".into()));
        }
        let codes: Vec<_> = pairs.iter().filter(|(k, _)| k == "code").collect();
        if codes.len() != 1 || codes[0].1.is_empty() {
            return Err(Error::Invalid("missing or duplicate OAuth code".into()));
        }
        Ok(codes[0].1.to_string())
    }

    pub async fn exchange(self, callback: &str) -> Result<SpotifyClient> {
        let code = self.code(callback)?;
        let http = crate::http_client()?;
        let raw: TokenResponse = checked(
            http.post("https://accounts.spotify.com/api/token")
                .form(&[
                    ("grant_type", "authorization_code"),
                    ("code", &code),
                    ("redirect_uri", self.redirect_uri.as_str()),
                    ("client_id", &self.client_id),
                    ("code_verifier", &self.verifier),
                ])
                .send()
                .await?,
        )
        .await?
        .json()
        .await?;
        SpotifyClient::from_tokens(self.client_id, raw.into_tokens(None)?)
    }
}

/// Секреты сохраняет вызывающая сторона в подходящем защищённом хранилище.
#[derive(Clone, Serialize, Deserialize)]
pub struct SpotifyTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at_unix: u64,
    pub scope: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
    token_type: String,
    scope: Option<String>,
}
impl TokenResponse {
    fn into_tokens(self, previous: Option<&SpotifyTokens>) -> Result<SpotifyTokens> {
        if !self.token_type.eq_ignore_ascii_case("bearer") || self.access_token.is_empty() {
            return Err(Error::Protocol("invalid Spotify bearer token".into()));
        }
        let refresh_token = self
            .refresh_token
            .or_else(|| previous.map(|p| p.refresh_token.clone()))
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Protocol("missing refresh token".into()))?;
        Ok(SpotifyTokens {
            access_token: self.access_token,
            refresh_token,
            expires_at_unix: now().saturating_add(self.expires_in),
            scope: self
                .scope
                .unwrap_or_else(|| previous.map(|p| p.scope.clone()).unwrap_or_default()),
        })
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub public: Option<bool>,
}

pub struct SpotifyClient {
    http: reqwest::Client,
    client_id: String,
    tokens: SpotifyTokens,
    api: Url,
    token_url: Url,
}

impl SpotifyClient {
    pub fn from_tokens(client_id: impl Into<String>, tokens: SpotifyTokens) -> Result<Self> {
        let client_id = client_id.into();
        if client_id.trim().is_empty()
            || tokens.refresh_token.is_empty()
            || tokens.access_token.is_empty()
        {
            return Err(Error::Invalid(
                "Spotify client ID and tokens are required".into(),
            ));
        }
        Ok(Self {
            http: crate::http_client()?,
            client_id,
            tokens,
            api: Url::parse("https://api.spotify.com/v1/").expect("static URL"),
            token_url: Url::parse("https://accounts.spotify.com/api/token").expect("static URL"),
        })
    }
    pub fn tokens(&self) -> &SpotifyTokens {
        &self.tokens
    }

    pub async fn refresh(&mut self) -> Result<()> {
        let raw: TokenResponse = checked(
            self.http
                .post(self.token_url.clone())
                .form(&[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", self.tokens.refresh_token.as_str()),
                    ("client_id", self.client_id.as_str()),
                ])
                .send()
                .await?,
        )
        .await?
        .json()
        .await?;
        self.tokens = raw.into_tokens(Some(&self.tokens))?;
        Ok(())
    }

    fn validate_page(&self, url: &Url) -> Result<()> {
        if url.origin() != self.api.origin()
            || !url.path().starts_with(self.api.path())
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(Error::Protocol("unsafe Spotify pagination URL".into()));
        }
        Ok(())
    }

    async fn pages<T: serde::de::DeserializeOwned>(&mut self, path: &str) -> Result<Vec<T>> {
        #[derive(Deserialize)]
        struct Page<T> {
            items: Vec<T>,
            next: Option<String>,
        }
        let mut next = Some(
            self.api
                .join(path)
                .map_err(|e| Error::Invalid(e.to_string()))?,
        );
        let mut seen = HashSet::new();
        let mut items = Vec::new();
        while let Some(url) = next.take() {
            self.validate_page(&url)?;
            if seen.len() >= 10_000 || !seen.insert(url.to_string()) {
                return Err(Error::Protocol("pagination loop or limit".into()));
            }
            if self.tokens.expires_at_unix <= now().saturating_add(30) {
                self.refresh().await?;
            }
            let mut response = self
                .http
                .get(url.clone())
                .bearer_auth(&self.tokens.access_token)
                .send()
                .await?;
            if response.status() == reqwest::StatusCode::UNAUTHORIZED {
                self.refresh().await?;
                response = self
                    .http
                    .get(url.clone())
                    .bearer_auth(&self.tokens.access_token)
                    .send()
                    .await?;
            }
            let page: Page<T> = checked(response).await?.json().await?;
            items.extend(page.items);
            next = page
                .next
                .map(|s| url.join(&s).map_err(|e| Error::Protocol(e.to_string())))
                .transpose()?;
        }
        Ok(items)
    }

    pub async fn playlists(&mut self) -> Result<Vec<Playlist>> {
        self.pages("me/playlists?limit=50").await
    }

    pub async fn playlist_tracks(&mut self, playlist_id: &str) -> Result<Vec<ImportItem>> {
        if playlist_id.is_empty() || !playlist_id.bytes().all(|c| c.is_ascii_alphanumeric()) {
            return Err(Error::Invalid("invalid Spotify playlist ID".into()));
        }
        let entries: Vec<Value> = self
            .pages(&format!("playlists/{playlist_id}/items?limit=50"))
            .await?;
        Ok(entries.into_iter().map(track_item).collect())
    }
}

fn track_item(entry: Value) -> ImportItem {
    // API использует item; track встречается в старых ответах и сохранённых данных.
    let t = entry
        .get("item")
        .or_else(|| entry.get("track"))
        .unwrap_or(&Value::Null);
    let id = t["id"].as_str().map(str::to_owned);
    let unavailable = if t.is_null() {
        Some("removed or unavailable Spotify item")
    } else if entry["is_local"] == true || t["is_local"] == true {
        Some("local Spotify file")
    } else if t["type"].as_str() != Some("track") {
        Some("non-music Spotify item")
    } else if t["is_playable"] == false {
        Some("Spotify track is not playable in this market")
    } else if t["name"].as_str().is_none() {
        Some("Spotify item lacks title")
    } else {
        None
    };
    let metadata = unavailable.is_none().then(|| TrackMetadata {
        title: t["name"].as_str().unwrap_or_default().into(),
        artists: t["artists"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| a["name"].as_str().map(str::to_owned))
            .collect(),
        album: t["album"]["name"].as_str().map(str::to_owned),
        duration_ms: t["duration_ms"].as_u64(),
        isrc: t["external_ids"]["isrc"].as_str().map(str::to_owned),
        source_url: t["external_urls"]["spotify"].as_str().map(str::to_owned),
    });
    ImportItem {
        source_id: id,
        metadata,
        unavailable_reason: unavailable.map(str::to_owned),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rfc7636_vector() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }
    #[test]
    fn callback_state_and_origin() {
        let flow = PkceAuthorization::new("id", "http://127.0.0.1:9000/callback").unwrap();
        assert!(
            flow.code("http://127.0.0.1:9000/callback?state=wrong&code=x")
                .is_err()
        );
        assert!(
            flow.code(&format!(
                "http://evil.test/callback?state={}&code=x",
                flow.state
            ))
            .is_err()
        );
        assert_eq!(
            flow.code(&format!(
                "http://127.0.0.1:9000/callback?state={}&code=x",
                flow.state
            ))
            .unwrap(),
            "x"
        );
    }
    #[test]
    fn refresh_preserves_unrotated_token() {
        let old = SpotifyTokens {
            access_token: "old".into(),
            refresh_token: "refresh".into(),
            expires_at_unix: 0,
            scope: "scope".into(),
        };
        let raw = TokenResponse {
            access_token: "new".into(),
            refresh_token: None,
            expires_in: 3600,
            token_type: "Bearer".into(),
            scope: None,
        };
        let new = raw.into_tokens(Some(&old)).unwrap();
        assert_eq!(new.refresh_token, "refresh");
        assert_eq!(new.scope, "scope");
    }
    #[test]
    fn unavailable_items_keep_positions() {
        assert!(
            track_item(serde_json::json!({"item":null}))
                .metadata
                .is_none()
        );
        assert!(
            track_item(serde_json::json!({"track":{"type":"episode"}}))
                .unavailable_reason
                .is_some()
        );
    }
}
