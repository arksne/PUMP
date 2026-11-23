//! Временные границы нужны для подсветки слов и перекрывающихся голосов.
use lyrics_engine::{ParseOptions, parse_lrc, parse_ttml};
#[test]
fn parses_word_boundaries() {
    let lyrics = parse_lrc(
        "[00:01.00]<00:01.00>Hello <00:01.50>world<00:02.00>",
        ParseOptions::default(),
    )
    .unwrap();
    assert_eq!(lyrics.lines[0].words[0].start_ms, 1000);
    assert_eq!(lyrics.lines[0].words[0].end_ms, Some(1500));
    assert_eq!(lyrics.lines[0].words[1].text, "world");
}
#[test]
fn parses_ttml_words() {
    let lyrics=parse_ttml("<tt xmlns=\"http://www.w3.org/ns/ttml\"><body><div><p begin=\"1s\" end=\"3s\"><span begin=\"1s\" end=\"2s\">Hello</span></p></div></body></tt>",ParseOptions::default()).unwrap();
    assert_eq!(lyrics.lines[0].start_ms, Some(1000));
    assert!(!lyrics.lines[0].words.is_empty());
}
