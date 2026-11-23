//! Типы данных и сериализация публичного контракта модуля.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LyricsFormat {
    Plain,
    Lrc,
    EnhancedLrc,
    Ttml,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Alignment {
    #[default]
    Auto,
    Start,
    Center,
    End,
    Left,
    Right,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Singer {
    pub id: String,
    pub name: Option<String>,
    pub kind: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimedWord {
    pub text: String,
    pub start_ms: u64,
    /// Исключительная граница; отсутствие означает неизвестный конец.
    pub end_ms: Option<u64>,
    pub singer_ids: Vec<String>,
    pub alignment: Alignment,
    pub role: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LyricLine {
    pub text: String,
    /// У обычного текста обе временные границы отсутствуют.
    pub start_ms: Option<u64>,
    pub end_ms: Option<u64>,
    pub words: Vec<TimedWord>,
    pub singer_ids: Vec<String>,
    pub alignment: Alignment,
    pub language: Option<String>,
    pub role: Option<String>,
}

impl LyricLine {
    pub(crate) fn plain(text: String) -> Self {
        Self {
            text,
            start_ms: None,
            end_ms: None,
            words: Vec::new(),
            singer_ids: Vec::new(),
            alignment: Alignment::Auto,
            language: None,
            role: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lyrics {
    pub format: LyricsFormat,
    pub lines: Vec<LyricLine>,
    pub singers: Vec<Singer>,
    pub metadata: BTreeMap<String, Vec<String>>,
    pub instrumental: bool,
}

impl Lyrics {
    pub fn playback_index(&self) -> crate::PlaybackIndex<'_> {
        crate::PlaybackIndex::new(self)
    }

    pub fn has_text(&self) -> bool {
        self.lines.iter().any(|line| !line.text.trim().is_empty())
    }

    pub(crate) fn empty(format: LyricsFormat) -> Self {
        Self {
            format,
            lines: Vec::new(),
            singers: Vec::new(),
            metadata: BTreeMap::new(),
            instrumental: false,
        }
    }

    pub(crate) fn register_singers(&mut self) {
        let ids: Vec<_> = self
            .lines
            .iter()
            .flat_map(|line| {
                line.singer_ids
                    .iter()
                    .chain(line.words.iter().flat_map(|word| &word.singer_ids))
            })
            .cloned()
            .collect();
        for id in ids {
            if !self.singers.iter().any(|singer| singer.id == id) {
                self.singers.push(Singer {
                    id,
                    name: None,
                    kind: None,
                });
            }
        }
    }
}

/// Только данные для интерфейса; загрузку шрифта и отрисовку выполняет приложение.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FontSettings {
    pub family: String,
    pub source: Option<String>,
    pub size_px: Option<f32>,
    pub weight: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GlowSettings {
    pub color_rgba: [u8; 4],
    pub radius_px: f32,
    pub intensity: f32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LyricsAppearance {
    pub font: Option<FontSettings>,
    pub inactive_blur_px: Option<f32>,
    pub active_glow: Option<GlowSettings>,
}
