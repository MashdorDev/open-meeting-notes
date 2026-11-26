use crate::api::TranscriptSegment;
use anyhow::Result;
use log::{debug, info};
use once_cell::sync::Lazy;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use uuid::Uuid;

static ENGINE_LIFECYCLE_LOCK: Lazy<Arc<AsyncMutex<()>>> =
    Lazy::new(|| Arc::new(AsyncMutex::new(())));

pub(crate) async fn acquire_engine_lifecycle_lock() -> OwnedMutexGuard<()> {
    ENGINE_LIFECYCLE_LOCK.clone().lock_owned().await
}

/// Unload the transcription engine after a batch job (import or retranscription).
/// Skips unloading if a live recording is currently in progress, since recording
/// uses the same global engine instances.
pub(crate) async fn unload_engine_after_batch(use_parakeet: bool) {
    let _engine_lifecycle_guard = acquire_engine_lifecycle_lock().await;

    if crate::audio::recording_commands::is_recording().await {
        log::info!("Skipping model unload after batch: recording in progress");
        return;
    }

    if use_parakeet {
        use crate::parakeet_engine::commands::PARAKEET_ENGINE;
        let engine = {
            let guard = PARAKEET_ENGINE.lock().unwrap_or_else(|e| e.into_inner());
            guard.as_ref().cloned()
        };
        if let Some(e) = engine {
            e.unload_model().await;
        }
    } else {
        use crate::whisper_engine::commands::WHISPER_ENGINE;
        let engine = {
            let guard = WHISPER_ENGINE.lock().unwrap_or_else(|e| e.into_inner());
            guard.as_ref().cloned()
        };
        if let Some(e) = engine {
            e.unload_model().await;
        }
    }
}

/// Pieces of one long speech stretch are cut at its quietest moment, often a
/// breath in mid-sentence, and the engine ends every piece with a period:
/// "who's a vice president." then "of quality at 4G Clinical". When the next
/// piece starts right where this one ends and opens in lowercase, the
/// sentence goes on and that period is not real.
fn continues_sentence(text: &str, end_ms: f64, next_text: &str, next_start_ms: f64) -> bool {
    // Pieces of one stretch touch (or overlap by the fallback second);
    // separate stretches are at least a pause apart.
    const TOUCHING_MS: f64 = 50.0;
    next_start_ms - end_ms < TOUCHING_MS
        && text.ends_with('.')
        && !text.ends_with("..")
        && next_text.trim_start().chars().next().is_some_and(|c| c.is_lowercase())
}

/// Create transcript segments from transcription results.
/// Each tuple is (text, start_ms, end_ms) from VAD timestamps.
pub(crate) fn create_transcript_segments(transcripts: &[(String, f64, f64)]) -> Vec<TranscriptSegment> {
    transcripts
        .iter()
        .enumerate()
        .map(|(i, (text, start_ms, end_ms))| {
            let start_seconds = start_ms / 1000.0;
            let end_seconds = end_ms / 1000.0;
            let duration = end_seconds - start_seconds;
            let mut text = text.trim().to_string();
            if let Some((next_text, next_start_ms, _)) = transcripts.get(i + 1) {
                if continues_sentence(&text, *end_ms, next_text, *next_start_ms) {
                    text.pop();
                }
            }

            TranscriptSegment {
                id: format!("transcript-{}", Uuid::new_v4()),
                text,
                timestamp: chrono::Utc::now().to_rfc3339(),
                audio_start_time: Some(start_seconds),
                audio_end_time: Some(end_seconds),
                duration: Some(duration),
                speaker: None,
                speaker_label: None,
                words: Vec::new(),
            }
        })
        .collect()
}

/// Write transcripts.json to a meeting folder (atomic write with temp file)
pub(crate) fn write_transcripts_json(folder: &Path, segments: &[TranscriptSegment]) -> Result<()> {
    let transcript_path = folder.join("transcripts.json");
    let temp_path = folder.join(".transcripts.json.tmp");

    let json = serde_json::json!({
        "version": "1.0",
        "last_updated": chrono::Utc::now().to_rfc3339(),
        "total_segments": segments.len(),
        "segments": segments.iter().enumerate().map(|(i, s)| {
            serde_json::json!({
                "id": s.id,
                "text": s.text,
                "timestamp": s.timestamp,
                "audio_start_time": s.audio_start_time,
                "audio_end_time": s.audio_end_time,
                "duration": s.duration,
                "sequence_id": i
            })
        }).collect::<Vec<_>>()
    });

    let json_string = serde_json::to_string_pretty(&json)?;
    std::fs::write(&temp_path, &json_string)?;
    std::fs::rename(&temp_path, &transcript_path)?;

    info!(
        "Wrote transcripts.json with {} segments to {}",
        segments.len(),
        transcript_path.display()
    );
    Ok(())
}

