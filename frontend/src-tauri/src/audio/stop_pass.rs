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

/// Starts the stop pass for a saved meeting without waiting for it. Skipped
/// when a retranscription is running; holds the retranscription guard so a
/// retranscription cannot start while it runs.
pub fn spawn<R: Runtime>(app: AppHandle<R>, pool: SqlitePool, meeting_id: String, folder_path: String) {
    tauri::async_runtime::spawn(async move {
        let Ok(_guard) = RetranscriptionGuard::acquire() else {
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

async fn run(pool: &SqlitePool, meeting_id: &str, folder: &Path) -> Result<usize> {
    let (model_dir, quantized) = pick_model().await?;
    let audio_path = find_audio_file(folder)?;

    let segments = tokio::task::spawn_blocking(move || -> Result<Vec<SpeechSegment>> {
        let samples = decode_audio_file(&audio_path)?.to_whisper_format();
        let speech = get_speech_chunks_with_progress(&samples, VAD_REDEMPTION_TIME_MS, |_, _| !is_cancelled())
            .map_err(|e| anyhow!("VAD processing failed: {}", e))?;
        Ok(split_long_segments(&speech))
    })
    .await
    .map_err(|e| anyhow!("decode task panicked: {}", e))??;
    stop_if_needed().await?;

    info!(
        "Stop pass for {meeting_id}: {} speech chunks, model {} ({})",
        segments.len(),
        model_dir.display(),
        if quantized { "int8" } else { "fp32" }
    );
    let mut model = tokio::task::spawn_blocking(move || {
        ParakeetModel::new(&model_dir, quantized).map_err(|e| anyhow!("failed to load Parakeet model: {}", e))
    })
    .await
    .map_err(|e| anyhow!("model load task panicked: {}", e))??;

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

/// The model folder and whether it is int8: full-precision v3 when its files
/// are there, else an installed int8 model, v3 first.
async fn pick_model() -> Result<(PathBuf, bool)> {
    let engine = {
        let guard = crate::parakeet_engine::commands::PARAKEET_ENGINE
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.as_ref().cloned()
    }
    .ok_or_else(|| anyhow!("Parakeet engine not initialized"))?;

    let full = engine.get_models_directory().await.join(FULL_PRECISION_MODEL);
    if FULL_PRECISION_FILES.iter().all(|f| full.join(f).is_file()) {
        return Ok((full, false));
    }

    let models = engine.discover_models().await?;
    let available = |m: &&crate::parakeet_engine::ModelInfo| {
        matches!(m.status, crate::parakeet_engine::ModelStatus::Available)
            && m.quantization == crate::parakeet_engine::QuantizationType::Int8
    };
    models
        .iter()
        .filter(available)
        .find(|m| m.name == INT8_MODEL)
        .or_else(|| models.iter().find(available))
        .map(|m| (m.path.clone(), true))
        .ok_or_else(|| anyhow!("no Parakeet model installed"))
}

/// Writes the new words onto the meeting's lines in one transaction. Lines
/// that get no words keep their live text; `live_text` keeps the first live
/// text, so running again never loses it.
async fn store(pool: &SqlitePool, meeting_id: &str, words: Vec<TranscriptWord>) -> Result<usize> {
    let mut tx = pool.begin().await?;
    let lines: Vec<(String, f64, f64)> = sqlx::query_as(
        "SELECT id, audio_start_time, audio_end_time FROM transcripts
         WHERE meeting_id = ? AND audio_start_time IS NOT NULL AND audio_end_time IS NOT NULL",
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

/// Groups words by line, in the order of `spans`. A word goes to the line
/// whose span holds its start (the line whose middle is nearest, when spans
/// overlap), else to the nearest line. Words keep their order.
fn assign_words(spans: &[(f64, f64)], words: Vec<TranscriptWord>) -> Vec<Vec<TranscriptWord>> {
    let mut per_line: Vec<Vec<TranscriptWord>> = vec![Vec::new(); spans.len()];
    for word in words {
        let t = word.start;
        let key = |&(start, end): &(f64, f64)| ((start - t).max(t - end).max(0.0), (t - (start + end) / 2.0).abs());
        let nearest = spans
            .iter()
            .enumerate()
            .min_by(|a, b| key(a.1).partial_cmp(&key(b.1)).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| i);
        if let Some(i) = nearest {
            per_line[i].push(word);
        }
    }
    per_line
}
