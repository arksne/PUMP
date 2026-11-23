//! Разбор TTML с временными границами строк, слов и голосов.
use std::collections::HashMap;

use roxmltree::{Document, Node, NodeId};

use crate::{Alignment, LyricLine, Lyrics, LyricsFormat, Singer, TimedWord};

use super::{ParseError, ParseOptions, check_size, decimal, milliseconds};

const TT: &str = "http://www.w3.org/ns/ttml";
const OLD_TT: &str = "http://www.w3.org/2006/10/ttaf1";
const XML: &str = "http://www.w3.org/XML/1998/namespace";

fn extension<'a>(node: Node<'a, '_>, family: &str, name: &str) -> Option<&'a str> {
    node.attributes()
        .find(|attr| {
            attr.name() == name
                && attr.namespace().is_some_and(|ns| {
                    ns == format!("{TT}#{family}") || ns == format!("{OLD_TT}#{family}")
                })
        })
        .map(|attr| attr.value())
}

fn is_tt(node: Node<'_, '_>, name: &str) -> bool {
    node.is_element()
        && node.tag_name().name() == name
        && matches!(node.tag_name().namespace(), None | Some(TT) | Some(OLD_TT))
}

fn is_content(node: Node<'_, '_>) -> bool {
    ["body", "div", "p", "span", "br"]
        .iter()
        .any(|name| is_tt(node, name))
}

#[derive(Clone, Copy)]
struct Rates {
    frame: f64,
    subframe: f64,
    tick: f64,
}

impl Rates {
    fn new(root: Node<'_, '_>) -> Result<Self, ParseError> {
        let parameter = |name| extension(root, "parameter", name);
        if let Some(base) = parameter("timeBase") {
            if base != "media" {
                return Err(ParseError::Unsupported(format!("timeBase={base}")));
            }
        }
        if parameter("dropMode").is_some_and(|mode| mode != "nonDrop") {
            return Err(ParseError::Unsupported("drop-frame timing".into()));
        }
        let positive = |value: &str| {
            decimal(value)
                .filter(|&number| number > 0.0)
                .ok_or_else(|| ParseError::InvalidTimestamp(value.into()))
        };
        let mut frame = parameter("frameRate")
            .map(positive)
            .transpose()?
            .unwrap_or(30.0);
        let subframe = parameter("subFrameRate")
            .map(positive)
            .transpose()?
            .unwrap_or(1.0);
        if let Some(multiplier) = parameter("frameRateMultiplier") {
            let parts: Vec<_> = multiplier.split_whitespace().collect();
            if parts.len() != 2 {
                return Err(ParseError::InvalidTimestamp(multiplier.into()));
            }
            frame *= positive(parts[0])? / positive(parts[1])?;
        }
        let tick = parameter("tickRate").map(positive).transpose()?.unwrap_or(
            if parameter("frameRate").is_some() {
                frame * subframe
            } else {
                1.0
            },
        );
        if !frame.is_finite() || frame <= 0.0 || !tick.is_finite() || tick <= 0.0 {
            return Err(ParseError::InvalidTiming(
                "invalid effective frame/tick rate".into(),
            ));
        }
        Ok(Self {
            frame,
            subframe,
            tick,
        })
    }

