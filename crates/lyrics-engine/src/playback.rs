//! Поиск активных строк и слов по временным интервалам.
use serde::{Deserialize, Serialize};

use crate::Lyrics;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveWord {
    pub line_index: usize,
    pub word_index: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybackPosition {
    /// Индексы исходных строк, включая одновременно звучащие голоса.
    pub line_indices: Vec<usize>,
    pub words: Vec<ActiveWord>,
}

#[derive(Debug)]
struct IntervalIndex {
    entries: Vec<(u64, Option<u64>, usize)>,
    max_ends: Vec<u128>,
}

impl IntervalIndex {
    fn new(mut entries: Vec<(u64, Option<u64>, usize)>) -> Self {
        entries.sort_by_key(|&(start, _, index)| (start, index));
        let mut maximum = 0;
        let max_ends = entries
            .iter()
            .map(|&(_, end, _)| {
                maximum = maximum.max(end.map(u128::from).unwrap_or(u128::MAX));
                maximum
            })
            .collect();
        Self { entries, max_ends }
    }

    fn at(&self, time_ms: u64) -> Vec<usize> {
        let mut cursor = self
            .entries
            .partition_point(|&(start, _, _)| start <= time_ms);
        let mut active = Vec::new();
        while cursor > 0 {
            cursor -= 1;
            if self.max_ends[cursor] <= u128::from(time_ms) {
                break;
            }
            let (_, end, index) = self.entries[cursor];
            if end.is_none_or(|end| time_ms < end) {
                active.push(index);
            }
        }
        active.reverse();
        active
    }
}

/// Индекс создаётся один раз; двоичный поиск поддерживает перемотку и перекрытия.
#[derive(Debug)]
pub struct PlaybackIndex<'a> {
    lyrics: &'a Lyrics,
    lines: IntervalIndex,
    words: Vec<IntervalIndex>,
}

impl<'a> PlaybackIndex<'a> {
    pub fn new(lyrics: &'a Lyrics) -> Self {
        let lines = IntervalIndex::new(
            lyrics
                .lines
                .iter()
                .enumerate()
                .filter_map(|(i, line)| line.start_ms.map(|start| (start, line.end_ms, i)))
                .collect(),
        );
        let words = lyrics
            .lines
            .iter()
            .map(|line| {
                IntervalIndex::new(
                    line.words
                        .iter()
                        .enumerate()
                        .map(|(i, word)| (word.start_ms, word.end_ms, i))
                        .collect(),
                )
            })
            .collect();
        Self {
            lyrics,
            lines,
            words,
        }
    }

    pub fn lyrics(&self) -> &'a Lyrics {
        self.lyrics
    }

    pub fn at(&self, time_ms: u64) -> PlaybackPosition {
        let line_indices = self.lines.at(time_ms);
        let words = line_indices
            .iter()
            .flat_map(|&line_index| {
                self.words[line_index]
                    .at(time_ms)
                    .into_iter()
                    .map(move |word_index| ActiveWord {
                        line_index,
                        word_index,
                    })
            })
            .collect();
        PlaybackPosition {
            line_indices,
            words,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Alignment, LyricLine, Lyrics, LyricsFormat, TimedWord};

    fn word(start: u64, end: Option<u64>) -> TimedWord {
        TimedWord {
            text: "w".into(),
            start_ms: start,
            end_ms: end,
            singer_ids: Vec::new(),
            alignment: Alignment::Auto,
            role: None,
        }
    }

    fn line(start: u64, end: Option<u64>, words: Vec<(u64, Option<u64>)>) -> LyricLine {
        LyricLine {
            text: String::new(),
            start_ms: Some(start),
            end_ms: end,
            words: words.into_iter().map(|(s, e)| word(s, e)).collect(),
            singer_ids: Vec::new(),
            alignment: Alignment::Auto,
            language: None,
            role: None,
        }
    }

    fn doc() -> Lyrics {
        Lyrics {
            format: LyricsFormat::Lrc,
            lines: vec![
                line(
                    1000,
                    Some(3000),
                    vec![(1000, Some(2000)), (2000, Some(3000))],
                ),
                line(2500, Some(5000), vec![(2500, None)]),
                line(6000, None, vec![]),
            ],
            singers: Vec::new(),
            metadata: Default::default(),
            instrumental: false,
        }
    }

    #[test]
    fn seek_finds_line_and_word_by_time() {
        let lyrics = doc();
        let index = PlaybackIndex::new(&lyrics);
        let at = index.at(1500);
        assert_eq!(at.line_indices, vec![0]);
        assert_eq!(
            at.words,
            vec![ActiveWord {
                line_index: 0,
                word_index: 0
            }]
        );
    }

    #[test]
    fn overlapping_lines_and_open_ended_words() {
        let lyrics = doc();
        let index = PlaybackIndex::new(&lyrics);
        // 2600 ms: line 0 (1000..3000) and line 1 (2500..5000) overlap.
        let at = index.at(2600);
        assert_eq!(at.line_indices, vec![0, 1]);
        assert_eq!(
            at.words,
            vec![
                ActiveWord {
                    line_index: 0,
                    word_index: 1
                },
                ActiveWord {
                    line_index: 1,
                    word_index: 0
                },
            ]
        );
        // Open-ended word stays active; line without times never matches.
        let at = index.at(9000);
        assert_eq!(at.line_indices, vec![2]);
        assert!(at.words.is_empty());
    }

    #[test]
    fn seeking_backwards_returns_earlier_position() {
        let lyrics = doc();
        let index = PlaybackIndex::new(&lyrics);
        assert_eq!(index.at(4500).line_indices, vec![1]);
        assert_eq!(index.at(1200).line_indices, vec![0]);
        assert!(index.at(0).line_indices.is_empty());
        assert!(index.at(500).words.is_empty());
    }
}
