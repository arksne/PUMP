//! Публичные метаданные через документированный iTunes Search API (Lookup).
//! MusicKit, HTML-скрейпинг и неофициальные токены здесь не используются.
use crate::{Error, ImportItem, Result, TrackMetadata, error::checked};
use serde::Deserialize;
use serde_json::Value;
use url::Url;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppleLink {
    Song { country: String, id: u64 },
    Album { country: String, id: u64 },
}

impl AppleLink {
    pub fn parse(link: &str) -> Result<Self> {
        let url = Url::parse(link).map_err(|e| Error::Invalid(e.to_string()))?;
        if url.scheme() != "https"
            || url.host_str() != Some("music.apple.com")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.port().is_some()
        {
            return Err(Error::Invalid(
                "expected https://music.apple.com public link".into(),
            ));
        }
        let parts: Vec<_> = url.path_segments().into_iter().flatten().collect();
        if parts.len() < 3
            || parts[0].len() != 2
            || !parts[0].bytes().all(|c| c.is_ascii_alphabetic())
        {
            return Err(Error::Invalid(
                "Apple link needs a two-letter storefront".into(),
            ));
        }
        let country = parts[0].to_uppercase();
        if parts[1] == "playlist" {
            return Err(Error::Unsupported("Apple Music playlist contents are not exposed by public iTunes Lookup; MusicKit authorization is required".into()));
        }
        if !matches!(parts[1], "album" | "song") {
            return Err(Error::Unsupported(
                "only public Apple album and song links are supported".into(),
            ));
        }
        let song_ids: Vec<_> = url
            .query_pairs()
            .filter(|(k, _)| k == "i")
            .map(|(_, v)| v.into_owned())
            .collect();
        if song_ids.len() > 1 {
            return Err(Error::Invalid("duplicate Apple song ID".into()));
        }
        let raw = song_ids
            .first()
            .map(String::as_str)
            .unwrap_or(parts.last().copied().unwrap_or_default());
        let id: u64 = raw
            .parse()
            .map_err(|_| Error::Invalid("Apple catalog ID must be numeric".into()))?;
        if id == 0 {
            return Err(Error::Invalid("zero Apple catalog ID".into()));
        }
        if !song_ids.is_empty() || parts[1] == "song" {
            Ok(Self::Song { country, id })
        } else {
            Ok(Self::Album { country, id })
        }
    }
}

pub struct AppleMusicClient {
    http: reqwest::Client,
}
impl AppleMusicClient {
    pub fn new() -> Result<Self> {
        Ok(Self {
            http: crate::http_client()?,
        })
    }

    pub async fn resolve_link(&self, link: &str) -> Result<Vec<ImportItem>> {
        let parsed = AppleLink::parse(link)?;
        let (country, id) = match &parsed {
            AppleLink::Song { country, id } | AppleLink::Album { country, id } => (country, id),
        };
        #[derive(Deserialize)]
        struct Lookup {
            results: Vec<Value>,
        }
        let response: Lookup = checked(
            self.http
                .get("https://itunes.apple.com/lookup")
                .query(&[
                    ("id", id.to_string()),
                    ("country", country.clone()),
                    ("entity", "song".into()),
                    ("limit", "200".into()),
                ])
                .send()
                .await?,
        )
        .await?
        .json()
        .await?;
        // Lookup не имеет пагинации: никогда не выдаём усечённый альбом за полный.
        let expected = response
            .results
            .iter()
            .find(|r| r["wrapperType"] == "collection")
            .and_then(|r| r["trackCount"].as_u64());
        let mut songs: Vec<_> = response
            .results
            .into_iter()
            .filter(|r| r["kind"] == "song")
            .filter(|r| match parsed {
                AppleLink::Song { id, .. } => r["trackId"].as_u64() == Some(id),
                AppleLink::Album { id, .. } => r["collectionId"].as_u64() == Some(id),
            })
            .collect();
        if songs.is_empty() {
            return Err(Error::Unsupported("public iTunes metadata is unavailable for this Apple Music catalog item/storefront".into()));
        }
        if matches!(parsed, AppleLink::Album { .. })
            && expected.is_none_or(|n| n != songs.len() as u64)
        {
            return Err(Error::Unsupported("public Lookup returned an incomplete album; complete MusicKit catalog access is required".into()));
        }
        songs.sort_by_key(|r| {
            (
                r["discNumber"].as_u64().unwrap_or(1),
                r["trackNumber"].as_u64().unwrap_or(0),
            )
        });
        songs
            .into_iter()
            .map(|r| {
                let title = r["trackName"]
                    .as_str()
                    .ok_or_else(|| Error::Protocol("Apple song lacks title".into()))?;
                let artist = r["artistName"]
                    .as_str()
                    .ok_or_else(|| Error::Protocol("Apple song lacks artist".into()))?;
                Ok(ImportItem {
                    source_id: r["trackId"].as_u64().map(|id| id.to_string()),
                    unavailable_reason: None,
                    metadata: Some(TrackMetadata {
                        title: title.into(),
                        artists: vec![artist.into()],
                        album: r["collectionName"].as_str().map(str::to_owned),
                        duration_ms: r["trackTimeMillis"].as_u64(),
                        isrc: None,
                        source_url: r["trackViewUrl"].as_str().map(str::to_owned),
                    }),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn public_links_and_honest_limits() {
        assert_eq!(
            AppleLink::parse("https://music.apple.com/us/album/example/123?i=456").unwrap(),
            AppleLink::Song {
                country: "US".into(),
                id: 456
            }
        );
        assert!(matches!(
            AppleLink::parse("https://music.apple.com/us/playlist/example/pl.123"),
            Err(Error::Unsupported(_))
        ));
        assert!(AppleLink::parse("https://music.apple.com.evil/us/song/123").is_err());
    }
}
