//! Общие ресурсы сервера и небольшие утилиты браузерного клиента.

pub const INDEX_HTML: &str = include_str!("../../../index.html");
pub const APP_JS: &str = include_str!("../../../web/app.js");
pub const UI_JS: &str = include_str!("../../../web/ui.js");
pub const UI_CSS: &str = include_str!("../../../web/ui.css");
pub const SERVICE_WORKER_JS: &str = include_str!("../../../web/sw.js");

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

/// Все слова запроса должны встречаться в описании настройки.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
pub fn settings_match(query: &str, description: &str) -> bool {
    let description = description.to_lowercase();
    query
        .to_lowercase()
        .split_whitespace()
        .all(|word| description.contains(word))
}

/// Непрозрачные пиксели обложки дают спокойный цвет для стекла.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
pub fn palette_rgb(rgba: &[u8]) -> u32 {
    let mut sum = [0_u64; 3];
    let mut count = 0_u64;
    for pixel in rgba
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|pixel| pixel[3] > 127)
    {
        for channel in 0..3 {
            sum[channel] += u64::from(pixel[channel]);
        }
        count += 1;
    }
    if count == 0 {
        return 0x555566;
    }
    ((sum[0] / count) as u32) << 16 | ((sum[1] / count) as u32) << 8 | (sum[2] / count) as u32
}

#[cfg(not(target_arch = "wasm32"))]
pub const WASM_JS: &str = include_str!("../../../web/pkg/ui_liquid_glass.js");
#[cfg(not(target_arch = "wasm32"))]
pub const WASM_BYTES: &[u8] = include_bytes!("../../../web/pkg/ui_liquid_glass_bg.wasm");

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn search_matches_all_words_and_cyrillic() {
        assert!(settings_match("ТЕКСТ blur", "Текст песни blur glow"));
        assert!(!settings_match("текст радио", "Текст песни blur glow"));
    }
    #[test]
    fn palette_ignores_transparent_pixels() {
        assert_eq!(palette_rgb(&[255, 0, 0, 255, 0, 255, 0, 0]), 0xff0000);
        assert_eq!(palette_rgb(&[]), 0x555566);
    }
}
