// diarization/voiceprints.rs
//
// Each speaker's voice at the end of a recording, saved next to the audio in
// the meeting folder. Naming a speaker later can then turn that voice into a
// profile of the person, and the file travels with the recording.
//
// A profile is the average of every voice named after that person. While
// recording, each voice heard is compared with the profiles, and a close,
// clear match becomes that voice's guessed name, kept in the same file.

use super::clustering::cosine_similarity;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

pub const VOICEPRINTS_FILE: &str = "speakers.json";
/// Where each voice folded away at stop went, so the database save gives
/// its lines the same label as transcripts.json.
pub const FOLDS_FILE: &str = "speaker-folds.json";
/// Call-side voices that were the user's own voice, as a JSON list of labels.
pub const OWN_VOICE_FILE: &str = "own-voice.json";

/// One speaker's cluster centroid (L2-normalized) and how many segments
/// built it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Voiceprint {
    pub label: String,
    pub segments: usize,
    pub embedding: Vec<f32>,
    /// The person this voice sounds like, when it matched a profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Dor said the guessed name is wrong: it is no longer shown, but the
    /// accuracy score still counts it as a wrong guess.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dismissed: bool,
    /// Transcript lines given this label, short ones included (`segments`
    /// counts only the lines long enough to move the average).
    #[serde(default)]
    pub lines: usize,
}

/// One person's voice: the segment-weighted mean of their samples,
/// L2-normalized.
#[derive(Debug, Clone, PartialEq)]
pub struct VoiceProfile {
    pub name: String,
    pub embedding: Vec<f32>,
}

/// Starting points, not yet tuned on named calls; the speaker accuracy score
/// shows whether they hold. Both sides are averages over many segments, so
/// one voice scores well above the 0.55 a single segment needs to join a
/// speaker. The margin keeps two similar-sounding people from being named
/// on a coin flip.
const PROFILE_MATCH_THRESHOLD: f32 = 0.6;
const PROFILE_MATCH_MARGIN: f32 = 0.08;
/// A voice heard in fewer segments is too unsettled to name.
const MIN_SEGMENTS_TO_NAME: usize = 3;

/// The guessed name of each voice, in order. Each person names at most one
/// voice, the one closest to their profile; the rest stay anonymous.
pub fn guess_names(voices: &[Voiceprint], profiles: &[VoiceProfile]) -> Vec<Option<String>> {
    let mut candidates: Vec<(f32, usize, usize)> = Vec::new();
    for (v, voice) in voices.iter().enumerate().filter(|(_, voice)| voice.segments >= MIN_SEGMENTS_TO_NAME) {
        let mut scores: Vec<(f32, usize)> = profiles
            .iter()
            .enumerate()
            .map(|(p, profile)| (cosine_similarity(&voice.embedding, &profile.embedding), p))
            .collect();
        scores.sort_by(|a, b| b.0.total_cmp(&a.0));
        let Some(&(best, p)) = scores.first() else { continue };
        let runner_up = scores.get(1).map_or(f32::MIN, |s| s.0);
        if best >= PROFILE_MATCH_THRESHOLD && best - runner_up >= PROFILE_MATCH_MARGIN {
            candidates.push((best, v, p));
        }
    }
    candidates.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut names = vec![None; voices.len()];
    let mut taken = vec![false; profiles.len()];
    for (_, v, p) in candidates {
        if !std::mem::replace(&mut taken[p], true) {
            names[v] = Some(profiles[p].name.clone());
        }
    }
    names
}

/// Every person's profile, from the voices named after them. Names that
/// differ only in case or spacing are one person.
pub async fn load_voice_profiles(pool: &sqlx::SqlitePool) -> Result<Vec<VoiceProfile>, sqlx::Error> {
    let rows: Vec<(String, i64, String)> =
        sqlx::query_as("SELECT name, segments, embedding FROM voice_samples ORDER BY meeting_id")
            .fetch_all(pool)
            .await?;
    let mut sums: HashMap<String, (String, Vec<f32>)> = HashMap::new();
    for (name, segments, embedding) in rows {
        let Ok(embedding) = serde_json::from_str::<Vec<f32>>(&embedding) else {
            log::warn!("Skipping an unreadable voice sample");
            continue;
        };
        let key = name.trim().to_lowercase();
        let (_, sum) = sums
            .entry(key)
            .or_insert_with(|| (name.trim().to_string(), vec![0.0; embedding.len()]));
        if sum.len() == embedding.len() {
            sum.iter_mut().zip(&embedding).for_each(|(s, e)| *s += e * segments as f32);
        }
    }
    Ok(sums
        .into_values()
        .filter_map(|(name, mut embedding)| {
            let norm = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
            (norm > 0.0).then(|| {
                embedding.iter_mut().for_each(|x| *x /= norm);
                VoiceProfile { name, embedding }
            })
        })
        .collect())
}