    fn time(self, value: &str) -> Result<u64, ParseError> {
        let value = value.trim();
        let fail = || ParseError::InvalidTimestamp(value.into());
        if value.contains(':') {
            let parts: Vec<_> = value.split(':').collect();
            if !(3..=4).contains(&parts.len()) {
                return Err(fail());
            }
            let hours = decimal(parts[0])
                .filter(|_| !parts[0].contains('.'))
                .ok_or_else(fail)?;
            let minutes = decimal(parts[1])
                .filter(|_| !parts[1].contains('.'))
                .ok_or_else(fail)?;
            let seconds = decimal(parts[2]).ok_or_else(fail)?;
            if minutes >= 60.0 || seconds >= 60.0 {
                return Err(fail());
            }
            let frames = if parts.len() == 4 {
                if parts[2].contains('.') {
                    return Err(fail());
                }
                let (frame, sub) = parts[3].split_once('.').unwrap_or((parts[3], "0"));
                let frame = decimal(frame).ok_or_else(fail)?;
                let sub = decimal(sub)
                    .filter(|_| !sub.contains('.'))
                    .ok_or_else(fail)?;
                if frame >= self.frame.ceil() || sub >= self.subframe {
                    return Err(fail());
                }
                (frame + sub / self.subframe) / self.frame
            } else {
                0.0
            };
            return milliseconds(hours * 3600.0 + minutes * 60.0 + seconds + frames, value);
        }
        for (suffix, factor) in [
            ("ms", 0.001),
            ("h", 3600.0),
            ("m", 60.0),
            ("s", 1.0),
            ("f", 1.0 / self.frame),
            ("t", 1.0 / self.tick),
        ] {
            if let Some(number) = value.strip_suffix(suffix) {
                return milliseconds(decimal(number).ok_or_else(fail)? * factor, value);
            }
        }
        if value == "indefinite" || value.starts_with("wallclock(") {
            return Err(ParseError::Unsupported(value.into()));
        }
        Err(fail())
    }
}

#[derive(Clone, Copy, Debug)]
struct Interval {
    start: u64,
    end: Option<u64>,
}

