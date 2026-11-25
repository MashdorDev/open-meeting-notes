//! Speaker names from the meeting app's own live captions.
//!
//! Google Meet (and other call apps) can show live captions with the
//! speaker's name. Only the user sees them, so nobody in the call notices.
//! While recording, the caption region is read about once a second through
//! the accessibility tree, and each new piece of text is kept with its
//! speaker and the time it appeared. At stop the pieces are saved as
//! captions.json in the meeting folder, for naming the transcript's voices.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[cfg(windows)]
mod uia;

/// Saved next to the transcript.
pub const CAPTIONS_FILE: &str = "captions.json";

/// New caption text, in the order it appeared.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptionPiece {
    /// The name the call app shows ("You" in Meet is the user).
    pub speaker: String,
    /// Seconds since the recording started.
    pub time: f64,
    pub text: String,
    /// Which app it came from, such as "meet".
    pub app: String,
}

/// Splits the texts of a caption region, in tree order with their depth,
/// into (speaker, words) blocks. In Meet a block's speaker name sits one
/// level deeper than its words, so a text deeper than the shallowest one
/// starts a block. Blank texts are spacing.
pub fn parse_caption_texts(texts: &[(usize, String)]) -> Vec<(String, String)> {
    let texts: Vec<&(usize, String)> = texts.iter().filter(|(_, t)| !t.trim().is_empty()).collect();
    let Some(words_depth) = texts.iter().map(|(d, _)| *d).min() else { return Vec::new() };
    let mut blocks: Vec<(String, String)> = Vec::new();
    for (depth, text) in texts {
        let text = text.trim();
        if *depth > words_depth {
            blocks.push((text.to_string(), String::new()));
        } else if let Some((_, words)) = blocks.last_mut() {
            if !words.is_empty() {
                words.push(' ');
            }
            words.push_str(text);
        }
    }
    blocks.retain(|(_, words)| !words.is_empty());
    blocks
}

/// Turns repeated reads of the caption region into pieces of new text. A
/// block keeps growing while its speaker talks, and the app may rewrite its
/// words, so only what follows the part already seen becomes a piece.
#[derive(Default)]
pub struct CaptionLog {
    /// The blocks of the last read, in screen order.
    shown: Vec<(String, String)>,
    pieces: Vec<CaptionPiece>,
}

/// How many words at the end of a block the app rewrites as it goes.
const REVISED_UNITS: usize = 3;

impl CaptionLog {
    /// Takes what is on screen without logging it: text that was there
    /// before the recording started was not said during it.
    pub fn baseline(&mut self, blocks: &[(String, String)]) {
        self.shown = blocks.to_vec();
    }

    pub fn observe(&mut self, app: &str, time: f64, blocks: &[(String, String)]) {
        // Blocks scroll off the top and new ones come in at the bottom, so
        // this read lines up with the end of the last one. Matching by place
        // keeps two blocks that start the same way ("Yeah") apart.
        let shift = (0..=self.shown.len())
            .find(|&s| {
                self.shown[s..]
                    .iter()
                    .zip(blocks)
                    .all(|((s_old, old), (s_new, new))| s_old == s_new && same_block(old, new))
            })
            .unwrap_or(self.shown.len());
        for (i, (speaker, text)) in blocks.iter().enumerate() {
            let new_part = match self.shown.get(shift + i) {
                Some((_, old)) => new_text(old, text),
                None => text.trim(),
            };
            if !new_part.is_empty() {
                self.pieces.push(CaptionPiece {
                    speaker: speaker.clone(),
                    time,
                    text: new_part.to_string(),
                    app: app.to_string(),
                });
            }
        }
        self.shown = blocks.to_vec();
    }

    pub fn into_pieces(self) -> Vec<CaptionPiece> {
        self.pieces
    }
}