/// A voice on fewer lines than this is folded at stop.
const MIN_LINES_TO_KEEP: usize = 2;
/// How close a folded voice must be to a kept one to take its label;
/// further than that, its lines get no label.
const FOLD_SIMILARITY: f32 = 0.4;

/// Folds the voices on a single line into the closest voice on more lines.
/// In a call with 3 people, 17 of 20 voices were such
/// phantoms; of 9 checked against the call's own transcript, 7 sat closest
/// to the right person, 1 to the wrong one and 1 to no one. Returns the
/// voices kept and, for each folded label, the label its lines now carry.
/// With no voice to fold into, nothing changes.
pub fn fold_single_line_voices(voices: Vec<Voiceprint>) -> (Vec<Voiceprint>, HashMap<String, Option<String>>) {
    let (kept, folded): (Vec<_>, Vec<_>) = voices.into_iter().partition(|v| v.lines >= MIN_LINES_TO_KEEP);
    if kept.is_empty() {
        return (folded, HashMap::new());
    }
    let folds = folded
        .into_iter()
        .map(|voice| {
            let target = kept
                .iter()
                .map(|k| (cosine_similarity(&voice.embedding, &k.embedding), k))
                .max_by(|a, b| a.0.total_cmp(&b.0))
                .filter(|(similarity, _)| *similarity >= FOLD_SIMILARITY)
                .map(|(_, k)| k.label.clone());
            (voice.label, target)
        })
        .collect();
    (kept, folds)
}

/// Saves the folds of one meeting (see `fold_single_line_voices`).
pub fn write_folds(folder: &Path, folds: &HashMap<String, Option<String>>) -> std::io::Result<()> {
    std::fs::write(folder.join(FOLDS_FILE), serde_json::to_vec(folds)?)
}

