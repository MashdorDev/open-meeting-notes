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
//!
//! When the local user talks over the other side, transcribing the mix keeps
//! only one of the two voices, so such segments are split: the raw mic and
//! system audio of the last minute is kept, and each channel is transcribed
//! on its own for that stretch. A split needs speech on both channels (a VAD
//! runs on the system channel too, so background noise there does not count).
//! Speaker echo is not removed yet: on laptop speakers, the other side's words
//! can show up in both lines of a split.

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

/// Share of mic speech above which a segment with active system audio is
/// treated as both sides talking, as long as the mic is not quieter than the
/// system channel while the mic hears speech (quieter mic speech is speaker
/// echo, not the local user). Comparing over the whole segment would count
/// the user's pauses and lose a short sentence said over a loud video.
const MIN_OVERLAP_MIC_SPEECH_SHARE: f64 = 0.2;

/// Share of system speech needed for a segment to count as both sides talking.
const MIN_OVERLAP_SYS_SPEECH_SHARE: f64 = 0.2;

/// Windows and speech intervals older than this behind the newest window are
/// dropped (about 23 MB of raw audio at 48 kHz). A VAD segment longer than
/// this (only under unbroken speech, since live segments have no length cap
/// yet, see #756) is labelled from its last minute and is not split.
const RETAIN_MS: f64 = 60_000.0;

struct Window {
    start_ms: f64,
    end_ms: f64,
    mic_sum_sq: f64,
    sys_sum_sq: f64,
    mic: Vec<f32>,
    sys: Vec<f32>,
}

/// How a segment is attributed.
#[derive(Debug, Clone, PartialEq)]
pub enum Attribution {
    /// One side spoke: transcribe the mixed audio under this label.
    Single(DeviceType),
    /// Both sides spoke: transcribe the mic and system channels separately.
    Split,
}

/// Why a segment got its label; logged so the thresholds can be tuned.
#[derive(Debug, Clone, Copy)]
pub struct SourceEvidence {
    pub mic_rms: f64,
    pub sys_rms: f64,
    pub mic_speech_share: f64,
    pub sys_speech_share: f64,
    /// Levels over the windows where the mic heard speech.
    pub mic_rms_in_speech: f64,
    pub sys_rms_in_speech: f64,
}

#[derive(Default)]
pub struct SpeakerEnergyTimeline {
    windows: VecDeque<Window>,
    mic_speech: VecDeque<(f64, f64)>,
    sys_speech: VecDeque<(f64, f64)>,
    elapsed_ms: f64,
}

fn sum_sq(samples: &[f32]) -> f64 {
    samples.iter().map(|&s| (s as f64) * (s as f64)).sum()
}

fn overlap(a_start: f64, a_end: f64, b_start: f64, b_end: f64) -> f64 {
    (a_end.min(b_end) - a_start.max(b_start)).max(0.0)
}

fn speech_share(
    intervals: &VecDeque<(f64, f64)>,
    since: Option<f64>,
    now_ms: f64,
    start_ms: f64,
    end_ms: f64,
) -> f64 {
    let mut speech_ms: f64 = intervals.iter().map(|&(s, e)| overlap(start_ms, end_ms, s, e)).sum();
    if let Some(since) = since {
        speech_ms += overlap(start_ms, end_ms, since, now_ms);
    }
    let duration = end_ms - start_ms;
    if duration > 0.0 { (speech_ms / duration).min(1.0) } else { 0.0 }
}

impl SpeakerEnergyTimeline {
    /// Records one mixing window. `mic` and `sys` are the same length.
    pub fn push_window(&mut self, mic: &[f32], sys: &[f32], sample_rate: u32) {
        let samples = mic.len().max(sys.len());
        if samples == 0 {
            return;
        }
        let duration_ms = samples as f64 * 1000.0 / sample_rate as f64;
        self.windows.push_back(Window {
            start_ms: self.elapsed_ms,
            end_ms: self.elapsed_ms + duration_ms,
            mic_sum_sq: sum_sq(mic),
            sys_sum_sq: sum_sq(sys),
            mic: mic.to_vec(),
            sys: sys.to_vec(),
        });
        self.elapsed_ms += duration_ms;

        let cutoff = self.elapsed_ms - RETAIN_MS;
        while self.windows.front().is_some_and(|w| w.end_ms < cutoff) {
            self.windows.pop_front();
        }
        for speech in [&mut self.mic_speech, &mut self.sys_speech] {
            while speech.front().is_some_and(|&(_, end)| end < cutoff) {
                speech.pop_front();
            }
        }
    }