/// The text split into the units it is compared by, each with where it
/// starts: words, or single characters in scripts written without spaces.
/// Units are compared without punctuation or case, which the app changes as
/// it revises ("there" becomes "There,", "now." loses its period).
fn units(text: &str) -> Vec<(usize, String)> {
    let mut units: Vec<(usize, String)> = Vec::new();
    let mut word: Option<(usize, String)> = None;
    for (i, c) in text.char_indices() {
        if c.is_whitespace() || unspaced(c) {
            units.extend(word.take());
            if unspaced(c) {
                units.push((i, c.to_string()));
            }
        } else {
            let (_, w) = word.get_or_insert((i, String::new()));
            w.extend(c.to_lowercase().filter(|c| c.is_alphanumeric()));
        }
    }
    units.extend(word);
    units.retain(|(_, u)| !u.is_empty());
    units
}

/// Chinese, Japanese, Thai, Lao, Khmer and Myanmar put no spaces between words.
fn unspaced(c: char) -> bool {
    matches!(c as u32,
        0x0E00..=0x0EFF | 0x1000..=0x109F | 0x1780..=0x17FF
        | 0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF)
}

/// Length of the longest sequence of units both have, in order, by prefix
/// of `b`: `ends[j]` covers `b[..j]`.
fn common_units(a: &[(usize, String)], b: &[(usize, String)]) -> Vec<usize> {
    let mut row = vec![0; b.len() + 1];
    for (_, x) in a {
        let mut prev = vec![0; b.len() + 1];
        std::mem::swap(&mut row, &mut prev);
        for (j, (_, y)) in b.iter().enumerate() {
            row[j + 1] = if x == y { prev[j] + 1 } else { prev[j + 1].max(row[j]) };
        }
    }
    row
}

/// Whether a block read now is the same one read before: it starts the way
/// the earlier one did, or, once the app has trimmed or rewritten its start,
/// at least half of the shorter one is in the other.
fn same_block(old: &str, new: &str) -> bool {
    let (old, new) = (units(old), units(new));
    let n = old.len().min(new.len());
    if n == 0 {
        return false;
    }
    let k = n.min(REVISED_UNITS);
    old[..k].iter().map(|(_, u)| u).eq(new[..k].iter().map(|(_, u)| u))
        || common_units(&old, &new)[new.len()] * 2 >= n.max(2)
}

/// What `new` adds to `old`. The app rewrites the last few words while it
/// listens, so after the common start the rewritten words come back; an
/// edit further back (or a start the app trimmed) gives only the tail that
/// follows everything already seen.
fn new_text<'a>(old: &str, new: &'a str) -> &'a str {
    let (old, new_units) = (units(old), units(new));
    let same = old.iter().zip(&new_units).take_while(|((_, a), (_, b))| a == b).count();
    let from = if old.len() - same <= REVISED_UNITS {
        same
    } else {
        let ends = common_units(&old, &new_units);
        let longest = ends[new_units.len()];
        // The earliest point by which everything shared has been seen.
        ends.iter().position(|&n| n == longest).unwrap_or(new_units.len())
    };
    new_units.get(from).map_or("", |(start, _)| new[*start..].trim())
}

pub fn write_captions(folder: &Path, pieces: &[CaptionPiece]) -> std::io::Result<()> {
    std::fs::write(folder.join(CAPTIONS_FILE), serde_json::to_vec(pieces)?)
}

pub fn read_captions(folder: &Path) -> Option<Vec<CaptionPiece>> {
    serde_json::from_slice(&std::fs::read(folder.join(CAPTIONS_FILE)).ok()?).ok()
}

/// Set while the recording is paused: captions are not read then, and the
/// caption clock stops, the way the audio's does.
static PAUSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_paused(paused: bool) {
    PAUSED.store(paused, std::sync::atomic::Ordering::SeqCst);
}

#[cfg_attr(not(windows), allow(dead_code))]
fn is_paused() -> bool {
    PAUSED.load(std::sync::atomic::Ordering::SeqCst)
}

/// Starts reading captions for a new recording.
pub fn start() {
    set_paused(false);
    #[cfg(windows)]
    uia::start();
}