/// The folds saved with a meeting; empty when it has none.
pub fn read_folds(folder: &Path) -> HashMap<String, Option<String>> {
    std::fs::read(folder.join(FOLDS_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn write_own_voice(folder: &Path, labels: &[String]) -> std::io::Result<()> {
    std::fs::write(folder.join(OWN_VOICE_FILE), serde_json::to_vec(labels)?)
}

/// The call-side labels that were the user; empty when there were none.
pub fn read_own_voice(folder: &Path) -> Vec<String> {
    std::fs::read(folder.join(OWN_VOICE_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// The user's own voice, kept across meetings in the app data folder, so a
/// colleague who talks more than the user on the user's mic is not taken
/// for them.
pub const OWN_VOICE_PROFILE_FILE: &str = "own-voice-profile.json";
/// How close a mic voice must be to the saved profile to be the user. Dor's
/// mic in one meeting scored 0.92 against his profile from another; the
/// others on that call 0.35 and 0.05.
const OWN_PROFILE_MATCH: f32 = PROFILE_MATCH_THRESHOLD;
/// Mic lines a voice needs before it is compared.
const MIN_MIC_LINES: usize = 3;
/// The profile stops weighting older meetings past this many lines, so it
/// follows a new mic or room.
const MAX_PROFILE_LINES: usize = 200;

/// The user's voice. Only the user sets it ("This is me" on a voice), since
/// nothing in the audio tells the user apart from a colleague at their desk;
/// meetings after that only blend into it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OwnVoiceProfile {
    pub embedding: Vec<f32>,
    pub lines: usize,
    /// The call-side voices the user marked as theirs. Heard through a
    /// colleague's mic, the user can sound unlike their own mic, so these
    /// are compared too, not only the mic voice.
    #[serde(default)]
    pub call_voices: Vec<Vec<f32>>,
}

/// Marked call-side voices kept; the oldest go first.
const MAX_CALL_VOICES: usize = 20;
/// A meeting adds at most this many lines' weight to the profile, so one
/// long meeting does not wash out the rest.
const MAX_MEETING_LINES: usize = 50;

impl OwnVoiceProfile {
    /// The profile after the user marks a call-side voice as theirs. The
    /// first mark starts the profile from that voice.
    pub fn with_marked_voice(saved: Option<OwnVoiceProfile>, voice: &Voiceprint) -> OwnVoiceProfile {
        let mut profile = saved.unwrap_or_else(|| OwnVoiceProfile {
            embedding: voice.embedding.clone(),
            lines: voice.segments,
            call_voices: Vec::new(),
        });
        // A retry after a failed save marks the same voice again.
        if profile.call_voices.contains(&voice.embedding) {
            return profile;
        }
        profile.call_voices.push(voice.embedding.clone());
        if profile.call_voices.len() > MAX_CALL_VOICES {
            profile.call_voices.remove(0);
        }
        profile
    }
}

/// What call-side voices are compared with to find the user: this
/// meeting's mic voice (see `own_voice_anchor`) and the marked call voices.
pub fn own_voice_references(mic: &[Voiceprint], profile: Option<&OwnVoiceProfile>) -> Vec<Vec<f32>> {
    let mut references: Vec<Vec<f32>> = own_voice_anchor(mic, profile).into_iter().collect();
    if let Some(profile) = profile {
        references.extend(profile.call_voices.iter().cloned());
    }
    references
}

/// The mic voice closest to the saved profile, when one is close enough.
fn profile_match<'a>(mic: &'a [Voiceprint], profile: &OwnVoiceProfile) -> Option<&'a Voiceprint> {
    mic.iter()
        .filter(|v| v.segments >= MIN_MIC_LINES)
        .map(|v| (v, cosine_similarity(&v.embedding, &profile.embedding)))
        .filter(|(_, score)| *score >= OWN_PROFILE_MATCH)
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(v, _)| v)
}

/// What the user sounds like in this meeting, to compare call-side voices
/// against; None until it is known. With a profile: the mic voice that
/// matches it, or the profile itself while none does. Without one: the
/// voice with the most mic lines, which a colleague who talks more than the
/// user at their desk can be, for that meeting only.
pub fn own_voice_anchor(mic: &[Voiceprint], profile: Option<&OwnVoiceProfile>) -> Option<Vec<f32>> {
    match profile {
        Some(profile) => Some(profile_match(mic, profile).map_or(&profile.embedding, |v| &v.embedding).clone()),
        None => mic
            .iter()
            .filter(|v| v.segments >= MIN_MIC_LINES)
            .max_by_key(|v| v.segments)
            .map(|v| v.embedding.clone()),
    }
}

/// The profile with this meeting's matching mic voice blended in; None when
/// there is no profile or nothing on the mic matched it.
pub fn update_own_profile(mic: &[Voiceprint], profile: Option<&OwnVoiceProfile>) -> Option<OwnVoiceProfile> {
    let profile = profile?;
    let voice = profile_match(mic, profile)?;
    let old = profile.lines.min(MAX_PROFILE_LINES) as f32;
    let new = voice.segments.min(MAX_MEETING_LINES) as f32;
    let mut embedding: Vec<f32> = profile
        .embedding
        .iter()
        .zip(&voice.embedding)
        .map(|(p, v)| p * old + v * new)
        .collect();
    let norm = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm == 0.0 {
        return None;
    }
    embedding.iter_mut().for_each(|x| *x /= norm);
    Some(OwnVoiceProfile {
        embedding,
        lines: (profile.lines + voice.segments).min(MAX_PROFILE_LINES),
        call_voices: profile.call_voices.clone(),
    })
}

pub fn read_own_voice_profile(path: &Path) -> Option<OwnVoiceProfile> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes)
        .map_err(|e| log::warn!("Ignoring unreadable own voice profile: {}", e))
        .ok()
}

/// Held while the profile is read, updated and written, so two recordings
/// ending together neither lose an update nor share the temp file.
pub static OWN_VOICE_PROFILE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn write_own_voice_profile(path: &Path, profile: &OwnVoiceProfile) -> std::io::Result<()> {
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, serde_json::to_vec(profile)?)?;
    std::fs::rename(&temp, path)
}

/// Writes the voiceprints of one meeting. Through a temp file, so a crash
/// mid-write leaves the old file or none, never half of one.
pub fn write_voiceprints(folder: &Path, voiceprints: &[Voiceprint]) -> std::io::Result<()> {
    let json = serde_json::to_vec(voiceprints)?;
    let temp = folder.join(format!(".{}.tmp", VOICEPRINTS_FILE));
    std::fs::write(&temp, json)?;
    std::fs::rename(&temp, folder.join(VOICEPRINTS_FILE))
}