fn earliest(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

fn add(a: u64, b: u64) -> Result<u64, ParseError> {
    a.checked_add(b)
        .ok_or_else(|| ParseError::InvalidTiming("timestamp overflow".into()))
}

fn layout(
    node: Node<'_, '_>,
    parent: Interval,
    base: u64,
    rates: Rates,
    intervals: &mut HashMap<NodeId, Interval>,
) -> Result<Interval, ParseError> {
    let start = add(
        base,
        node.attribute("begin")
            .map(|v| rates.time(v))
            .transpose()?
            .unwrap_or(0),
    )?;
    let explicit_end = node
        .attribute("end")
        .map(|v| rates.time(v).and_then(|t| add(base, t)))
        .transpose()?;
    let duration_end = node
        .attribute("dur")
        .map(|v| rates.time(v).and_then(|t| add(start, t)))
        .transpose()?;
    let own_end = earliest(explicit_end, duration_end);
    if own_end.is_some_and(|end| end < start) {
        return Err(ParseError::InvalidTiming("TTML end precedes begin".into()));
    }
    let mut interval = Interval {
        start,
        end: earliest(own_end, parent.end),
    };
    let sequential = match node.attribute("timeContainer") {
        None | Some("par") => false,
        Some("seq") => true,
        Some(other) => return Err(ParseError::Unsupported(format!("timeContainer={other}"))),
    };
    let children: Vec<_> = node.children().filter(|child| is_content(*child)).collect();
    let mut cursor = start;
    let mut child_end = Some(start);
    for (index, child) in children.iter().enumerate() {
        let child_interval = layout(
            *child,
            interval,
            if sequential { cursor } else { start },
            rates,
            intervals,
        )?;
        child_end = match (child_end, child_interval.end) {
            (Some(a), Some(b)) => Some(a.max(b)),
            _ => None,
        };
        if sequential && index + 1 < children.len() {
            cursor = child_interval.end.ok_or_else(|| {
                ParseError::InvalidTiming("unbounded child in sequential container".into())
            })?;
        }
    }
    if own_end.is_none() && !children.is_empty() {
        interval.end = earliest(interval.end, child_end);
    }
    interval.start = interval.start.max(parent.start);
    if let Some(end) = interval.end {
        interval.start = interval.start.min(end);
    }
    intervals.insert(node.id(), interval);
    Ok(interval)
}

fn voices(node: Node<'_, '_>) -> Vec<String> {
    for ancestor in node.ancestors().filter(Node::is_element) {
        if let Some(value) = extension(ancestor, "metadata", "agent")
            .or_else(|| extension(ancestor, "metadata", "voice"))
            .or_else(|| ancestor.attribute("voice"))
        {
            return value
                .split_whitespace()
                .map(|id| id.trim_start_matches('#').to_owned())
                .collect();
        }
    }
    Vec::new()
}

fn inherited(node: Node<'_, '_>, family: &str, name: &str) -> Option<String> {
    node.ancestors()
        .find_map(|ancestor| extension(ancestor, family, name).map(str::to_owned))
}

fn text_align(value: &str) -> Alignment {
    match value {
        "start" => Alignment::Start,
        "end" => Alignment::End,
        "center" => Alignment::Center,
        "left" => Alignment::Left,
        "right" => Alignment::Right,
        _ => Alignment::Auto,
    }
}

fn style_alignment(
    node: Node<'_, '_>,
    ids: &HashMap<String, Node<'_, '_>>,
    seen: &mut Vec<NodeId>,
) -> Result<Option<Alignment>, ParseError> {
    if seen.contains(&node.id()) {
        return Err(ParseError::InvalidXml("cyclic TTML style reference".into()));
    }
    if seen.len() >= 128 {
        return Err(ParseError::InvalidXml(
            "TTML style nesting exceeds 128".into(),
        ));
    }
    seen.push(node.id());
    let mut alignment = None;
    if let Some(references) = node.attribute("style") {
        for id in references.split_whitespace() {
            if let Some(style) = ids.get(id) {
                alignment = style_alignment(*style, ids, seen)?.or(alignment);
            }
        }
    }
    alignment = extension(node, "styling", "textAlign")
        .map(text_align)
        .or(alignment);
    seen.pop();
    Ok(alignment)
}

fn alignment(
    node: Node<'_, '_>,
    ids: &HashMap<String, Node<'_, '_>>,
) -> Result<Alignment, ParseError> {
    let mut ancestors: Vec<_> = node.ancestors().filter(Node::is_element).collect();
    ancestors.reverse();
    let mut result = Alignment::Auto;
    for ancestor in ancestors {
        if let Some(region) = ancestor.attribute("region").and_then(|id| ids.get(id)) {
            result = style_alignment(*region, ids, &mut Vec::new())?.unwrap_or(result);
        }
        result = style_alignment(ancestor, ids, &mut Vec::new())?.unwrap_or(result);
    }
    Ok(result)
}

struct TextBuilder<'a, 'input> {
    intervals: &'a HashMap<NodeId, Interval>,
    ids: &'a HashMap<String, Node<'input, 'input>>,
    text: String,
    words: Vec<TimedWord>,
    owners: Vec<NodeId>,
    pending_space: bool,
}

impl TextBuilder<'_, '_> {
    fn walk(&mut self, node: Node<'_, '_>, owner: Option<NodeId>) -> Result<(), ParseError> {
        let owner = if is_tt(node, "span")
            && ["begin", "end", "dur"]
                .iter()
                .any(|attr| node.has_attribute(*attr))
        {
            Some(node.id())
        } else {
            owner
        };
        if is_tt(node, "br") {
            self.text.push('\n');
            self.pending_space = false;
            return Ok(());
        }
        for child in node.children() {
            if child.is_text() {
                let preserve =
                    node.ancestors().find_map(|n| n.attribute((XML, "space"))) == Some("preserve");
                let mut fragment = String::new();
                for character in child.text().unwrap_or_default().chars() {
                    if !preserve && matches!(character, ' ' | '\t' | '\r' | '\n') {
                        self.pending_space = true;
                    } else {
                        if self.pending_space && !self.text.is_empty() && !self.text.ends_with('\n')
                        {
                            self.text.push(' ');
                            fragment.push(' ');
                        }
                        self.pending_space = false;
                        self.text.push(character);
                        fragment.push(character);
                    }
                }
                if let Some(owner) = owner {
                    if !fragment.is_empty() {
                        let singer_ids = voices(node);
                        let align = alignment(node, self.ids)?;
                        let role = inherited(node, "metadata", "role");
                        if self.owners.last() == Some(&owner)
                            && self.words.last().is_some_and(|word| {
                                word.singer_ids == singer_ids
                                    && word.alignment == align
                                    && word.role == role
                            })
                        {
                            self.words.last_mut().unwrap().text.push_str(&fragment);
                        } else {
                            let interval = self.intervals[&owner];
                            self.words.push(TimedWord {
                                text: fragment,
                                start_ms: interval.start,
                                end_ms: interval.end,
                                singer_ids,
                                alignment: align,
                                role,
                            });
                            self.owners.push(owner);
                        }
                    }
                }
            } else if is_content(child) {
                self.walk(child, owner)?;
            }
        }
        Ok(())
    }
}

pub fn parse_ttml(input: &str, options: ParseOptions) -> Result<Lyrics, ParseError> {
    check_size(input)?;
    let document = Document::parse_with_options(
        input.trim_start_matches('\u{feff}'),
        roxmltree::ParsingOptions {
            allow_dtd: false,
            nodes_limit: 100_000,
        },
    )
    .map_err(|error| ParseError::InvalidXml(error.to_string()))?;
    let root = document.root_element();
    if !is_tt(root, "tt") {
        return Err(ParseError::InvalidXml("expected TTML tt root".into()));
    }
    for node in root.descendants() {
        if node.ancestors().take(130).count() > 128 {
            return Err(ParseError::InvalidXml("TTML nesting exceeds 128".into()));
        }
    }
    let rates = Rates::new(root)?;
    let mut lyrics = Lyrics::empty(LyricsFormat::Ttml);
    let mut ids = HashMap::new();
    for node in root.descendants().filter(Node::is_element) {
        if let Some(id) = node.attribute((XML, "id")) {
            if ids.insert(id.to_owned(), node).is_some() {
                return Err(ParseError::InvalidXml(format!("duplicate xml:id {id}")));
            }
            if node.tag_name().name() == "agent"
                && node.tag_name().namespace().is_some_and(|ns| {
                    ns == format!("{TT}#metadata") || ns == format!("{OLD_TT}#metadata")
                })
            {
                let names: Vec<_> = node
                    .children()
                    .filter(|n| n.is_element() && n.tag_name().name() == "name")
                    .map(|name| {
                        name.descendants()
                            .filter(Node::is_text)
                            .filter_map(|n| n.text())
                            .collect::<String>()
                    })
                    .collect();
                lyrics.singers.push(Singer {
                    id: id.into(),
                    name: (!names.is_empty()).then(|| names.join(" / ")),
                    kind: node.attribute("type").map(str::to_owned),
                });
            }
        }
        if node
            .tag_name()
            .namespace()
            .is_some_and(|ns| ns == format!("{TT}#metadata") || ns == format!("{OLD_TT}#metadata"))
            && matches!(node.tag_name().name(), "title" | "desc" | "copyright")
        {
            let text = node
                .descendants()
                .filter(Node::is_text)
                .filter_map(|n| n.text())
                .collect::<String>();
            lyrics
                .metadata
                .entry(node.tag_name().name().into())
                .or_default()
                .push(text);
        }
    }
    let mut intervals = HashMap::new();
    layout(
        root,
        Interval {
            start: 0,
            end: options.duration_ms,
        },
        0,
        rates,
        &mut intervals,
    )?;
    for paragraph in root.descendants().filter(|node| is_tt(*node, "p")) {
        let Some(interval) = intervals.get(&paragraph.id()) else {
            continue;
        };
        let mut builder = TextBuilder {
            intervals: &intervals,
            ids: &ids,
            text: String::new(),
            words: Vec::new(),
            owners: Vec::new(),
            pending_space: false,
        };
        builder.walk(paragraph, None)?;
        let mut line = LyricLine::plain(builder.text);
        line.start_ms = Some(interval.start);
        line.end_ms = interval.end;
        line.singer_ids = voices(paragraph);
        line.alignment = alignment(paragraph, &ids)?;
        line.language = paragraph
            .ancestors()
            .find_map(|node| node.attribute((XML, "lang")))
            .map(str::to_owned);
        line.role = inherited(paragraph, "metadata", "role");
        line.words = builder.words;
        for word in &mut line.words {
            word.text = word.text.trim().to_owned();
            word.start_ms = word.start_ms.max(interval.start);
            word.end_ms = earliest(word.end_ms, interval.end);
            if let Some(end) = word.end_ms {
                word.start_ms = word.start_ms.min(end);
            }
        }
        line.words.retain(|word| !word.text.is_empty());
        if paragraph.attribute("begin").is_none() && !line.words.is_empty() {
            line.start_ms = line.words.iter().map(|word| word.start_ms).min();
        }
        lyrics.lines.push(line);
    }
    lyrics.lines.sort_by_key(|line| line.start_ms);
    lyrics.register_singers();
    Ok(lyrics)
}
