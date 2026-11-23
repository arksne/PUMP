//! Импорт открытых метаданных Spotify Embed без OAuth и пользовательских ключей.
//! Публичная страница может содержать ограниченную выборку треков.
use crate::{Error, ImportItem, Result, TrackMetadata};
use serde::Serialize;
use serde_json::Value;
use url::Url;
#[derive(Clone, Debug, Serialize)]
pub struct PublicPlaylist {
    pub title: String,
    pub tracks: Vec<ImportItem>,
    pub possibly_truncated: bool,
}
#[derive(Clone)]
pub struct PublicSpotify {
    http: reqwest::Client,
}
impl PublicSpotify {
    pub fn new() -> Result<Self> {
        Ok(Self {
            http: crate::http_client()?,
        })
    }
    pub async fn playlist(&self, link: &str) -> Result<PublicPlaylist> {
        let id = playlist_id(link)?;
        let mut response = self
            .http
            .get(format!("https://open.spotify.com/embed/playlist/{id}"))
            .header("User-Agent", "PUMP public playlist importer")
            .send()
            .await?
            .error_for_status()?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if bytes.len() + chunk.len() > 8 * 1024 * 1024 {
                return Err(Error::Protocol(
                    "Spotify Embed response exceeds 8 MiB".into(),
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        let html = String::from_utf8(bytes)
            .map_err(|_| Error::Protocol("Spotify Embed returned invalid UTF-8".into()))?;
        parse_embed(&html)
    }
}
pub fn playlist_id(link: &str) -> Result<String> {
    let url =
        Url::parse(link).map_err(|_| Error::Invalid("Нужна ссылка Spotify playlist".into()))?;
    if url.scheme() != "https"
        || url.host_str() != Some("open.spotify.com")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(Error::Invalid("Нужна HTTPS-ссылка open.spotify.com".into()));
    }
    let parts: Vec<_> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    let parts = if parts.first().is_some_and(|s| s.starts_with("intl-")) {
        &parts[1..]
    } else {
        &parts[..]
    };
    let id = match parts {
        ["playlist", id] | ["embed", "playlist", id] => *id,
        _ => return Err(Error::Invalid("Нужна ссылка на плейлист Spotify".into())),
    };
    if id.len() != 22 || !id.bytes().all(|c| c.is_ascii_alphanumeric()) {
        return Err(Error::Invalid("Некорректный ID плейлиста Spotify".into()));
    }
    Ok(id.into())
}
pub fn parse_embed(html: &str) -> Result<PublicPlaylist> {
    let mut data = None;
    for script in html.split("<script").skip(1) {
        let Some((attributes, body)) = script.split_once('>') else {
            continue;
        };
        if (attributes.contains("id=\"__NEXT_DATA__\"")
            || attributes.contains("id='__NEXT_DATA__'"))
            && let Some((json, _)) = body.split_once("</script>")
        {
            data = Some(serde_json::from_str::<Value>(json)?);
            break;
        }
    }
    let data = data.ok_or_else(|| {
        Error::Protocol("Spotify не предоставил открытые метаданные. Можно загрузить CSV.".into())
    })?;
    let entity = &data["props"]["pageProps"]["state"]["data"]["entity"];
    let title = entity["title"]
        .as_str()
        .or_else(|| entity["name"].as_str())
        .ok_or_else(|| {
            Error::Unsupported(
                "Плейлист недоступен через публичный Embed. Можно загрузить CSV из Exportify."
                    .into(),
            )
        })?;
    let rows = entity["trackList"]
        .as_array()
        .ok_or_else(|| Error::Protocol("В Spotify Embed нет списка треков".into()))?;
    let tracks = rows
        .iter()
        .map(|track| {
            let uri = track["uri"].as_str().unwrap_or("");
            let id = uri.strip_prefix("spotify:track:");
            let metadata = track["title"]
                .as_str()
                .filter(|s| !s.is_empty())
                .and_then(|title| {
                    let artist = track["subtitle"].as_str().unwrap_or("");
                    if artist.is_empty() {
                        return None;
                    }
                    let mut artists = vec![artist.into()];
                    for part in artist.split(", ") {
                        if part != artist {
                            artists.push(part.into());
                        }
                    }
                    Some(TrackMetadata {
                        title: title.into(),
                        artists,
                        album: None,
                        duration_ms: track["duration"].as_u64(),
                        isrc: None,
                        source_url: id.map(|id| format!("https://open.spotify.com/track/{id}")),
                    })
                });
            let reason = if metadata.is_none() {
                Some("В открытой странице отсутствуют метаданные записи".into())
            } else {
                None
            };
            ImportItem {
                source_id: Some(uri.into()),
                metadata,
                unavailable_reason: reason,
            }
        })
        .collect();
    Ok(PublicPlaylist {
        title: title.into(),
        tracks,
        possibly_truncated: rows.len() >= 100,
    })
}

/// Читает CSV Exportify, сохраняя порядок, повторяющиеся записи и Unicode.
pub fn parse_exportify(input: &[u8]) -> Result<Vec<ImportItem>> {
    if input.len() > 16 * 1024 * 1024 {
        return Err(Error::Invalid("CSV больше 16 МБ".into()));
    }
    let mut csv = csv::ReaderBuilder::new().flexible(false).from_reader(input);
    let headers = csv
        .headers()
        .map_err(|e| Error::Protocol(e.to_string()))?
        .clone();
    let field = |names: &[&str]| {
        headers.iter().position(|h| {
            names
                .iter()
                .any(|name| h.trim_start_matches('\u{feff}').eq_ignore_ascii_case(name))
        })
    };
    let title = field(&["Track Name", "Track name", "Name"])
        .ok_or_else(|| Error::Invalid("В CSV нет колонки Track Name".into()))?;
    let artist = field(&["Artist Name(s)", "Artist Name", "Artists", "Artist"])
        .ok_or_else(|| Error::Invalid("В CSV нет колонки Artist Name(s)".into()))?;
    let uri = field(&["Track URI", "Spotify URI", "URI"]);
    let duration = field(&["Duration (ms)", "Duration_ms"]);
    let album = field(&["Album Name", "Album"]);
    let isrc = field(&["ISRC"]);
    let mut tracks = Vec::new();
    for row in csv.records() {
        let row = row.map_err(|e| Error::Protocol(e.to_string()))?;
        if tracks.len() >= 10000 {
            return Err(Error::Invalid("В CSV больше 10 000 записей".into()));
        }
        let get = |i: Option<usize>| {
            i.and_then(|i| row.get(i))
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        let name = row.get(title).unwrap_or("");
        let artist = row.get(artist).unwrap_or("");
        let metadata = if name.is_empty() || artist.is_empty() {
            None
        } else {
            Some(TrackMetadata {
                title: name.into(),
                artists: artist.split(';').map(|s| s.trim().to_owned()).collect(),
                album: get(album),
                duration_ms: get(duration).and_then(|v| v.parse().ok()),
                isrc: get(isrc),
                source_url: get(uri).and_then(|v| {
                    v.strip_prefix("spotify:track:")
                        .map(|id| format!("https://open.spotify.com/track/{id}"))
                }),
            })
        };
        let unavailable_reason = if metadata.is_none() {
            Some("Пустое название или исполнитель в CSV".into())
        } else {
            None
        };
        tracks.push(ImportItem {
            source_id: get(uri),
            metadata,
            unavailable_reason,
        });
    }
    Ok(tracks)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn public_link_variants() {
        assert_eq!(
            playlist_id("https://open.spotify.com/intl-ru/playlist/3cEYpjA9oz9GiPac4AsH4n?si=test")
                .unwrap(),
            "3cEYpjA9oz9GiPac4AsH4n"
        );
        assert!(playlist_id("https://evil.test/playlist/3cEYpjA9oz9GiPac4AsH4n").is_err());
    }
    #[test]
    fn csv_preserves_quotes_unicode_and_duplicate_tracks() {
        let csv = "Track URI,Track Name,Artist Name(s),Duration (ms)\nspotify:track:x,\"Трек, один\",Артист,120000\nspotify:track:x,\"Трек, один\",Артист,120000\n";
        let tracks = parse_exportify(csv.as_bytes()).unwrap();
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].metadata.as_ref().unwrap().title, "Трек, один");
        assert_eq!(
            tracks[0].metadata.as_ref().unwrap().duration_ms,
            Some(120000)
        );
    }
    #[test]
    fn embed_parses_metadata_and_flags_partial_list() {
        let item = serde_json::json!({"uri":"spotify:track:a","title":"Song","subtitle":"Artist","duration":100000});
        let data = serde_json::json!({"props":{"pageProps":{"state":{"data":{"entity":{"title":"Playlist","trackList":vec![item;100]}}}}}});
        let html =
            format!("<script id=\"__NEXT_DATA__\" type=\"application/json\">{data}</script>");
        let playlist = parse_embed(&html).unwrap();
        assert_eq!(playlist.tracks.len(), 100);
        assert!(playlist.possibly_truncated);
        assert_eq!(
            playlist.tracks[0].metadata.as_ref().unwrap().artists,
            ["Artist"]
        );
    }
}