/// Split a long speech segment at the lowest-energy (silence) point near the target size.
///
/// Scans for 100ms windows with minimal RMS energy within +/-3 seconds of each target
/// split point. If no clear silence is found, falls back to a 1-second overlap split
/// to avoid cutting words at boundaries.
pub(crate) fn split_segment_at_silence(
    segment: &crate::audio::vad::SpeechSegment,
    max_samples: usize,
) -> Vec<crate::audio::vad::SpeechSegment> {
    const SAMPLE_RATE: usize = 16000;
    // 100ms window for energy measurement (1600 samples at 16kHz)
    const ENERGY_WINDOW: usize = SAMPLE_RATE / 10;
    // Search +/-3 seconds around the target split point
    const SEARCH_RADIUS: usize = SAMPLE_RATE * 3;
    // RMS threshold below which we consider a window "silent"
    const SILENCE_RMS_THRESHOLD: f32 = 0.02;
    // Overlap to use when no silence boundary is found (1 second)
    const FALLBACK_OVERLAP: usize = SAMPLE_RATE;

    let total = segment.samples.len();
    if total <= max_samples {
        return vec![segment.clone()];
    }

    let ms_per_sample = (segment.end_timestamp_ms - segment.start_timestamp_ms)
        / segment.samples.len() as f64;
    let mut result = Vec::new();
    let mut pos = 0usize;

    while pos < total {
        let remaining = total - pos;
        if remaining <= max_samples {
            // Last chunk - take everything remaining
            let chunk_samples = segment.samples[pos..].to_vec();
            let chunk_start_ms = segment.start_timestamp_ms + (pos as f64 * ms_per_sample);
            let chunk_end_ms = segment.end_timestamp_ms;
            result.push(crate::audio::vad::SpeechSegment {
                samples: chunk_samples,
                start_timestamp_ms: chunk_start_ms,
                end_timestamp_ms: chunk_end_ms,
                confidence: segment.confidence,
            });
            break;
        }

        // Target split point
        let target = pos + max_samples;

        // Search window: [target - SEARCH_RADIUS, target + SEARCH_RADIUS]
        let search_start = target.saturating_sub(SEARCH_RADIUS).max(pos + SAMPLE_RATE);
        let search_end = (target + SEARCH_RADIUS).min(total.saturating_sub(ENERGY_WINDOW));

        // Find the lowest-energy 100ms window in the search range
        let mut best_split = target.min(total); // fallback: exact target
        let mut best_rms = f32::MAX;

        if search_start + ENERGY_WINDOW <= search_end {
            let mut idx = search_start;
            while idx + ENERGY_WINDOW <= search_end {
                let window = &segment.samples[idx..idx + ENERGY_WINDOW];
                let rms = (window.iter().map(|s| s * s).sum::<f32>() / ENERGY_WINDOW as f32).sqrt();
                if rms < best_rms {
                    best_rms = rms;
                    best_split = idx + ENERGY_WINDOW / 2; // split at center of quiet window
                }
                // Step by 10ms (160 samples) for efficiency
                idx += SAMPLE_RATE / 100;
            }
        }

        let split_at = best_split;
        if best_rms <= SILENCE_RMS_THRESHOLD {
            debug!(
                "Splitting at silence boundary: sample {} (RMS={:.4})",
                split_at, best_rms
            );
        } else {
            debug!(
                "No silence found near target (best RMS={:.4}), splitting with overlap at sample {}",
                best_rms, split_at
            );
        }

        // Determine the actual end of this chunk (with overlap if no silence)
        let chunk_end = if best_rms > SILENCE_RMS_THRESHOLD {
            (split_at + FALLBACK_OVERLAP).min(total)
        } else {
            split_at
        };

        let chunk_samples = segment.samples[pos..chunk_end].to_vec();
        let chunk_start_ms = segment.start_timestamp_ms + (pos as f64 * ms_per_sample);
        let chunk_end_ms = segment.start_timestamp_ms + (chunk_end as f64 * ms_per_sample);

        result.push(crate::audio::vad::SpeechSegment {
            samples: chunk_samples,
            start_timestamp_ms: chunk_start_ms,
            end_timestamp_ms: chunk_end_ms,
            confidence: segment.confidence,
        });

        // Advance position to where the current chunk actually ends
        // to avoid transcribing the overlap region twice
        pos = chunk_end;
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_engine_lifecycle_lock_serializes_acquirers() {
        let guard = acquire_engine_lifecycle_lock().await;
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (acquired_tx, mut acquired_rx) = tokio::sync::oneshot::channel();
        let waiter = tokio::spawn(async {
            started_tx.send(()).unwrap();
            let _guard = acquire_engine_lifecycle_lock().await;
            acquired_tx.send(()).unwrap();
        });

        started_rx.await.unwrap();
        assert!(acquired_rx.try_recv().is_err());
        drop(guard);

        acquired_rx.await.unwrap();
        waiter.await.unwrap();
    }
}

#[cfg(test)]
mod continues_sentence_tests {
    use super::*;

    #[test]
    fn a_period_at_a_mid_sentence_cut_is_dropped() {
        let pieces = vec![
            ("Lara Aujo, who's a vice president.".to_string(), 30_000.0, 54_180.0),
            ("of quality at 4G Clinical.".to_string(), 54_180.0, 78_000.0),
        ];
        let segments = create_transcript_segments(&pieces);
        assert_eq!(segments[0].text, "Lara Aujo, who's a vice president");
        assert_eq!(segments[1].text, "of quality at 4G Clinical.");
    }

    #[test]
    fn real_sentence_ends_are_kept() {
        // Next piece starts a new sentence, comes after a pause, or the
        // period is an ellipsis.
        assert!(!continues_sentence("We shipped it.", 10_000.0, "Then we waited.", 10_000.0));
        assert!(!continues_sentence("We shipped it.", 10_000.0, "and then", 12_500.0));
        assert!(!continues_sentence("So...", 10_000.0, "anyway", 10_000.0));
        assert!(continues_sentence("a vice president.", 10_000.0, "of quality", 9_000.0));
    }
}
