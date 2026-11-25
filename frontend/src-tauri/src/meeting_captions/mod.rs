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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
mod ax;
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
        // keeps two blocks that start the same way ("Yeah") apart. The shift
        // is chosen by strict matches only. Best is a shift whose matches run
        // unbroken from the top of this read to the last block shown before;
        // then the longest unbroken run, then the most matches, then the
        // largest shift, since in a two-person call a block also lines up
        // with the same speaker's block two places on.
        let strict = |old: &(String, String), new: &(String, String)| old.0 == new.0 && same_block(&old.1, &new.1);
        let shift = (0..=self.shown.len())
            .max_by_key(|&s| {
                let overlap = || self.shown[s..].iter().zip(blocks);
                let run = overlap().take_while(|(old, new)| strict(old, new)).count();
                let total = overlap().filter(|(old, new)| strict(old, new)).count();
                let to_end = run > 0 && run == self.shown.len() - s;
                (to_end, run, total)
            })
            .unwrap_or(0);
        // In a place the strict matches fixed, a one-word draft rewritten
        // whole ("I" to "Hi") is still that block. A block that matches
        // nothing is new only if it is like no block already shown, so a
        // rewrite never re-logs.
        let pair = |old: &(String, String), new: &(String, String)| old.0 == new.0 && related(&old.1, &new.1);
        for (i, block) in blocks.iter().enumerate() {
            let (speaker, text) = block;
            let new_part = match self.shown.get(shift + i) {
                Some(old) if pair(old, block) => new_text(&old.1, text),
                Some(_) if self.shown.iter().any(|old| old.0 == *speaker && same_block(&old.1, text)) => "",
                _ => text.trim(),
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

/// Whether a block in the same place and from the same speaker is the one
/// read before. A one-word draft ("I", "So") is often rewritten whole
/// ("Hi", "Sure"), so in its place it counts as the same block.
fn related(old: &str, new: &str) -> bool {
    units(old).len().min(units(new).len()) <= 1 || same_block(old, new)
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
static PAUSED: AtomicBool = AtomicBool::new(false);

pub fn set_paused(paused: bool) {
    PAUSED.store(paused, Ordering::SeqCst);
}

/// The reader thread of the current recording and the flag that stops it.
type Reader = (Arc<AtomicBool>, JoinHandle<Vec<CaptionPiece>>);
static RUNNING: Mutex<Option<Reader>> = Mutex::new(None);

/// Starts reading captions for a new recording.
pub fn start() {
    set_paused(false);
    #[cfg(any(windows, target_os = "macos"))]
    {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let flag = stop_flag.clone();
        #[cfg(windows)]
        let handle = std::thread::spawn(move || uia::run(&flag));
        #[cfg(target_os = "macos")]
        let handle = std::thread::spawn(move || ax::run(&flag));
        let previous = RUNNING.lock().unwrap_or_else(|p| p.into_inner()).replace((stop_flag, handle));
        // A reader left over from a recording that never stopped: let it
        // finish on its own rather than wait for it here.
        if let Some((flag, _)) = previous {
            flag.store(true, Ordering::SeqCst);
        }
    }
}

/// Stops reading and hands back what was captured (nothing off Windows and
/// macOS). The reader may be in the middle of a slow read, so it is waited
/// for off the async runtime's threads.
pub async fn stop() -> Vec<CaptionPiece> {
    let running = RUNNING.lock().unwrap_or_else(|p| p.into_inner()).take();
    let Some((flag, handle)) = running else { return Vec::new() };
    flag.store(true, Ordering::SeqCst);
    match tokio::task::spawn_blocking(move || handle.join()).await {
        Ok(Ok(pieces)) => pieces,
        _ => {
            log::warn!("Meeting caption reader panicked");
            Vec::new()
        }
    }
}

/// Captions show up within a second of the words; a read takes a few ms.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
const READ_EVERY: Duration = Duration::from_millis(700);

/// How long to wait before looking for the caption region again. The
/// search walks browser windows, so after a minute without captions it is
/// done less often.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
fn search_wait(misses: u32) -> Duration {
    match misses {
        0..=12 => Duration::from_secs(5),
        13..=24 => Duration::from_secs(15),
        _ => Duration::from_secs(30),
    }
}

/// One read of the caption region.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
enum Read {
    Texts(Vec<(usize, String)>),
    /// Part of the region changed mid-read; try again at the next read.
    #[cfg_attr(not(windows), allow(dead_code))]
    Partial,
    /// The tab closed or captions were turned off.
    Gone,
}

/// The reader loop both platforms share: finds the caption region with
/// `find`, reads it with `read` and logs the new text against recording
/// time, which is wall time minus pauses, like the audio's timestamps.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
fn capture<R>(stop: &AtomicBool, mut find: impl FnMut() -> Option<R>, mut read: impl FnMut(&R) -> Read) -> Vec<CaptionPiece> {
    let mut clock = Duration::ZERO;
    let mut ticked = Instant::now();
    let mut log = CaptionLog::default();
    let mut region: Option<R> = None;
    let mut last_search: Option<Instant> = None;
    let mut misses = 0;
    // The next read only takes what is on screen without logging it. Set at
    // the start and at a pause: if the region is showing then (or at the
    // first search after), its text was said before the recording or during
    // the pause. A region found by a later search is logged from its first
    // read, since its text was said while recording.
    let mut baseline = true;
    while !stop.load(Ordering::SeqCst) {
        let paused = PAUSED.load(Ordering::SeqCst);
        if !paused {
            clock += ticked.elapsed();
        }
        ticked = Instant::now();
        if paused {
            baseline = true;
            // Search again as soon as the recording resumes.
            last_search = None;
            std::thread::sleep(READ_EVERY);
            continue;
        }
        if region.is_none() && last_search.is_none_or(|t| t.elapsed() >= search_wait(misses)) {
            last_search = Some(Instant::now());
            region = find();
            if region.is_some() {
                misses = 0;
                log::info!("Reading speaker names from Meet's captions");
            } else {
                misses += 1;
                baseline = false;
            }
        }
        if let Some(found) = &region {
            match read(found) {
                Read::Texts(texts) => {
                    let blocks = parse_caption_texts(&texts);
                    if std::mem::take(&mut baseline) {
                        log.baseline(&blocks);
                    } else {
                        log.observe("meet", clock.as_secs_f64(), &blocks);
                    }
                }
                Read::Partial => {}
                Read::Gone => {
                    region = None;
                    baseline = false;
                }
            }
        }
        std::thread::sleep(READ_EVERY);
    }
    let pieces = log.into_pieces();
    log::info!("Captured {} caption pieces", pieces.len());
    pieces
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
        // A one-word draft rewritten whole is still the same block.
        log.observe("meet", 6.0, &blocks(&[("You", "I")]));
        log.observe("meet", 7.0, &blocks(&[("You", "Hi there"), ("Ben", "So")]));
        log.observe("meet", 8.0, &blocks(&[("You", "Hi there"), ("Ben", "Sure")]));
        // A block rewritten past recognition does not re-log its neighbours.
        log.observe("meet", 9.0, &blocks(&[("You", "Okay then so we"), ("Ben", "Sure")]));
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
                ("You".into(), 6.0, "I".into()),
                ("You".into(), 7.0, "Hi there".into()),
                ("Ben".into(), 7.0, "So".into()),
                ("Ben".into(), 8.0, "Sure".into()),
                ("You".into(), 9.0, "Okay then so we".into()),
            ]
        );

        // Two people taking turns: each block also lines up with the same
        // speaker's block two places on, which must not win.
        let mut log = CaptionLog::default();
        log.baseline(&blocks(&[("A", "Yeah"), ("B", "Okay"), ("A", "Sure"), ("B", "Right")]));
        log.observe("meet", 10.0, &blocks(&[("A", "Sure"), ("B", "Right"), ("A", "Yeah definitely")]));
        let turns: Vec<(String, f64, String)> =
            log.into_pieces().into_iter().map(|p| (p.speaker, p.time, p.text)).collect();
        assert_eq!(turns, vec![("A".into(), 10.0, "Yeah definitely".into())]);

        // Short replies that repeat: the run to the last block shown wins
        // over a longer one that stops short of it.
        let mut log = CaptionLog::default();
        log.baseline(&blocks(&[("A", "I think so"), ("B", "Yeah"), ("A", "I think we"), ("B", "Yeah")]));
        log.observe("meet", 11.0, &blocks(&[("A", "I think we"), ("B", "Yeah"), ("A", "I think that works")]));
        let repeats: Vec<(String, f64, String)> =
            log.into_pieces().into_iter().map(|p| (p.speaker, p.time, p.text)).collect();
        assert_eq!(repeats, vec![("A".into(), 11.0, "I think that works".into())]);
    }
}
