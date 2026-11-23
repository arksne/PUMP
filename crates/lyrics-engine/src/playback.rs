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
