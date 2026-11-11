//! Labels each transcript segment as the local user (mic) or the other side
//! (system audio) by comparing how loud each channel was while it was spoken.
//!
//! Transcription runs on the mixed signal, so the label has to be recovered
//! afterwards. The pipeline records the energy of both channels for every
//! mixing window on the same timeline the VAD uses (samples fed so far), and a
//! finished VAD segment looks up the windows it covers.

use super::recording_state::DeviceType;
use std::collections::VecDeque;

/// System audio wins unless the mic is at least this many times louder (6 dB).
/// The mic is loudness-normalized and picks up speaker echo, while the system
/// channel is near-silent whenever only the local user is talking.
const MIC_DOMINANCE: f64 = 2.0;

/// Below this RMS (about -50 dBFS) the system channel counts as silent.
const SYSTEM_SILENCE_RMS: f64 = 0.003;

/// Windows older than this behind the newest one are dropped. VAD segments are
/// force-split well before this, so a lookup never needs older windows.
const RETAIN_MS: f64 = 120_000.0;

struct WindowEnergy {
    start_ms: f64,
    end_ms: f64,
    mic_sum_sq: f64,
    sys_sum_sq: f64,
    samples: usize,
}

#[derive(Default)]
pub struct SpeakerEnergyTimeline {
    windows: VecDeque<WindowEnergy>,
    elapsed_ms: f64,
}

fn sum_sq(samples: &[f32]) -> f64 {
    samples.iter().map(|&s| (s as f64) * (s as f64)).sum()
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

        while self
            .windows
            .front()
            .is_some_and(|w| w.end_ms < self.elapsed_ms - RETAIN_MS)
        {
            self.windows.pop_front();
        }
    }

    /// Picks the source for a VAD segment spanning `[start_ms, end_ms)`.
    /// Returns the label and the two RMS values (for logging and tuning).
    pub fn source_for(&self, start_ms: f64, end_ms: f64) -> (DeviceType, f64, f64) {
        let mut mic = 0.0;
        let mut sys = 0.0;
        let mut samples = 0usize;
        for w in &self.windows {
            if w.end_ms <= start_ms || w.start_ms >= end_ms {
                continue;
            }
            mic += w.mic_sum_sq;
            sys += w.sys_sum_sq;
            samples += w.samples;
        }
        if samples == 0 {
            return (DeviceType::Microphone, 0.0, 0.0);
        }
        let mic_rms = (mic / samples as f64).sqrt();
        let sys_rms = (sys / samples as f64).sqrt();
        let source = if sys_rms < SYSTEM_SILENCE_RMS || mic_rms >= sys_rms * MIC_DOMINANCE {
            DeviceType::Microphone
        } else {
            DeviceType::System
        };
        (source, mic_rms, sys_rms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;
    const WINDOW: usize = 28_800; // 600 ms

    fn tone(level: f32) -> Vec<f32> {
        (0..WINDOW).map(|i| if i % 2 == 0 { level } else { -level }).collect()
    }

    fn is_mic(source: DeviceType) -> bool {
        matches!(source, DeviceType::Microphone)
    }

    #[test]
    fn local_speech_with_silent_system_is_mic() {
        let mut t = SpeakerEnergyTimeline::default();
        t.push_window(&tone(0.1), &tone(0.0), RATE);
        assert!(is_mic(t.source_for(0.0, 600.0).0));
    }

    #[test]
    fn remote_speech_with_mic_echo_is_system() {
        let mut t = SpeakerEnergyTimeline::default();
        // Speaker echo reaching the mic, quieter than the system channel.
        t.push_window(&tone(0.05), &tone(0.1), RATE);
        assert!(!is_mic(t.source_for(0.0, 600.0).0));
    }

    #[test]
    fn mic_must_clearly_dominate_to_win() {
        let mut t = SpeakerEnergyTimeline::default();
        t.push_window(&tone(0.15), &tone(0.1), RATE);
        assert!(!is_mic(t.source_for(0.0, 600.0).0));
        t.push_window(&tone(0.25), &tone(0.1), RATE);
        assert!(is_mic(t.source_for(600.0, 1200.0).0));
    }

    #[test]
    fn segment_uses_only_overlapping_windows() {
        let mut t = SpeakerEnergyTimeline::default();
        t.push_window(&tone(0.1), &tone(0.0), RATE); // 0-600: local
        t.push_window(&tone(0.01), &tone(0.1), RATE); // 600-1200: remote
        assert!(is_mic(t.source_for(100.0, 500.0).0));
        assert!(!is_mic(t.source_for(700.0, 1100.0).0));
    }

    #[test]
    fn unknown_range_defaults_to_mic() {
        let t = SpeakerEnergyTimeline::default();
        assert!(is_mic(t.source_for(0.0, 600.0).0));
    }

    #[test]
    fn old_windows_are_pruned() {
        let mut t = SpeakerEnergyTimeline::default();
        for _ in 0..400 {
            t.push_window(&tone(0.1), &tone(0.0), RATE);
        }
        assert!(t.windows.len() <= (RETAIN_MS / 600.0) as usize + 2);
    }
}
