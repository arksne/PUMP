//! Проверка открытого плейлиста без OAuth. Ссылка передаётся первым аргументом.
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let link = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "https://open.spotify.com/playlist/3cEYpjA9oz9GiPac4AsH4n".into());
    let playlist = social_integrations::spotify_public::PublicSpotify::new()?
        .playlist(&link)
        .await?;
    println!("{} — {} записей", playlist.title, playlist.tracks.len());
    for item in playlist.tracks {
        if let Some(track) = item.metadata {
            println!("{} — {}", track.artists.join(", "), track.title);
        }
    }
    if playlist.possibly_truncated {
        println!(
            "Embed может содержать только часть плейлиста. Для полного импорта используй CSV."
        );
    }
    Ok(())
}
