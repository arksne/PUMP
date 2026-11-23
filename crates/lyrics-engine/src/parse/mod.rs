//! Определение формата текста и общие проверки временных отметок.
mod lrc;
mod ttml;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{LyricLine, Lyrics, LyricsFormat};

pub use lrc::parse_lrc;
pub use ttml::parse_ttml;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ParseOptions {
    /// Позволяет ограничить последнюю строку длительностью записи.
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ParseError {
    #[error("invalid timestamp: {0}")]
    InvalidTimestamp(String),
    #[error("invalid timing: {0}")]
    InvalidTiming(String),
    #[error("invalid XML: {0}")]
    InvalidXml(String),
    #[error("unsupported TTML feature: {0}")]
    Unsupported(String),
    #[error("lyrics input exceeds 4 MiB")]
    InputTooLarge,
}

pub(crate) fn check_size(input: &str) -> Result<(), ParseError> {
    if input.len() > 4 * 1024 * 1024 {
        Err(ParseError::InputTooLarge)
    } else {
        Ok(())
    }
}

pub fn parse(input: &str, options: ParseOptions) -> Result<Lyrics, ParseError> {
    check_size(input)?;
    let trimmed = input.trim_start_matches('\u{feff}').trim_start();
    if trimmed.starts_with("<?xml")
        || trimmed.starts_with("<tt")
        || (trimmed.starts_with('<')
            && trimmed
                .split_once('>')
                .is_some_and(|(tag, _)| tag.contains(":tt")))
    {
        parse_ttml(trimmed, options)
    } else {
        parse_lrc(trimmed, options)
    }
}

pub fn parse_plain(input: &str) -> Lyrics {
    let mut lyrics = Lyrics::empty(LyricsFormat::Plain);
    lyrics.lines = input
        .trim_start_matches('\u{feff}')
        .lines()
        .map(|line| LyricLine::plain(line.trim_end_matches('\r').to_owned()))
        .collect();
    lyrics
}

pub(crate) fn milliseconds(seconds: f64, original: &str) -> Result<u64, ParseError> {
    let ms = seconds * 1000.0;
    if !ms.is_finite() || ms < 0.0 || ms >= u64::MAX as f64 {
        return Err(ParseError::InvalidTimestamp(original.to_owned()));
    }
    Ok(ms.round() as u64)
}

pub(crate) fn decimal(value: &str) -> Option<f64> {
    if value.is_empty()
        || !value.chars().all(|c| c.is_ascii_digit() || c == '.')
        || value.chars().filter(|&c| c == '.').count() > 1
    {
        return None;
    }
    value.parse::<f64>().ok().filter(|v| v.is_finite())
}