/// Stops reading and hands back what was captured (nothing off Windows).
pub fn stop() -> Vec<CaptionPiece> {
    #[cfg(windows)]
    return uia::stop();
    #[cfg(not(windows))]
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(items: &[(usize, &str)]) -> Vec<(usize, String)> {
        items.iter().map(|(d, t)| (*d, t.to_string())).collect()
    }

    #[test]
    fn a_deeper_text_names_the_block_that_follows() {
        // The shape Meet showed in Firefox on 2026-11-25.
        let blocks = parse_caption_texts(&texts(&[
            (12, "You"),
            (11, "Hello, how you doing?"),
            (11, "  "),
            (11, "I'd like to eat."),
            (12, "Ben Gur"),
            (11, "Ice cream."),
        ]));
        assert_eq!(
            blocks,
            vec![
                ("You".to_string(), "Hello, how you doing? I'd like to eat.".to_string()),
                ("Ben Gur".to_string(), "Ice cream.".to_string()),
            ]
        );
        assert!(parse_caption_texts(&texts(&[(11, "words with no speaker")])).is_empty());
    }

    #[test]
    fn only_new_words_become_pieces() {
        let mut log = CaptionLog::default();
        let block = |s: &str, t: &str| vec![(s.to_string(), t.to_string())];
        log.observe("meet", 1.0, &block("You", "Hello there"));
        log.observe("meet", 2.0, &block("You", "Hello there"));
        log.observe("meet", 3.0, &block("You", "Hello there, how are"));
        // The app rewrites the last word.
        log.observe("meet", 4.0, &block("You", "Hello there, how is it"));
        // Meet's draft period on the last word.
        log.observe("meet", 4.5, &block("You", "Hello there, how is it."));
        log.observe("meet", 4.8, &block("You", "Hello there, how is it going"));
        log.observe("meet", 5.0, &block("Ben", "Fine thanks"));
        let pieces: Vec<(String, f64, String)> =
            log.into_pieces().into_iter().map(|p| (p.speaker, p.time, p.text)).collect();
        assert_eq!(
            pieces,
            vec![
                ("You".into(), 1.0, "Hello there".into()),
                ("You".into(), 3.0, "how are".into()),
                ("You".into(), 4.0, "is it".into()),
                ("You".into(), 4.8, "going".into()),
                ("Ben".into(), 5.0, "Fine thanks".into()),
            ]
        );

        let mut log = CaptionLog::default();
        let blocks = |items: &[(&str, &str)]| items.iter().map(|(s, t)| (s.to_string(), t.to_string())).collect::<Vec<_>>();
        // On screen before the recording: not said during it.
        log.baseline(&blocks(&[("You", "Before we start")]));
        log.observe("meet", 1.0, &blocks(&[("You", "Before we start"), ("Ben", "Yeah")]));
        // A second short block that starts the same way is its own block.
        log.observe("meet", 2.0, &blocks(&[("You", "Before we start"), ("Ben", "Yeah"), ("You", "I"), ("Ben", "Yeah")]));
        // An edit early in a block gives only the new tail.
        log.observe("meet", 3.0, &blocks(&[("Ben", "Yeah"), ("You", "I"), ("Ben", "Yeah so the plan is fine")]));
        log.observe("meet", 3.5, &blocks(&[("Ben", "Yeah"), ("You", "I"), ("Ben", "Yeah, so that plan is fine today")]));
        // No spaces between words.
        log.observe("meet", 4.0, &blocks(&[("Ken", "今日は")]));
        log.observe("meet", 5.0, &blocks(&[("Ken", "今日はいい天気")]));
        let pieces: Vec<(String, f64, String)> =
            log.into_pieces().into_iter().map(|p| (p.speaker, p.time, p.text)).collect();
        assert_eq!(
            pieces,
            vec![
                ("Ben".into(), 1.0, "Yeah".into()),
                ("You".into(), 2.0, "I".into()),
                ("Ben".into(), 2.0, "Yeah".into()),
                ("Ben".into(), 3.0, "so the plan is fine".into()),
                ("Ben".into(), 3.5, "today".into()),
                ("Ken".into(), 4.0, "今日は".into()),
                ("Ken".into(), 5.0, "いい天気".into()),
            ]
        );
    }
}
