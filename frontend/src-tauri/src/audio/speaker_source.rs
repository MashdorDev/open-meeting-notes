//! Labels each transcript segment as the local user (mic) or the other side
//! (system audio).
//!
//! Transcription runs on the mixed signal, so the label has to be recovered
//! afterwards. The pipeline records, on the same timeline the VAD uses
//! (samples fed so far), the energy of both channels for every mixing window
//! and the intervals where a separate VAD heard speech on the mic alone. A
//! segment is the local user's only if the mic carried speech for most of it
//! and was clearly louder than the system audio. Loudness alone is not enough:
//! typing, movement or a cough near the mic is loud but is not speech.

use super::recording_state::DeviceType;
use std::collections::VecDeque;

/// System audio wins unless the mic is at least this many times louder (6 dB).
/// The mic is loudness-normalized and can pick up speaker echo, while the
/// system channel is near-silent whenever only the local user is talking.
const MIC_DOMINANCE: f64 = 2.0;

/// Below this RMS (about -50 dBFS) the system channel counts as silent.
const SYSTEM_SILENCE_RMS: f64 = 0.003;

/// Share of a segment the mic VAD must mark as speech for it to be the
/// local user's.
const MIN_MIC_SPEECH_SHARE: f64 = 0.5;

/// Windows and speech intervals older than this behind the newest window are
/// dropped. A VAD segment longer than this (only under unbroken speech, since
/// live segments have no length cap yet, see #756) is labelled from its last
/// two minutes.
const RETAIN_MS: f64 = 120_000.0;

struct WindowEnergy {
    start_ms: f64,
    end_ms: f64,
    mic_sum_sq: f64,
    sys_sum_sq: f64,
    samples: usize,
}

/// Why a segment got its label; logged so the thresholds can be tuned.
#[derive(Debug, Clone, Copy)]
pub struct SourceEvidence {
    pub mic_rms: f64,
    pub sys_rms: f64,
    pub mic_speech_share: f64,
}

#[derive(Default)]
pub struct SpeakerEnergyTimeline {
    windows: VecDeque<WindowEnergy>,
    mic_speech: VecDeque<(f64, f64)>,
    elapsed_ms: f64,
}

fn sum_sq(samples: &[f32]) -> f64 {
    samples.iter().map(|&s| (s as f64) * (s as f64)).sum()
}

fn overlap(a_start: f64, a_end: f64, b_start: f64, b_end: f64) -> f64 {
    (a_end.min(b_end) - a_start.max(b_start)).max(0.0)
}

impl SpeakerEnergyTimeline {
    /// Records one mixing window. `mic` and `sys` are the same length.
    pub fn push_window(&mut self, mic: &[f32], sys: &[f32], sample_rate: u32) {
        let samples = mic.len().max(sys.len());
        if samples == 0 {
            return;
        }
        let duration_ms = samples as f64 * 1000.0 / sample_rate as f64;
        self.windows.push_back(WindowEnergy {
            start_ms: self.elapsed_ms,
            end_ms: self.elapsed_ms + duration_ms,
            mic_sum_sq: sum_sq(mic),
            sys_sum_sq: sum_sq(sys),
            samples,
        });
        self.elapsed_ms += duration_ms;

        let cutoff = self.elapsed_ms - RETAIN_MS;
        while self.windows.front().is_some_and(|w| w.end_ms < cutoff) {
            self.windows.pop_front();
        }
        while self.mic_speech.front().is_some_and(|&(_, end)| end < cutoff) {
            self.mic_speech.pop_front();
        }
    }

    /// Records a finished stretch of speech heard on the mic alone.
    pub fn add_mic_speech(&mut self, start_ms: f64, end_ms: f64) {
        self.mic_speech.push_back((start_ms, end_ms));
    }

    /// Picks the source for a segment spanning `[start_ms, end_ms)`.
    /// `mic_speech_since` is the start of mic speech still in progress, if any.
    pub fn source_for(
        &self,
        start_ms: f64,
        end_ms: f64,
        mic_speech_since: Option<f64>,
    ) -> (DeviceType, SourceEvidence) {
        let (mut mic, mut sys, mut samples) = (0.0, 0.0, 0usize);
        for w in self.windows.iter().filter(|w| w.end_ms > start_ms && w.start_ms < end_ms) {
            mic += w.mic_sum_sq;
            sys += w.sys_sum_sq;
            samples += w.samples;
        }
        let (mic_rms, sys_rms) = if samples == 0 {
            (0.0, 0.0)
        } else {
            ((mic / samples as f64).sqrt(), (sys / samples as f64).sqrt())
        };

        let mut speech_ms: f64 = self
            .mic_speech
            .iter()
            .map(|&(s, e)| overlap(start_ms, end_ms, s, e))
            .sum();
        if let Some(since) = mic_speech_since {
            speech_ms += overlap(start_ms, end_ms, since, self.elapsed_ms);
        }
        let duration = end_ms - start_ms;
        let mic_speech_share = if duration > 0.0 { (speech_ms / duration).min(1.0) } else { 0.0 };

        let evidence = SourceEvidence { mic_rms, sys_rms, mic_speech_share };
        let mic_spoke = mic_speech_share >= MIN_MIC_SPEECH_SHARE;
        let mic_dominant = sys_rms < SYSTEM_SILENCE_RMS || mic_rms >= sys_rms * MIC_DOMINANCE;
        let source = if mic_spoke && mic_dominant {
            DeviceType::Microphone
        } else {
            DeviceType::System
        };
        (source, evidence)
    }
}

