//! Word timings, so the meeting page can light each word as it is played.

use log::warn;
use serde::{Deserialize, Serialize};

/// One spoken word and when it was said, in seconds. The engine gives times
/// within the chunk it heard; the worker moves them to the recording's clock.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptWord {
    pub text: String,
    pub start: f64,
    pub end: f64,
}

/// The longest a line's last word is taken to last. The engine gives only
/// start times, and the chunk can run on in silence after the last word.
const LAST_WORD_MAX_SECONDS: f64 = 0.6;

/// Groups Parakeet's word pieces into words. A piece that starts with a space
/// begins a word, except punctuation, which joins the word before it. Each
/// word ends where the next one starts.
pub fn words_from_pieces(pieces: &[String], starts: &[f32], chunk_seconds: f64) -> Vec<TranscriptWord> {
    let mut words: Vec<TranscriptWord> = Vec::new();
    for (piece, &start) in pieces.iter().zip(starts) {
        let start = start as f64;
        let begins_word = piece
            .strip_prefix(' ')
            .and_then(|rest| rest.chars().next())
            .is_some_and(char::is_alphanumeric);
        match words.last_mut() {
            Some(word) if !begins_word => word.text.push_str(piece.trim_start()),
            _ => {
                let text = piece.trim_start();
                if text.is_empty() {
                    continue;
                }
                if let Some(previous) = words.last_mut() {
                    previous.end = start;
                }
                words.push(TranscriptWord { text: text.to_string(), start, end: start });
            }
        }
    }
    if let Some(last) = words.last_mut() {
        last.end = (last.start + LAST_WORD_MAX_SECONDS).min(chunk_seconds.max(last.start));
    }
    words
}

/// Moves chunk-relative times to the recording's clock, to the millisecond.
pub fn shift_words(words: Vec<TranscriptWord>, offset: f64) -> Vec<TranscriptWord> {
    let at = |seconds: f64| ((offset + seconds) * 1000.0).round() / 1000.0;
    words
        .into_iter()
        .map(|w| TranscriptWord { start: at(w.start), end: at(w.end), text: w.text })
        .collect()
}

/// The `words` column: JSON, or NULL for lines without word times.
pub fn words_to_json(words: &[TranscriptWord]) -> Option<String> {
    if words.is_empty() {
        return None;
    }
    serde_json::to_string(words)
        .map_err(|e| warn!("Could not store word times: {}", e))
        .ok()
}

pub fn words_from_json(json: Option<&str>) -> Vec<TranscriptWord> {
    let Some(json) = json else { return Vec::new() };
    serde_json::from_str(json).unwrap_or_else(|e| {
        warn!("Ignoring unreadable word times: {}", e);
        Vec::new()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pieces(list: &[&str]) -> Vec<String> {
        list.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn joins_pieces_and_punctuation_into_words() {
        let words = words_from_pieces(
            &pieces(&[" Hel", "lo", " there", " ,", " Ben", "'s", " ."]),
            &[0.0, 0.16, 0.4, 0.64, 0.8, 1.04, 1.2],
            3.0,
        );
        let texts: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(texts, ["Hello", "there,", "Ben's."]);
        assert_eq!((words[0].start, words[0].end), (0.0, 0.4f32 as f64));
        assert_eq!(words[1].end, 0.8f32 as f64);
        // The last word lasts at most LAST_WORD_MAX_SECONDS.
        assert!((words[2].end - (0.8f32 as f64 + LAST_WORD_MAX_SECONDS)).abs() < 1e-9);
    }

    #[test]
    fn last_word_stops_at_the_chunk_end() {
        let words = words_from_pieces(&pieces(&[" Yes"]), &[0.4], 0.5);
        assert_eq!(words[0].end, 0.5);
    }

    #[test]
    fn leading_punctuation_does_not_drop_or_crash() {
        let words = words_from_pieces(&pieces(&[" .", " ok"]), &[0.0, 0.2], 1.0);
        let texts: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(texts, [".", "ok"]);
    }

    #[test]
    fn shifts_to_the_recording_clock_and_round_trips_json() {
        let words = shift_words(
            vec![TranscriptWord { text: "hi".into(), start: 0.08, end: 0.4 }],
            125.3,
        );
        assert_eq!((words[0].start, words[0].end), (125.38, 125.7));
        let json = words_to_json(&words);
        assert_eq!(words_from_json(json.as_deref()), words);
        assert_eq!(words_to_json(&[]), None);
        assert!(words_from_json(Some("not json")).is_empty());
    }
}
