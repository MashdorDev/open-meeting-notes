//! The stop pass: once a recording is saved, transcribe the whole recording
//! again in the background with Parakeet and put the new words on the lines
//! the live pass made. Lines keep their times, speaker, label and name; the
//! live text is kept in `live_text` so it can be restored. Any failure leaves
//! the live text as it was.

use super::decoder::decode_audio_file;
use super::retranscription::{
    find_audio_file, is_cancelled, reset_cancellation, split_long_segments, RetranscriptionGuard,
    VAD_REDEMPTION_TIME_MS,
};
use super::transcription::words::{shift_words, words_from_pieces, words_to_json, TranscriptWord};
use super::vad::{get_speech_chunks_with_progress, SpeechSegment};
use crate::parakeet_engine::ParakeetModel;
use anyhow::{anyhow, Result};
use log::{info, warn};
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Emitter, Runtime};

/// Full-precision Parakeet v3, used when its folder is installed.
const FULL_PRECISION_MODEL: &str = "parakeet-tdt-0.6b-v3";
const FULL_PRECISION_FILES: &[&str] = &[
    "encoder-model.onnx",
    "decoder_joint-model.onnx",
    "nemo128.onnx",
    "vocab.txt",
];
/// The int8 model preferred when full precision is not installed.
const INT8_MODEL: &str = "parakeet-tdt-0.6b-v3-int8";
/// A word further than this from every line is speech the live pass never
/// made a line for; it is dropped rather than put on an unrelated line.
const MAX_WORD_DISTANCE_SECONDS: f64 = 5.0;

/// Starts the stop pass for a saved meeting without waiting for it. Runs only
/// when the transcript provider is Parakeet. Skipped when a retranscription
/// is running; holds the retranscription guard, and a retranscription the
/// user starts cancels it.
pub fn spawn<R: Runtime>(app: AppHandle<R>, pool: SqlitePool, meeting_id: String, folder_path: String) {
    tauri::async_runtime::spawn(async move {
        match uses_parakeet(&pool).await {
            Ok(true) => {}
            Ok(false) => {
                info!("Stop pass for {meeting_id}: skipped, the transcript provider is not Parakeet");
                return;
            }
            Err(e) => {
                warn!("Stop pass for {meeting_id}: skipped, could not read the transcript provider: {e}");
                return;
            }
        }
        let Ok(_guard) = RetranscriptionGuard::acquire_for_stop_pass() else {
            info!("Stop pass for {meeting_id}: skipped, a retranscription is running");
            return;
        };
        reset_cancellation();
        let started = std::time::Instant::now();
        match run(&pool, &meeting_id, Path::new(&folder_path)).await {
            Ok(0) => info!("Stop pass for {meeting_id}: no line changed"),
            Ok(lines_updated) => {
                info!(
                    "Stop pass for {meeting_id}: updated {lines_updated} lines in {:.1}s",
                    started.elapsed().as_secs_f64()
                );
                let _ = app.emit(
                    "meeting-transcript-refined",
                    serde_json::json!({ "meeting_id": meeting_id, "lines_updated": lines_updated }),
                );
            }
            Err(e) => warn!("Stop pass for {meeting_id}: kept the live text: {e}"),
        }
    });
}

/// Whether the saved transcript provider is Parakeet, read the same way
/// retranscription reads it.
async fn uses_parakeet(pool: &SqlitePool) -> Result<bool> {
    let provider: Option<String> =
        sqlx::query_scalar("SELECT provider FROM transcript_settings WHERE id = '1'")
            .fetch_optional(pool)
            .await?;
    Ok(provider.as_deref() == Some("parakeet"))
}