    /// Records a finished stretch of speech heard on the mic alone.
    pub fn add_mic_speech(&mut self, start_ms: f64, end_ms: f64) {
        self.mic_speech.push_back((start_ms, end_ms));
    }

    /// Records a finished stretch of speech heard on the system audio alone.
    pub fn add_sys_speech(&mut self, start_ms: f64, end_ms: f64) {
        self.sys_speech.push_back((start_ms, end_ms));
    }

    /// Picks the attribution for a segment spanning `[start_ms, end_ms)`.
    /// `mic_speech_since` and `sys_speech_since` are the starts of speech
    /// still in progress on each channel, if any.
    pub fn source_for(
        &self,
        start_ms: f64,
        end_ms: f64,
        mic_speech_since: Option<f64>,
        sys_speech_since: Option<f64>,
    ) -> (Attribution, SourceEvidence) {
        let rms = |windows: &mut dyn Iterator<Item = &Window>| {
            let (mut mic, mut sys, mut samples) = (0.0, 0.0, 0usize);
            for w in windows {
                mic += w.mic_sum_sq;
                sys += w.sys_sum_sq;
                samples += w.mic.len().max(w.sys.len());
            }
            if samples == 0 {
                (0.0, 0.0)
            } else {
                ((mic / samples as f64).sqrt(), (sys / samples as f64).sqrt())
            }
        };
        let in_segment = |w: &&Window| w.end_ms > start_ms && w.start_ms < end_ms;
        let (mic_rms, sys_rms) = rms(&mut self.windows.iter().filter(in_segment));
        let mic_speaking = |w: &&Window| {
            self.mic_speech.iter().any(|&(s, e)| overlap(w.start_ms, w.end_ms, s, e) > 0.0)
                || mic_speech_since.is_some_and(|since| w.end_ms > since)
        };
        let (mic_rms_in_speech, sys_rms_in_speech) =
            rms(&mut self.windows.iter().filter(in_segment).filter(mic_speaking));

        let mic_speech_share =
            speech_share(&self.mic_speech, mic_speech_since, self.elapsed_ms, start_ms, end_ms);
        let sys_speech_share =
            speech_share(&self.sys_speech, sys_speech_since, self.elapsed_ms, start_ms, end_ms);
        let system_active = sys_rms >= SYSTEM_SILENCE_RMS;

        let evidence = SourceEvidence {
            mic_rms,
            sys_rms,
            mic_speech_share,
            sys_speech_share,
            mic_rms_in_speech,
            sys_rms_in_speech,
        };
        let both_spoke = system_active
            && mic_speech_share >= MIN_OVERLAP_MIC_SPEECH_SHARE
            && sys_speech_share >= MIN_OVERLAP_SYS_SPEECH_SHARE
            && mic_rms_in_speech >= sys_rms_in_speech
            && self.covers(start_ms);
        let attribution = if both_spoke {
            Attribution::Split
        } else if mic_speech_share >= MIN_MIC_SPEECH_SHARE
            && (!system_active || mic_rms >= sys_rms * MIC_DOMINANCE)
        {
            Attribution::Single(DeviceType::Microphone)
        } else {
            Attribution::Single(DeviceType::System)
        };
        (attribution, evidence)
    }

    /// True if raw audio from `start_ms` onwards is still kept.
    fn covers(&self, start_ms: f64) -> bool {
        self.windows.front().is_some_and(|w| w.start_ms <= start_ms)
    }

    /// Raw audio of one channel for `[start_ms, end_ms)`, at the capture rate.
    /// None if part of the range has already been dropped.
    pub fn channel_audio(
        &self,
        device: &DeviceType,
        start_ms: f64,
        end_ms: f64,
        sample_rate: u32,
    ) -> Option<Vec<f32>> {
        if !self.covers(start_ms) {
            return None;
        }
        let per_ms = sample_rate as f64 / 1000.0;
        let mut out = Vec::new();
        for w in self.windows.iter().filter(|w| w.end_ms > start_ms && w.start_ms < end_ms) {
            let data = match device {
                DeviceType::Microphone => &w.mic,
                DeviceType::System => &w.sys,
            };
            let from = (((start_ms - w.start_ms).max(0.0)) * per_ms) as usize;
            let to = ((((end_ms.min(w.end_ms)) - w.start_ms) * per_ms) as usize).min(data.len());
            if from < to {
                out.extend_from_slice(&data[from..to]);
            }
        }
        Some(out)
    }
}

