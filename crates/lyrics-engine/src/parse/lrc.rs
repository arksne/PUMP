//! Разбор обычного и пословного LRC с сохранением смещений времени.
use crate::{LyricLine, Lyrics, LyricsFormat, TimedWord};

use super::{ParseError, ParseOptions, check_size, decimal, milliseconds, parse_plain};

fn timestamp(value: &str) -> Result<u64, ParseError> {
    let fail = || ParseError::InvalidTimestamp(value.to_owned());
    let parts: Vec<_> = value.split(':').collect();
    let (minutes, seconds) = match parts.as_slice() {
        [m, s] => (
            decimal(m).filter(|_| !m.contains('.')).ok_or_else(fail)?,
            decimal(s).ok_or_else(fail)?,
        ),
        [m, s, fraction] if !fraction.contains('.') => {
            let minutes = decimal(m).filter(|_| !m.contains('.')).ok_or_else(fail)?;
            let seconds = decimal(s).filter(|_| !s.contains('.')).ok_or_else(fail)?;
            if fraction.is_empty() || fraction.len() > 3 || seconds >= 60.0 {
                return Err(fail());
            }
            let sub = decimal(fraction).ok_or_else(fail)? / 10_f64.powi(fraction.len() as i32);
            (minutes, seconds + sub)
        }
        [h, m, s] => {
            let hours = decimal(h).filter(|_| !h.contains('.')).ok_or_else(fail)?;
            let minutes = decimal(m).filter(|_| !m.contains('.')).ok_or_else(fail)?;
            if minutes >= 60.0 {
                return Err(fail());
            }
            (hours * 60.0 + minutes, decimal(s).ok_or_else(fail)?)
        }
        _ => return Err(fail()),
    };
    if seconds >= 60.0 {
        return Err(fail());
    }
    milliseconds(minutes * 60.0 + seconds, value)
}

fn shift(time: u64, delta: i128) -> u64 {
    (i128::from(time) + delta).clamp(0, i128::from(u64::MAX)) as u64
}

fn words(text: &str, line_start: u64) -> Result<(String, Vec<TimedWord>, Option<u64>), ParseError> {
    let mut rest = text;
    let mut plain = String::new();
    let mut result: Vec<TimedWord> = Vec::new();
    let mut current: Option<usize> = None;
    let mut last_marker = None;
    while let Some(open) = rest.find('<') {
        let Some(close) = rest[open + 1..].find('>').map(|i| i + open + 1) else {
            break;
        };
        let tag = &rest[open + 1..close];
        if !tag.as_bytes().first().is_some_and(u8::is_ascii_digit) || !tag.contains(':') {
            let literal = &rest[..close + 1];
            plain.push_str(literal);
            if let Some(index) = current {
                result[index].text.push_str(literal);
            }
            rest = &rest[close + 1..];
            continue;
        }
        let time = timestamp(tag)?;
        if time < line_start || last_marker.is_some_and(|last| time < last) {
            return Err(ParseError::InvalidTiming(
                "LRC word timestamps decrease or precede the line".into(),
            ));
        }
        let segment = &rest[..open];
        plain.push_str(segment);
        if let Some(index) = current {
            result[index].text.push_str(segment);
            result[index].end_ms = Some(time);
        }
        result.push(TimedWord {
            text: String::new(),
            start_ms: time,
            end_ms: None,
            singer_ids: Vec::new(),
            alignment: Default::default(),
            role: None,
        });
        current = Some(result.len() - 1);
        last_marker = Some(time);
        rest = &rest[close + 1..];
    }
    plain.push_str(rest);
    if let Some(index) = current {
        result[index].text.push_str(rest);
    }
    let terminal = result
        .last()
        .filter(|word| word.text.trim().is_empty())
        .map(|word| word.start_ms);
    result.retain(|word| !word.text.trim().is_empty());
    Ok((plain, result, terminal))
}