async fn run(pool: &SqlitePool, meeting_id: &str, folder: &Path) -> Result<usize> {
    // Resolved before decoding so a missing model fails early.
    let full_precision = full_precision_dir().await?;
    let int8 = match full_precision {
        Some(_) => None,
        None => Some(int8_dir().await?),
    };
    let audio_path = find_audio_file(folder)?;

    stop_if_needed().await?;
    let samples = tokio::task::spawn_blocking(move || -> Result<Vec<f32>> {
        Ok(decode_audio_file(&audio_path)?.to_whisper_format())
    })
    .await
    .map_err(|e| anyhow!("decode task panicked: {}", e))??;

    stop_if_needed().await?;
    let segments = tokio::task::spawn_blocking(move || -> Result<Vec<SpeechSegment>> {
        let speech = get_speech_chunks_with_progress(&samples, VAD_REDEMPTION_TIME_MS, |_, _| !is_cancelled())
            .map_err(|e| anyhow!("VAD processing failed: {}", e))?;
        Ok(split_long_segments(&speech))
    })
    .await
    .map_err(|e| anyhow!("VAD task panicked: {}", e))??;
    stop_if_needed().await?;

    info!("Stop pass for {meeting_id}: {} speech chunks", segments.len());
    let mut model = match full_precision {
        Some(dir) => match load_model(dir, false).await {
            Ok(model) => model,
            Err(e) => {
                warn!("Stop pass for {meeting_id}: full-precision model failed, using int8: {e}");
                load_model(int8_dir().await?, true).await?
            }
        },
        None => load_model(int8.expect("int8 resolved when no full precision"), true).await?,
    };

    let mut words: Vec<TranscriptWord> = Vec::new();
    for segment in segments {
        stop_if_needed().await?;
        // Under 100 ms of audio: nothing to hear.
        if segment.samples.len() < 1600 {
            continue;
        }
        let offset = segment.start_timestamp_ms / 1000.0;
        let seconds = segment.samples.len() as f64 / 16_000.0;
        let (returned, result) = tokio::task::spawn_blocking(move || {
            let result = model.transcribe_samples(segment.samples);
            (model, result)
        })
        .await
        .map_err(|e| anyhow!("transcription task panicked: {}", e))?;
        model = returned;
        let result = result.map_err(|e| anyhow!("Parakeet transcription failed: {}", e))?;
        words.extend(shift_words(words_from_pieces(&result.tokens, &result.timestamps, seconds), offset));
    }
    drop(model);
    stop_if_needed().await?;

    if words.is_empty() {
        return Ok(0);
    }
    store(pool, meeting_id, words).await
}

/// Stops the pass when a retranscription cancel was asked for, or when a new
/// recording started and needs the CPU for its live transcript.
async fn stop_if_needed() -> Result<()> {
    if is_cancelled() {
        return Err(anyhow!("cancelled"));
    }
    if super::recording_commands::is_recording().await {
        return Err(anyhow!("a new recording started"));
    }
    Ok(())
}

fn parakeet_engine() -> Result<std::sync::Arc<crate::parakeet_engine::ParakeetEngine>> {
    let guard = crate::parakeet_engine::commands::PARAKEET_ENGINE
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    guard.as_ref().cloned().ok_or_else(|| anyhow!("Parakeet engine not initialized"))
}

/// The full-precision v3 folder, when its files are there.
async fn full_precision_dir() -> Result<Option<PathBuf>> {
    let full = parakeet_engine()?.get_models_directory().await.join(FULL_PRECISION_MODEL);
    Ok(FULL_PRECISION_FILES.iter().all(|f| full.join(f).is_file()).then_some(full))
}

/// An installed int8 model folder, v3 first.
async fn int8_dir() -> Result<PathBuf> {
    let models = parakeet_engine()?.discover_models().await?;
    let available = |m: &&crate::parakeet_engine::ModelInfo| {
        matches!(m.status, crate::parakeet_engine::ModelStatus::Available)
            && m.quantization == crate::parakeet_engine::QuantizationType::Int8
    };
    models
        .iter()
        .filter(available)
        .find(|m| m.name == INT8_MODEL)
        .or_else(|| models.iter().find(available))
        .map(|m| m.path.clone())
        .ok_or_else(|| anyhow!("no Parakeet model installed"))
}