/// Carries speaker labels over to re-segmented lines (retranscription keeps
/// only the mixed audio, so labels cannot be rebuilt from it). Each new line
/// takes the label whose old lines overlap it for the longest total time.
/// Ties (a split, where both sides cover the same range) go to the other
/// side, so the result does not depend on row order.
pub fn speaker_by_overlap(old: &[(f64, f64, String)], start: f64, end: f64) -> Option<String> {
    let mut totals: Vec<(&str, f64)> = Vec::new();
    for (s, e, speaker) in old {
        let o = overlap(start, end, *s, *e);
        if o <= 0.0 {
            continue;
        }
        match totals.iter_mut().find(|(name, _)| *name == speaker.as_str()) {
            Some((_, total)) => *total += o,
            None => totals.push((speaker.as_str(), o)),
        }
    }
    totals
        .into_iter()
        .max_by(|a, b| a.1.total_cmp(&b.1).then_with(|| (a.0 == "system").cmp(&(b.0 == "system"))))
        .map(|(speaker, _)| speaker.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;
    const WINDOW: usize = 28_800; // 600 ms

    fn tone(level: f32) -> Vec<f32> {
        (0..WINDOW).map(|i| if i % 2 == 0 { level } else { -level }).collect()
    }

    fn label(t: &SpeakerEnergyTimeline, start: f64, end: f64) -> Attribution {
        t.source_for(start, end, None, None).0
    }

    const MIC: Attribution = Attribution::Single(DeviceType::Microphone);
    const SYS: Attribution = Attribution::Single(DeviceType::System);

    #[test]
    fn local_speech_with_silent_system_is_mic() {
        let mut t = SpeakerEnergyTimeline::default();
        t.push_window(&tone(0.1), &tone(0.0), RATE);
        t.add_mic_speech(0.0, 600.0);
        assert_eq!(label(&t, 0.0, 600.0), MIC);
    }

    #[test]
    fn loud_mic_without_speech_is_system() {
        // Typing near the mic while the other side talks quietly.
        let mut t = SpeakerEnergyTimeline::default();
        t.push_window(&tone(0.13), &tone(0.018), RATE);
        assert_eq!(label(&t, 0.0, 600.0), SYS);
    }

    #[test]
    fn remote_speech_with_mic_echo_is_system() {
        // Speaker echo the mic VAD hears as speech, quieter than the system channel.
        let mut t = SpeakerEnergyTimeline::default();
        t.push_window(&tone(0.05), &tone(0.1), RATE);
        t.add_mic_speech(0.0, 600.0);
        assert_eq!(label(&t, 0.0, 600.0), SYS);
    }

    #[test]
    fn talking_over_the_other_side_is_split() {
        // Dor talks over a video: the mic is louder and carries speech.
        let mut t = SpeakerEnergyTimeline::default();
        t.push_window(&tone(0.09), &tone(0.025), RATE);
        t.add_mic_speech(0.0, 600.0);
        t.add_sys_speech(0.0, 600.0);
        assert_eq!(label(&t, 0.0, 600.0), Attribution::Split);
    }

    #[test]
    fn talking_over_background_noise_is_mic() {
        // The other side is unmuted with a fan running: audible, but not speech.
        let mut t = SpeakerEnergyTimeline::default();
        t.push_window(&tone(0.09), &tone(0.006), RATE);
        t.add_mic_speech(0.0, 600.0);
        assert_eq!(label(&t, 0.0, 600.0), MIC);
    }

    #[test]
    fn short_interjection_over_the_other_side_is_split() {
        let mut t = SpeakerEnergyTimeline::default();
        for _ in 0..5 {
            t.push_window(&tone(0.06), &tone(0.04), RATE);
        }
        t.add_mic_speech(0.0, 800.0); // 800 of 3000 ms
        t.add_sys_speech(0.0, 3000.0);
        assert_eq!(label(&t, 0.0, 3000.0), Attribution::Split);
    }

    #[test]
    fn channel_audio_cuts_the_requested_range() {
        let mut t = SpeakerEnergyTimeline::default();
        let mic: Vec<f32> = (0..WINDOW).map(|i| i as f32).collect();
        t.push_window(&mic, &tone(0.0), RATE);
        t.push_window(&mic, &tone(0.0), RATE);
        // 300 ms into the first window up to 300 ms into the second
        let cut = t.channel_audio(&DeviceType::Microphone, 300.0, 900.0, RATE).unwrap();
        assert_eq!(cut.len(), WINDOW);
        assert_eq!(cut[0], (WINDOW / 2) as f32);
        assert_eq!(t.channel_audio(&DeviceType::System, 0.0, 600.0, RATE).unwrap().len(), WINDOW);
    }

    #[test]
    fn a_short_sentence_over_a_loud_video_is_split() {
        // Seen live: Dor talks for 2.4 of 6 s over a video. Averaged over the
        // whole segment the mic is quieter than the video, during his speech
        // it is louder.
        let mut t = SpeakerEnergyTimeline::default();
        for i in 0..10 {
            let mic = if i < 4 { tone(0.09) } else { tone(0.0) };
            t.push_window(&mic, &tone(0.06), RATE);
        }
        t.add_mic_speech(0.0, 2400.0);
        t.add_sys_speech(0.0, 6000.0);
        let (attribution, evidence) = t.source_for(0.0, 6000.0, None, None);
        assert!(evidence.mic_rms < evidence.sys_rms);
        assert_eq!(attribution, Attribution::Split);
    }

    #[test]
    fn dropped_range_is_not_split() {
        let mut t = SpeakerEnergyTimeline::default();
        for _ in 0..200 {
            t.push_window(&tone(0.09), &tone(0.025), RATE);
        }
        t.add_mic_speech(0.0, 120_000.0);
        t.add_sys_speech(0.0, 120_000.0);
        assert!(t.channel_audio(&DeviceType::Microphone, 0.0, 600.0, RATE).is_none());
        assert_ne!(label(&t, 0.0, 600.0), Attribution::Split);
    }

    #[test]
    fn brief_mic_speech_in_a_long_segment_is_system() {
        let mut t = SpeakerEnergyTimeline::default();
        for _ in 0..5 {
            t.push_window(&tone(0.1), &tone(0.0), RATE);
        }
        t.add_mic_speech(0.0, 600.0); // 600 of 3000 ms
        assert_eq!(label(&t, 0.0, 3000.0), SYS);
    }

    #[test]
    fn speech_still_in_progress_counts() {
        let mut t = SpeakerEnergyTimeline::default();
        t.push_window(&tone(0.1), &tone(0.0), RATE);
        t.push_window(&tone(0.1), &tone(0.0), RATE);
        assert_eq!(t.source_for(0.0, 1200.0, Some(100.0), None).0, MIC);
    }

    #[test]
    fn segment_uses_only_overlapping_windows() {
        let mut t = SpeakerEnergyTimeline::default();
        t.push_window(&tone(0.1), &tone(0.0), RATE); // 0-600: local
        t.push_window(&tone(0.01), &tone(0.1), RATE); // 600-1200: remote
        t.add_mic_speech(0.0, 600.0);
        assert_eq!(label(&t, 100.0, 500.0), MIC);
        assert_eq!(label(&t, 700.0, 1100.0), SYS);
    }

    #[test]
    fn unknown_range_is_system() {
        let t = SpeakerEnergyTimeline::default();
        assert_eq!(label(&t, 0.0, 600.0), SYS);
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
    fn retranscribed_label_sums_overlap_per_speaker() {
        let old = vec![
            (0.0, 3.0, "system".to_string()),
            (3.0, 6.0, "system".to_string()),
            (6.0, 10.0, "mic".to_string()),
        ];
        assert_eq!(speaker_by_overlap(&old, 0.0, 10.0).as_deref(), Some("system"));
        // A split row pair ties: the label does not depend on row order.
        let split_a = vec![(0.0, 4.0, "mic".to_string()), (0.0, 4.0, "system".to_string())];
        let split_b = vec![(0.0, 4.0, "system".to_string()), (0.0, 4.0, "mic".to_string())];
        assert_eq!(speaker_by_overlap(&split_a, 0.0, 4.0).as_deref(), Some("system"));
        assert_eq!(speaker_by_overlap(&split_b, 0.0, 4.0).as_deref(), Some("system"));
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