pub fn parse_lrc(input: &str, options: ParseOptions) -> Result<Lyrics, ParseError> {
    check_size(input)?;
    let mut lyrics = Lyrics::empty(LyricsFormat::Lrc);
    let mut offset = 0_i64;
    for raw in input.trim_start_matches('\u{feff}').lines() {
        let mut rest = raw.trim();
        let mut times = Vec::new();
        let mut voices = Vec::new();
        while let Some(tag_rest) = rest.strip_prefix('[') {
            let Some(close) = tag_rest.find(']') else {
                if tag_rest.as_bytes().first().is_some_and(u8::is_ascii_digit) {
                    return Err(ParseError::InvalidTimestamp(tag_rest.to_owned()));
                }
                break;
            };
            let tag = &tag_rest[..close];
            if tag.as_bytes().first().is_some_and(u8::is_ascii_digit) && tag.contains(':') {
                times.push(timestamp(tag)?);
            } else if let Some((key, value)) = tag.split_once(':') {
                match key.trim().to_ascii_lowercase().as_str() {
                    "offset" => {
                        offset = value
                            .trim()
                            .parse()
                            .map_err(|_| ParseError::InvalidTimestamp(tag.into()))?
                    }
                    "voice" | "singer" => {
                        voices = value.split_whitespace().map(str::to_owned).collect()
                    }
                    _ => {
                        lyrics
                            .metadata
                            .entry(key.trim().to_ascii_lowercase())
                            .or_default()
                            .push(value.trim().into());
                    }
                }
            } else {
                break;
            }
            rest = tag_rest[close + 1..].trim_start();
        }
        if times.is_empty() {
            continue;
        }
        if let Some((voice, body)) = rest.split_once(':') {
            if voice
                .strip_prefix('v')
                .is_some_and(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()))
            {
                voices = vec![voice.to_owned()];
                rest = body.trim_start();
            }
        }
        let (text, timed_words, terminal) = words(rest, times[0])?;
        if !timed_words.is_empty() {
            lyrics.format = LyricsFormat::EnhancedLrc;
        }
        for &time in &times {
            let delta = i128::from(time) - i128::from(times[0]);
            let mut line = LyricLine::plain(text.clone());
            line.start_ms = Some(time);
            line.end_ms = terminal.map(|end| shift(end, delta));
            line.singer_ids = voices.clone();
            line.words = timed_words
                .iter()
                .cloned()
                .map(|mut word| {
                    word.start_ms = shift(word.start_ms, delta);
                    word.end_ms = word.end_ms.map(|end| shift(end, delta));
                    word.singer_ids = voices.clone();
                    word
                })
                .collect();
            lyrics.lines.push(line);
        }
    }
    if lyrics.lines.is_empty() {
        return Ok(parse_plain(input));
    }
    for line in &mut lyrics.lines {
        line.start_ms = line.start_ms.map(|start| shift(start, i128::from(offset)));
        line.end_ms = line.end_ms.map(|end| shift(end, i128::from(offset)));
        for word in &mut line.words {
            word.start_ms = shift(word.start_ms, i128::from(offset));
            word.end_ms = word.end_ms.map(|end| shift(end, i128::from(offset)));
        }
    }
    lyrics.lines.sort_by_key(|line| line.start_ms);
    let starts: Vec<_> = lyrics
        .lines
        .iter()
        .map(|line| line.start_ms.unwrap())
        .collect();
    for line in &mut lyrics.lines {
        let start = line.start_ms.unwrap();
        let next = starts.partition_point(|&other| other <= start);
        let boundary = starts.get(next).copied().or(options.duration_ms);
        line.end_ms = match (line.end_ms, boundary) {
            (Some(a), Some(b)) => Some(a.min(b).max(start)),
            (a, b) => a.or(b).map(|end| end.max(start)),
        };
        for word in &mut line.words {
            word.start_ms = line
                .end_ms
                .map_or(word.start_ms, |end| word.start_ms.min(end));
            word.end_ms = match (word.end_ms, line.end_ms) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
        }
    }
    lyrics.register_singers();
    Ok(lyrics)
}