async fn load_model(dir: PathBuf, quantized: bool) -> Result<ParakeetModel> {
    info!(
        "Stop pass: loading {} ({})",
        dir.display(),
        if quantized { "int8" } else { "fp32" }
    );
    tokio::task::spawn_blocking(move || {
        ParakeetModel::new(&dir, quantized).map_err(|e| anyhow!("failed to load Parakeet model: {}", e))
    })
    .await
    .map_err(|e| anyhow!("model load task panicked: {}", e))?
}

/// Writes the new words onto the meeting's lines in one transaction. Lines
/// that get no words keep their live text; `live_text` keeps the first live
/// text, so running again never loses it.
async fn store(pool: &SqlitePool, meeting_id: &str, words: Vec<TranscriptWord>) -> Result<usize> {
    let mut tx = pool.begin().await?;
    let lines: Vec<(String, f64, f64)> = sqlx::query_as(
        "SELECT id, audio_start_time, audio_end_time FROM transcripts
         WHERE meeting_id = ? AND audio_start_time IS NOT NULL AND audio_end_time IS NOT NULL
         ORDER BY audio_start_time, id",
    )
    .bind(meeting_id)
    .fetch_all(&mut *tx)
    .await?;
    let spans: Vec<(f64, f64)> = lines.iter().map(|(_, start, end)| (*start, *end)).collect();

    let mut updated = 0;
    for ((id, _, _), line_words) in lines.iter().zip(assign_words(&spans, words)) {
        if line_words.is_empty() {
            continue;
        }
        let text = line_words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ");
        sqlx::query(
            "UPDATE transcripts SET live_text = COALESCE(live_text, transcript), transcript = ?, words = ?
             WHERE id = ?",
        )
        .bind(text)
        .bind(words_to_json(&line_words))
        .bind(id)
        .execute(&mut *tx)
        .await?;
        updated += 1;
    }
    tx.commit().await?;
    Ok(updated)
}

/// Groups words by line, in the order of `spans`. Lines whose spans overlap
/// another line never get words: the recording is mixed, so a word in the
/// overlap cannot be given to one voice. A word that starts inside an overlap
/// is dropped. Any other word goes to the eligible line whose span holds its
/// start, else to the nearest eligible line within MAX_WORD_DISTANCE_SECONDS,
/// else it is dropped. Words keep their order.
fn assign_words(spans: &[(f64, f64)], words: Vec<TranscriptWord>) -> Vec<Vec<TranscriptWord>> {
    let overlaps = |a: (f64, f64), b: (f64, f64)| (a.0 < b.1 && b.0 < a.1) || a == b;
    let mut overlapped: Vec<(f64, f64)> = Vec::new();
    let mut eligible = vec![true; spans.len()];
    for i in 0..spans.len() {
        for j in i + 1..spans.len() {
            if overlaps(spans[i], spans[j]) {
                eligible[i] = false;
                eligible[j] = false;
                overlapped.push((spans[i].0.max(spans[j].0), spans[i].1.min(spans[j].1)));
            }
        }
    }

    let mut per_line: Vec<Vec<TranscriptWord>> = vec![Vec::new(); spans.len()];
    for word in words {
        let t = word.start;
        if overlapped.iter().any(|&(start, end)| start <= t && t <= end) {
            continue;
        }
        let distance = |&(start, end): &(f64, f64)| (start - t).max(t - end).max(0.0);
        let nearest = spans
            .iter()
            .enumerate()
            .filter(|(i, _)| eligible[*i])
            .map(|(i, span)| (i, distance(span)))
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        if let Some((i, d)) = nearest {
            if d <= MAX_WORD_DISTANCE_SECONDS {
                per_line[i].push(word);
            }
        }
    }
    per_line
}
