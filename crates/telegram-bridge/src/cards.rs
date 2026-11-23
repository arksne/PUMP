//! PNG-карточка итогов. Шрифт DejaVu поддерживает имена треков на кириллице.
use ab_glyph::FontRef;
use image::{ImageFormat, Rgb, RgbImage};
use imageproc::drawing::{draw_filled_rect_mut, draw_text_mut, text_size};
use imageproc::rect::Rect;
use library_storage::sessions::{Period, Summary};
use std::io::Cursor;
pub fn render(stats: &Summary) -> anyhow::Result<Vec<u8>> {
    let font = FontRef::try_from_slice(include_bytes!("../assets/DejaVuSans.ttf"))?;
    let mut image = RgbImage::from_pixel(1000, 920, Rgb([17, 18, 23]));
    let white = Rgb([245, 245, 247]);
    let muted = Rgb([171, 175, 187]);
    draw_text_mut(&mut image, white, 56, 48, 48.0, &font, "PUMP");
    let period = match stats.period {
        Period::Week => "Итоги за 7 дней",
        Period::Month => "Итоги за 30 дней",
    };
    draw_text_mut(&mut image, muted, 56, 122, 32.0, &font, period);
    draw_text_mut(
        &mut image,
        white,
        56,
        195,
        56.0,
        &font,
        &format!("{} минут", stats.milliseconds.max(0) / 60000),
    );
    draw_text_mut(
        &mut image,
        muted,
        56,
        270,
        26.0,
        &font,
        &format!("Треков в истории — {}", stats.tracks),
    );
    draw_text_mut(&mut image, white, 56, 350, 30.0, &font, "Чаще всего слушал");
    let maximum = stats
        .top_tracks
        .iter()
        .map(|track| track.milliseconds)
        .max()
        .unwrap_or(1)
        .max(1);
    for (index, track) in stats.top_tracks.iter().take(5).enumerate() {
        let y = 410 + index as i32 * 92;
        let mut title = format!("{} — {}", track.artist, track.title);
        while text_size(24.0, &font, &title).0 > 880 {
            title.pop();
        }
        draw_text_mut(&mut image, white, 56, y, 24.0, &font, &title);
        let width = (880 * track.milliseconds.max(0) / maximum).max(1) as u32;
        draw_filled_rect_mut(&mut image, Rect::at(56, y + 44).of_size(width, 5), muted);
    }
    let mut bytes = Cursor::new(Vec::new());
    image.write_to(&mut bytes, ImageFormat::Png)?;
    Ok(bytes.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn renders_russian_track_metadata_into_png() {
        let stats = Summary {
            period: Period::Month,
            milliseconds: 125000,
            tracks: 1,
            top_tracks: vec![library_storage::sessions::Rank {
                title: "Название трека".into(),
                artist: "Исполнитель".into(),
                milliseconds: 125000,
            }],
            top_artists: vec![],
        };
        let png = render(&stats).unwrap();
        let decoded = image::load_from_memory(&png).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (1000, 920));
    }
}
