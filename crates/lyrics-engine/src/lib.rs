//! Независимый движок текстов: время задаётся в миллисекундах от начала записи.

mod client;
mod model;
mod parse;
mod playback;

pub use client::*;
pub use model::*;
pub use parse::{ParseError, ParseOptions, parse, parse_lrc, parse_plain, parse_ttml};
pub use playback::{ActiveWord, PlaybackIndex, PlaybackPosition};
