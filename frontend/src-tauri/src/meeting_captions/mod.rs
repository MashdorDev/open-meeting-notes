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
/// last words, so only what follows the part already seen becomes a piece.
#[derive(Default)]
pub struct CaptionLog {
    /// The last text seen of each recent block, by speaker.
    open: Vec<(String, String)>,
    pieces: Vec<CaptionPiece>,
}

/// How many recent blocks are remembered; Meet shows about two at a time.
const OPEN_BLOCKS: usize = 8;

impl CaptionLog {
    pub fn observe(&mut self, app: &str, time: f64, blocks: &[(String, String)]) {
        for (speaker, text) in blocks {
            let seen = self
                .open
                .iter()
                .rposition(|(s, old)| s == speaker && same_block(old, text));
            let new_part = match seen {
                Some(i) => {
                    let old = std::mem::replace(&mut self.open[i].1, text.clone());
                    let common = common_prefix_chars(&old, text);
                    // Back to the start of the word the two first differ in.
                    let cut = text[..common].rfind(' ').map_or(0, |i| i + 1);
                    if common == text.len() { String::new() } else { text[cut..].to_string() }
                }
                None => {
                    self.open.push((speaker.clone(), text.clone()));
                    if self.open.len() > OPEN_BLOCKS {
                        self.open.remove(0);
                    }
                    text.clone()
                }
            };
            let new_part = new_part.trim();
            if !new_part.is_empty() {
                self.pieces.push(CaptionPiece {
                    speaker: speaker.clone(),
                    time,
                    text: new_part.to_string(),
                    app: app.to_string(),
                });
            }
        }
    }

    pub fn into_pieces(self) -> Vec<CaptionPiece> {
        self.pieces
    }
}

/// Whether a block read now is the same one read before: it starts the way
/// the earlier one did (the app only rewrites the last few words).
fn same_block(old: &str, new: &str) -> bool {
    let n = old.split_whitespace().count().min(new.split_whitespace().count()).min(3);
    // Punctuation and case come and go as the app revises ("there" becomes "There,").
    let start = |s: &str| {
        s.split_whitespace()
            .take(n)
            .map(|w| w.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase())
            .collect::<Vec<_>>()
    };
    n > 0 && start(old) == start(new)
}

/// Bytes the two strings share at the start, on a character boundary.
fn common_prefix_chars(a: &str, b: &str) -> usize {
    a.char_indices()
        .zip(b.chars())
        .find(|((_, x), y)| x != y)
        .map_or(a.len().min(b.len()), |((i, _), _)| i)
}

pub fn write_captions(folder: &Path, pieces: &[CaptionPiece]) -> std::io::Result<()> {
    std::fs::write(folder.join(CAPTIONS_FILE), serde_json::to_vec(pieces)?)
}

pub fn read_captions(folder: &Path) -> Option<Vec<CaptionPiece>> {
    serde_json::from_slice(&std::fs::read(folder.join(CAPTIONS_FILE)).ok()?).ok()
}

/// Starts reading captions for a new recording.
pub fn start() {
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
        log.observe("meet", 5.0, &block("Ben", "Fine thanks"));
        let pieces: Vec<(String, f64, String)> =
            log.into_pieces().into_iter().map(|p| (p.speaker, p.time, p.text)).collect();
        assert_eq!(
            pieces,
            vec![
                ("You".into(), 1.0, "Hello there".into()),
                ("You".into(), 3.0, "there, how are".into()),
                ("You".into(), 4.0, "is it".into()),
                ("Ben".into(), 5.0, "Fine thanks".into()),
            ]
        );
    }
}