/// The voiceprints saved with a meeting; empty when it has none (recorded
/// before voiceprints were kept, or with no one on the other side).
pub fn read_voiceprints(folder: &Path) -> std::io::Result<Vec<Voiceprint>> {
    match std::fs::read(folder.join(VOICEPRINTS_FILE)) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_the_meeting_folder() {
        let folder = tempfile::tempdir().unwrap();
        assert!(read_voiceprints(folder.path()).unwrap().is_empty());
        let prints = vec![Voiceprint { label: "Speaker 1".into(), segments: 4, embedding: vec![0.6, 0.8], name: Some("Alex".into()), dismissed: true, lines: 6 }];
        write_voiceprints(folder.path(), &prints).unwrap();
        assert_eq!(read_voiceprints(folder.path()).unwrap(), prints);
    }

    fn voice(label: &str, segments: usize, embedding: Vec<f32>) -> Voiceprint {
        Voiceprint { label: label.into(), segments, embedding, name: None, dismissed: false, lines: segments }
    }

    #[test]
    fn folds_one_line_voices_into_the_closest_voice_or_none() {
        // Speaker 5 had one long line, then only short ones matched to it.
        let speaker_5 = Voiceprint { lines: 9, ..voice("Speaker 5", 1, vec![-0.71, 0.7]) };
        let voices = vec![
            voice("Speaker 1", 40, vec![1.0, 0.0]),
            voice("Speaker 2", 1, vec![0.9, 0.44]),
            voice("Speaker 3", 25, vec![0.0, 1.0]),
            voice("Speaker 4", 1, vec![-0.6, -0.8]),
            speaker_5,
        ];
        let (kept, folds) = fold_single_line_voices(voices);
        assert_eq!(kept.iter().map(|v| v.label.as_str()).collect::<Vec<_>>(), ["Speaker 1", "Speaker 3", "Speaker 5"]);
        assert_eq!(
            folds,
            HashMap::from([("Speaker 2".to_string(), Some("Speaker 1".to_string())), ("Speaker 4".to_string(), None)])
        );
        let folder = tempfile::tempdir().unwrap();
        assert!(read_folds(folder.path()).is_empty());
        write_folds(folder.path(), &folds).unwrap();
        assert_eq!(read_folds(folder.path()), folds);
    }

    #[test]
    fn keeps_every_voice_when_none_has_two_lines() {
        let voices = vec![voice("Speaker 1", 1, vec![1.0, 0.0]), voice("Speaker 2", 1, vec![0.0, 1.0])];
        let (kept, folds) = fold_single_line_voices(voices.clone());
        assert_eq!((kept, folds.is_empty()), (voices, true));
    }

    fn profile(name: &str, embedding: Vec<f32>) -> VoiceProfile {
        VoiceProfile { name: name.into(), embedding }
    }

    #[test]
    fn names_a_voice_that_clearly_matches_one_profile() {
        let profiles = [profile("Alex", vec![1.0, 0.0, 0.0]), profile("Sam", vec![0.0, 1.0, 0.0])];
        let voices = [
            voice("Speaker 1", 5, vec![0.98, 0.2, 0.0]),
            voice("Speaker 2", 5, vec![0.0, 0.0, 1.0]),
            voice("Speaker 3", 2, vec![0.0, 1.0, 0.0]),
        ];
        assert_eq!(
            guess_names(&voices, &profiles),
            vec![Some("Alex".into()), None, None],
            "a stranger and a barely heard voice stay anonymous"
        );
    }

    #[test]
    fn leaves_a_voice_between_two_people_anonymous() {
        let profiles = [profile("Alex", vec![1.0, 0.0]), profile("Sam", vec![0.8, 0.6])];
        assert_eq!(guess_names(&[voice("Speaker 1", 9, vec![0.95, 0.31])], &profiles), vec![None]);
    }

    #[test]
    fn gives_a_name_only_to_the_closest_voice() {
        let profiles = [profile("Alex", vec![1.0, 0.0])];
        let voices = [voice("Speaker 1", 4, vec![0.8, 0.6]), voice("Speaker 2", 4, vec![0.99, 0.14])];
        assert_eq!(guess_names(&voices, &profiles), vec![None, Some("Alex".into())]);
    }

    #[tokio::test]
    async fn a_profile_is_the_weighted_mean_of_one_persons_samples() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        crate::database::manager::ensure_voice_samples_table(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO voice_samples VALUES ('m1', 'Speaker 1', 'Alex', 3, '[1.0, 0.0]'),
             ('m2', 'Speaker 4', 'alex ', 1, '[0.0, 1.0]'), ('m2', 'Speaker 2', 'Sam', 2, '[0.0, 1.0]')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let mut profiles = load_voice_profiles(&pool).await.unwrap();
        profiles.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles[0].name, "Alex");
        let expected = [3.0 / 10f32.sqrt(), 1.0 / 10f32.sqrt()];
        assert!(profiles[0].embedding.iter().zip(expected).all(|(a, b)| (a - b).abs() < 1e-6));
        assert_eq!(profiles[1], profile("Sam", vec![0.0, 1.0]));
    }

    #[test]
    fn own_voice_labels_round_trip_and_default_to_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_own_voice(dir.path()).is_empty());
        write_own_voice(dir.path(), &["Speaker 1".to_string()]).unwrap();
        assert_eq!(read_own_voice(dir.path()), vec!["Speaker 1".to_string()]);
    }

    fn mic_voice(label: &str, segments: usize, embedding: [f32; 2]) -> Voiceprint {
        Voiceprint { label: label.into(), segments, embedding: embedding.to_vec(), name: None, dismissed: false, lines: segments }
    }

    #[test]
    fn without_a_profile_the_main_mic_voice_is_the_user_and_nothing_is_saved() {
        let user = mic_voice("Speaker 1", 12, [1.0, 0.0]);
        let colleague = mic_voice("Speaker 2", 8, [0.0, 1.0]);
        assert_eq!(own_voice_anchor(&[colleague.clone(), user.clone()], None), Some(vec![1.0, 0.0]));
        assert_eq!(update_own_profile(&[user, colleague], None), None);
        assert_eq!(own_voice_anchor(&[mic_voice("Speaker 1", 2, [1.0, 0.0])], None), None);
    }

    #[test]
    fn a_profile_keeps_a_talkative_colleague_from_becoming_the_user() {
        let profile = OwnVoiceProfile { embedding: vec![1.0, 0.0], lines: 50, call_voices: Vec::new() };
        let colleague = mic_voice("Speaker 1", 40, [0.0, 1.0]);
        let user = mic_voice("Speaker 2", 5, [0.8, 0.6]);
        assert_eq!(own_voice_anchor(&[colleague.clone(), user.clone()], Some(&profile)), Some(vec![0.8, 0.6]));
        // The user silent on the mic: the profile itself, never the colleague.
        assert_eq!(own_voice_anchor(&[colleague.clone()], Some(&profile)), Some(vec![1.0, 0.0]));
        assert_eq!(update_own_profile(&[colleague.clone()], Some(&profile)), None);
        let updated = update_own_profile(&[colleague, user], Some(&profile)).unwrap();
        assert!(updated.embedding[0] > 0.9);
    }

    #[test]
    fn a_marked_call_voice_counts_even_when_the_mic_voice_matches() {
        let call = mic_voice("Speaker 2", 6, [0.6, 0.8]);
        let profile = OwnVoiceProfile::with_marked_voice(None, &call);
        let user_mic = mic_voice("Speaker 1", 30, [0.8, 0.6]);
        let references = own_voice_references(&[user_mic.clone()], Some(&profile));
        assert_eq!(references, vec![vec![0.8, 0.6], vec![0.6, 0.8]]);
        // Blending the mic voice in keeps the marked call voice.
        let updated = update_own_profile(&[user_mic], Some(&profile)).unwrap();
        assert_eq!(updated.call_voices, vec![vec![0.6, 0.8]]);
        // A second mark adds to the profile rather than replacing it.
        let again = OwnVoiceProfile::with_marked_voice(Some(updated.clone()), &mic_voice("Speaker 4", 3, [0.0, 1.0]));
        assert_eq!((again.embedding, again.call_voices.len()), (updated.embedding.clone(), 2));
        // Marking the same voice twice keeps it once.
        let twice = OwnVoiceProfile::with_marked_voice(Some(updated), &call);
        assert_eq!(twice.call_voices.len(), 1);
    }

    #[test]
    fn a_matching_meeting_blends_into_the_profile() {
        let profile = OwnVoiceProfile { embedding: vec![1.0, 0.0], lines: 10, call_voices: Vec::new() };
        let user = mic_voice("Speaker 1", 10, [0.8, 0.6]);
        let updated = update_own_profile(&[user], Some(&profile)).unwrap();
        assert_eq!(updated.lines, 20);
        let norm = (1.8f32 * 1.8 + 0.6 * 0.6).sqrt();
        assert!((updated.embedding[0] - 1.8 / norm).abs() < 1e-6);
    }
}