/// Carries speaker labels over to re-segmented lines (retranscription keeps
/// only the mixed audio, so labels cannot be rebuilt from it). Each new line
/// takes the label of the old line it overlaps most.
pub fn speaker_by_overlap(old: &[(f64, f64, String)], start: f64, end: f64) -> Option<String> {
    old.iter()
        .map(|(s, e, speaker)| (overlap(start, end, *s, *e), speaker))
        .filter(|(o, _)| *o > 0.0)
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, speaker)| speaker.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;
    const WINDOW: usize = 28_800; // 600 ms

    fn tone(level: f32) -> Vec<f32> {
        (0..WINDOW).map(|i| if i % 2 == 0 { level } else { -level }).collect()
    }

    fn label(t: &SpeakerEnergyTimeline, start: f64, end: f64) -> DeviceType {
        t.source_for(start, end, None).0
    }

    #[test]
    fn local_speech_with_silent_system_is_mic() {
        let mut t = SpeakerEnergyTimeline::default();
        t.push_window(&tone(0.1), &tone(0.0), RATE);
        t.add_mic_speech(0.0, 600.0);
        assert_eq!(label(&t, 0.0, 600.0), DeviceType::Microphone);
    }

    #[test]
    fn loud_mic_without_speech_is_system() {
        // Typing near the mic while the other side talks quietly.
        let mut t = SpeakerEnergyTimeline::default();
        t.push_window(&tone(0.13), &tone(0.018), RATE);
        assert_eq!(label(&t, 0.0, 600.0), DeviceType::System);
    }

    #[test]
    fn remote_speech_with_mic_echo_is_system() {
        // Speaker echo the mic VAD hears as speech, quieter than the system channel.
        let mut t = SpeakerEnergyTimeline::default();
        t.push_window(&tone(0.05), &tone(0.1), RATE);
        t.add_mic_speech(0.0, 600.0);
        assert_eq!(label(&t, 0.0, 600.0), DeviceType::System);
    }

    #[test]
    fn mic_must_clearly_dominate_to_win() {
        let mut t = SpeakerEnergyTimeline::default();
        t.push_window(&tone(0.15), &tone(0.1), RATE);
        t.push_window(&tone(0.25), &tone(0.1), RATE);
        t.add_mic_speech(0.0, 1200.0);
        assert_eq!(label(&t, 0.0, 600.0), DeviceType::System);
        assert_eq!(label(&t, 600.0, 1200.0), DeviceType::Microphone);
    }

    #[test]
    fn brief_mic_speech_in_a_long_segment_is_system() {
        let mut t = SpeakerEnergyTimeline::default();
        for _ in 0..5 {
            t.push_window(&tone(0.1), &tone(0.0), RATE);
        }
        t.add_mic_speech(0.0, 600.0); // 600 of 3000 ms
        assert_eq!(label(&t, 0.0, 3000.0), DeviceType::System);
    }

    #[test]
    fn speech_still_in_progress_counts() {
        let mut t = SpeakerEnergyTimeline::default();
        t.push_window(&tone(0.1), &tone(0.0), RATE);
        t.push_window(&tone(0.1), &tone(0.0), RATE);
        assert_eq!(t.source_for(0.0, 1200.0, Some(100.0)).0, DeviceType::Microphone);
    }

    #[test]
    fn segment_uses_only_overlapping_windows() {
        let mut t = SpeakerEnergyTimeline::default();
        t.push_window(&tone(0.1), &tone(0.0), RATE); // 0-600: local
        t.push_window(&tone(0.01), &tone(0.1), RATE); // 600-1200: remote
        t.add_mic_speech(0.0, 600.0);
        assert_eq!(label(&t, 100.0, 500.0), DeviceType::Microphone);
        assert_eq!(label(&t, 700.0, 1100.0), DeviceType::System);
    }

    #[test]
    fn unknown_range_is_system() {
        let t = SpeakerEnergyTimeline::default();
        assert_eq!(label(&t, 0.0, 600.0), DeviceType::System);
    }

    #[test]
    fn retranscribed_lines_keep_the_most_overlapping_label() {
        let old = vec![
            (0.0, 4.0, "system".to_string()),
            (4.0, 6.0, "mic".to_string()),
        ];
        assert_eq!(speaker_by_overlap(&old, 3.5, 6.0).as_deref(), Some("mic"));
        assert_eq!(speaker_by_overlap(&old, 0.5, 4.5).as_deref(), Some("system"));
        assert_eq!(speaker_by_overlap(&old, 7.0, 8.0), None);
    }

    #[test]
    fn old_entries_are_pruned() {
        let mut t = SpeakerEnergyTimeline::default();
        for i in 0..400 {
            t.push_window(&tone(0.1), &tone(0.0), RATE);
            t.add_mic_speech(i as f64 * 600.0, i as f64 * 600.0 + 300.0);
        }
        let limit = (RETAIN_MS / 600.0) as usize + 2;
        assert!(t.windows.len() <= limit);
        assert!(t.mic_speech.len() <= limit);
    }
}
