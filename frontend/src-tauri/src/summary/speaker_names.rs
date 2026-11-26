//! Finds the names of the people on the other side of a call from what is
//! said in it ("I'm Sarah", "thanks, Dan"), after the transcript is saved.
//!
//! The model proposes names with the line each comes from. Whether a name
//! addresses or introduces someone is left to the model: word rules for that
//! kept rejecting real addresses or passing mentions. Only cheap facts are
//! checked here (the quoted line contains the name, it is not Dor's own
//! name), and the meeting page lets Dor rename or clear the result. A name Dor
//! set by hand is never replaced by a later automatic pass.

use crate::summary::title::{ask_llm, visible_reply};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tauri::{AppHandle, Emitter, Manager, Runtime};
use log::{info, warn};

// Greetings and introductions sit at the start of a call, thanks at the end.
const HEAD_CHARS: usize = 4500;
const TAIL_CHARS: usize = 1500;
const MAX_NAME_CHARS: usize = 40;
const MAX_NAMES: usize = 5;

const SYSTEM_PROMPT: &str = "You find the names of the people on the other side of a call. \
Each transcript line is numbered and starts with Me: (the user) or Others: (the other side); \
an Others line may name its voice, as in Others (Speaker 2):. \
Reply with only JSON: {\"names\": [{\"name\": \"Sarah\", \"line\": 12}]}. \
List a name only if an Others line introduces that person (\"I'm Sarah\", \"this is Dan\") \
or a line speaks to someone by name (\"Sarah, you're up first\") and a different voice answers. \
\"line\" is the number of the line that contains the name. \
Never list the user's own name or people who are only talked about. \
If no name is clear, reply {\"names\": []}.";

#[derive(Clone, Serialize)]
pub struct MeetingSpeakersIdentified {
    pub meeting_id: String,
    pub names: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Side {
    Me,
    Others,
}

struct Line {
    side: Side,
    /// The voice ("Speaker 2"), on lines the speaker separation labelled.
    label: Option<String>,
    text: String,
}

#[derive(Debug, Deserialize)]
struct Proposal {
    name: String,
    line: usize,
}

fn side_of(speaker: &str) -> Option<Side> {
    match speaker {
        "mic" => Some(Side::Me),
        "system" => Some(Side::Others),
        _ => None,
    }
}

/// Numbered "[n] Me: text" lines, keeping the start and end of long calls.
fn numbered_excerpt(lines: &[Line]) -> String {
    let rendered: Vec<String> = lines
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let who = match (l.side, &l.label) {
                (Side::Me, _) => "Me".to_string(),
                (Side::Others, Some(label)) => format!("Others ({label})"),
                (Side::Others, None) => "Others".to_string(),
            };
            format!("[{i}] {who}: {}", l.text)
        })
        .collect();
    let total: usize = rendered.iter().map(|l| l.chars().count() + 1).sum();
    if total <= HEAD_CHARS + TAIL_CHARS {
        return rendered.join("\n");
    }
    let (mut head, mut used) = (Vec::new(), 0);
    for line in &rendered {
        used += line.chars().count() + 1;
        if used > HEAD_CHARS {
            break;
        }
        head.push(line.as_str());
    }
    let (mut tail, mut used) = (Vec::new(), 0);
    for line in rendered[head.len()..].iter().rev() {
        used += line.chars().count() + 1;
        if used > TAIL_CHARS {
            break;
        }
        tail.push(line.as_str());
    }
    tail.reverse();
    format!("{}\n[...]\n{}", head.join("\n"), tail.join("\n"))
}

fn parse_reply(raw: &str) -> Option<Vec<Proposal>> {
    let visible = visible_reply(raw)?;
    // The first JSON object from the first "{"; text after it is ignored.
    let value: serde_json::Value = serde_json::Deserializer::from_str(&visible[visible.find('{')?..])
        .into_iter()
        .next()?
        .ok()?;
    let names = match value.get("names") {
        Some(serde_json::Value::Array(items)) => items,
        _ => return Some(Vec::new()),
    };
    // A malformed entry ("line": "3", a null name) drops that entry only.
    Some(
        names
            .iter()
            .filter_map(|item| {
                let name = item.get("name")?.as_str()?.to_string();
                let line = match item.get("line")? {
                    serde_json::Value::Number(n) => usize::try_from(n.as_u64()?).ok()?,
                    serde_json::Value::String(t) => t.trim().parse().ok()?,
                    _ => return None,
                };
                Some(Proposal { name, line })
            })
            .collect(),
    )
}

/// Words of `text` without surrounding punctuation, as written.
fn raw_words(text: &str) -> Vec<&str> {
    text.split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric() && c != '\''))
        .filter(|w| !w.is_empty())
        .collect()
}

fn lowercase(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_lowercase()).collect()
}

/// Position of the name's words inside `words`, if they appear in order.
fn find_name(words: &[String], name: &[String]) -> Option<usize> {
    (0..=words.len().checked_sub(name.len())?).find(|&i| words[i..i + name.len()] == *name)
}

fn is_self_intro(words: &[String], at: usize, name_len: usize) -> bool {
    let before = |phrase: &[&str]| {
        at >= phrase.len() && words[at - phrase.len()..at].iter().zip(phrase).all(|(w, p)| w == p)
    };
    // "this is Dan" and "Dan here" introduce the speaker only at the start
    // of what they say: "is Dan here yet?" asks about someone else.
    let opens_line = |offset: usize| {
        offset == 0 || (offset == 1 && ["hi", "hey", "hello", "היי", "שלום"].contains(&words[0].as_str()))
    };
    before(&["i'm"])
        || before(&["im"])
        || before(&["i", "am"])
        || before(&["my", "name", "is"])
        || (before(&["this", "is"]) && opens_line(at - 2))
        || before(&["אני"])
        || before(&["שמי"])
        || before(&["קוראים", "לי"])
        || (opens_line(at) && words.get(at + name_len).is_some_and(|w| w == "here"))
}

/// Names Dor types are kept as written ("J. R. Smith", "Dr. Cohen"), only
/// trimmed and capped.
fn clean_typed_name(name: &str) -> Option<String> {
    let name = name.trim();
    (!name.is_empty()).then(|| name.chars().take(2 * MAX_NAME_CHARS).collect())
}

/// Names from the model must look like a name.
fn clean_name(name: &str) -> Option<String> {
    let name = name.trim().trim_matches(|c: char| !c.is_alphanumeric());
    let ok = !name.is_empty()
        && name.chars().count() <= MAX_NAME_CHARS
        && name.split_whitespace().count() <= 3
        && name.chars().all(|c| c.is_alphabetic() || c == ' ' || c == '-' || c == '\'');
    ok.then(|| name.to_string())
}

/// Keeps proposals whose quoted line holds the name (capitalised there, in
/// scripts with case, so "I will" does not back "Will") and that are not a
/// name Dor introduces himself with. In order, without duplicates.
#[cfg(test)]
fn accept_names(lines: &[Line], proposals: &[Proposal]) -> Vec<String> {
    accept_proposals(lines, proposals).into_iter().map(|p| p.name).collect()
}

/// `accept_names`, keeping the line each name was found on.
fn accept_proposals(lines: &[Line], proposals: &[Proposal]) -> Vec<Proposal> {
    let introduces = |side: Side, name_words: &[String]| {
        lines.iter().filter(|l| l.side == side).any(|l| {
            let w = lowercase(&raw_words(&l.text));
            find_name(&w, name_words).is_some_and(|at| is_self_intro(&w, at, name_words.len()))
        })
    };
    // When the other side introduces themselves with it too, it is theirs
    // ("this is Sarah, our new designer" from Dor, then "I'm Sarah").
    let own_name =
        |name_words: &[String]| introduces(Side::Me, name_words) && !introduces(Side::Others, name_words);

    let mut accepted: Vec<Proposal> = Vec::new();
    for proposal in proposals {
        let Some(name) = clean_name(&proposal.name) else { continue };
        let Some(line) = lines.get(proposal.line) else { continue };
        let name_words = lowercase(&raw_words(&name));
        let written = raw_words(&line.text);
        let Some(at) = find_name(&lowercase(&written), &name_words) else { continue };
        let capitalised = written[at].chars().next().is_some_and(|c| !c.is_lowercase());
        if !capitalised || own_name(&name_words) {
            continue;
        }
        // "Ana" and "Ana Silva" are the same person.
        let same_person = |a: &String| {
            let a = lowercase(&raw_words(a));
            find_name(&a, &name_words).is_some() || find_name(&name_words, &a).is_some()
        };
        if !accepted.iter().any(|a| same_person(&a.name)) {
            accepted.push(Proposal { name, line: proposal.line });
        }
        if accepted.len() == MAX_NAMES {
            break;
        }
    }
    accepted
}

/// How far after a name said to someone the answer is looked for.
const ANSWER_LOOKAHEAD_LINES: usize = 3;

/// The voice each accepted name belongs to, as (label, name). A line that
/// introduces its speaker ("I'm Sarah") names that line's voice. A name said
/// to someone ("Alyssa, you're up first") names the next voice that answers,
/// a labelled Others line from a different voice within a few lines. Each
/// voice and each name is used once, first found first. These are offered as
/// guesses for Dor to confirm, never saved as names on their own.
fn voices_for_names(lines: &[Line], accepted: &[Proposal]) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = Vec::new();
    for p in accepted {
        let Some(line) = lines.get(p.line) else { continue };
        let words = lowercase(&raw_words(&line.text));
        let name_words = lowercase(&raw_words(&p.name));
        let Some(at) = find_name(&words, &name_words) else { continue };
        let label = if line.side == Side::Others && is_self_intro(&words, at, name_words.len()) {
            line.label.clone()
        } else {
            lines[p.line + 1..]
                .iter()
                .take(ANSWER_LOOKAHEAD_LINES)
                .find(|l| l.side == Side::Others && l.label.is_some() && (line.side == Side::Me || l.label != line.label))
                .and_then(|l| l.label.clone())
        };
        let Some(label) = label else { continue };
        if found.iter().any(|(l, n)| *l == label || *n == p.name) {
            continue;
        }
        found.push((label, p.name.clone()));
    }
    found
}

async fn meeting_folder(pool: &SqlitePool, meeting_id: &str) -> Option<std::path::PathBuf> {
    sqlx::query_scalar::<_, Option<String>>("SELECT folder_path FROM meetings WHERE id = ?")
        .bind(meeting_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .flatten()
        .map(std::path::PathBuf::from)
}

/// Puts each voice's heard name into the meeting's voiceprints, clearing
/// names an earlier pass left on voices that no longer have one. Returns
/// whether anything changed.
fn store_heard_names(folder: &std::path::Path, heard: &[(String, String)]) -> std::io::Result<bool> {
    use crate::diarization::voiceprints::{read_voiceprints, write_voiceprints};
    let mut voices = read_voiceprints(folder)?;
    let mut changed = false;
    for v in voices.iter_mut() {
        let name = heard.iter().find(|(label, _)| *label == v.label).map(|(_, n)| n.clone());
        if v.heard_name != name {
            v.heard_name = name;
            changed = true;
        }
    }
    if changed {
        write_voiceprints(folder, &voices)?;
    }
    Ok(changed)
}

async fn load_lines(pool: &SqlitePool, meeting_id: &str) -> Result<Vec<Line>, sqlx::Error> {
    let rows: Vec<(String, Option<String>, String)> = sqlx::query_as(
        "SELECT speaker, speaker_label, transcript FROM transcripts
         WHERE meeting_id = ? AND speaker IS NOT NULL
         ORDER BY audio_start_time, timestamp",
    )
    .bind(meeting_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(speaker, label, text)| {
            let text = text.trim().to_string();
            Some(Line { side: side_of(&speaker)?, label, text }).filter(|l| !l.text.is_empty())
        })
        .collect())
}

/// Names live in their own table, created on first use instead of by a
/// migration: sqlx refuses to open a database that has a migration it does
/// not know, which would lock out an upstream build on the same data.
async fn ensure_table(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS meeting_other_speakers (
            meeting_id TEXT PRIMARY KEY,
            names TEXT NOT NULL,
            edited INTEGER NOT NULL DEFAULT 0
        )",
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_other_speaker_names(pool: &SqlitePool, meeting_id: &str) -> Result<Vec<String>, sqlx::Error> {
    ensure_table(pool).await?;
    let stored: Option<String> =
        sqlx::query_scalar("SELECT names FROM meeting_other_speakers WHERE meeting_id = ?")
            .bind(meeting_id)
            .fetch_optional(pool)
            .await?;
    Ok(stored.and_then(|json| serde_json::from_str(&json).ok()).unwrap_or_default())
}

/// Names Dor typed on the meeting page. An empty list clears the label but
/// still marks the meeting as edited, so an automatic pass will not refill it.
pub async fn set_other_speaker_names(
    pool: &SqlitePool,
    meeting_id: &str,
    names: &[String],
) -> Result<Vec<String>, sqlx::Error> {
    let names: Vec<String> = names.iter().filter_map(|n| clean_typed_name(n)).take(MAX_NAMES).collect();
    ensure_table(pool).await?;
    sqlx::query("INSERT OR REPLACE INTO meeting_other_speakers (meeting_id, names, edited) VALUES (?, ?, 1)")
        .bind(meeting_id)
        .bind(serde_json::to_string(&names).unwrap_or_default())
        .execute(pool)
        .await?;
    Ok(names)
}

/// Stores names found automatically, unless Dor already set them by hand.
async fn store_found_names(pool: &SqlitePool, meeting_id: &str, names: &[String]) -> Result<bool, sqlx::Error> {
    ensure_table(pool).await?;
    let stored = sqlx::query(
        "INSERT INTO meeting_other_speakers (meeting_id, names, edited) VALUES (?, ?, 0)
         ON CONFLICT(meeting_id) DO UPDATE SET names = excluded.names WHERE edited = 0",
    )
    .bind(meeting_id)
    .bind(serde_json::to_string(names).unwrap_or_default())
    .execute(pool)
    .await?;
    Ok(stored.rows_affected() > 0)
}

/// Finds and stores the other side's names for a saved meeting. Failures are
/// logged and otherwise ignored: the lines just stay labelled "Others".
pub async fn identify_other_speakers<R: Runtime>(app: AppHandle<R>, pool: SqlitePool, meeting_id: String) {
    let lines = match load_lines(&pool, &meeting_id).await {
        Ok(lines) => lines,
        Err(e) => {
            warn!("Speaker names for {meeting_id}: failed to read transcript: {e}");
            return;
        }
    };
    if !lines.iter().any(|l| l.side == Side::Others) {
        return;
    }

    let app_data_dir = app.path().app_data_dir().ok();
    let user_prompt = format!("Transcript:\n{}", numbered_excerpt(&lines));
    let raw = match ask_llm(&pool, app_data_dir.as_ref(), SYSTEM_PROMPT, &user_prompt).await {
        Ok(raw) => raw,
        Err(e) => {
            warn!("Speaker names for {meeting_id}: {e}");
            return;
        }
    };
    let Some(proposals) = parse_reply(&raw) else {
        warn!("Speaker names for {meeting_id}: unusable model output ({} chars)", raw.chars().count());
        return;
    };
    // An empty result is stored too: after a retranscription it replaces a
    // name the earlier, worse transcript produced (unless Dor set one).
    let accepted = accept_proposals(&lines, &proposals);
    let names: Vec<String> = accepted.iter().map(|p| p.name.clone()).collect();
    let heard = voices_for_names(&lines, &accepted);
    info!(
        "Speaker names for {meeting_id}: proposed {}, kept {}, matched to a voice {}",
        proposals.len(),
        names.len(),
        heard.len()
    );

    let heard_changed = match meeting_folder(&pool, &meeting_id).await {
        Some(folder) => store_heard_names(&folder, &heard).unwrap_or_else(|e| {
            warn!("Speaker names for {meeting_id}: voice names not saved: {e}");
            false
        }),
        None => false,
    };
    let names_changed = match store_found_names(&pool, &meeting_id, &names).await {
        Ok(stored) => {
            if !stored {
                info!("Speaker names for {meeting_id}: kept the names set by hand");
            }
            stored
        }
        Err(e) => {
            warn!("Speaker names for {meeting_id}: failed to save: {e}");
            false
        }
    };
    // The meeting page reloads the "Other side" names and the voice guesses.
    if names_changed || heard_changed {
        let names = get_other_speaker_names(&pool, &meeting_id).await.unwrap_or(names);
        let _ = app.emit("meeting-speakers-identified", MeetingSpeakersIdentified { meeting_id, names });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me(text: &str) -> Line {
        Line { side: Side::Me, label: None, text: text.to_string() }
    }
    fn others(text: &str) -> Line {
        Line { side: Side::Others, label: None, text: text.to_string() }
    }
    fn voice(label: &str, text: &str) -> Line {
        Line { side: Side::Others, label: Some(label.to_string()), text: text.to_string() }
    }

    #[test]
    fn a_name_said_to_someone_goes_to_the_voice_that_answers() {
        // The CNN podcast test: the host hands over to each guest by name.
        let lines = vec![
            voice("Speaker 1", "Something you can't stop talking about. Alright, Alyssa, you're up first."),
            voice("Speaker 2", "For what? And for the record, I also say thank you."),
            voice("Speaker 1", "Okay. Patrick, you've got an obsession."),
            voice("Speaker 3", "Well I think this started when I turned sixty."),
        ];
        let accepted = accept_proposals(&lines, &[proposal("Alyssa", 0), proposal("Patrick", 2)]);
        assert_eq!(
            voices_for_names(&lines, &accepted),
            vec![("Speaker 2".to_string(), "Alyssa".to_string()), ("Speaker 3".to_string(), "Patrick".to_string())]
        );
    }

    #[test]
    fn a_self_introduction_names_its_own_voice() {
        let lines = vec![me("Who's joining?"), voice("Speaker 4", "Hi, I'm Dana from support."), voice("Speaker 1", "Welcome.")];
        let accepted = accept_proposals(&lines, &[proposal("Dana", 1)]);
        assert_eq!(voices_for_names(&lines, &accepted), vec![("Speaker 4".to_string(), "Dana".to_string())]);
    }

    #[test]
    fn a_name_with_no_other_voice_answering_names_nobody() {
        // Only the same voice keeps talking, or the answer has no label.
        let lines = vec![voice("Speaker 1", "Thanks, Dan."), voice("Speaker 1", "Moving on."), others("Sure.")];
        let accepted = accept_proposals(&lines, &[proposal("Dan", 0)]);
        assert!(voices_for_names(&lines, &accepted).is_empty());
    }

    #[test]
    fn each_voice_gets_one_name() {
        let lines = vec![
            me("Sarah, can you start?"),
            voice("Speaker 2", "Sure."),
            me("And Tom, you're after Sarah."),
            voice("Speaker 2", "Yes."),
        ];
        let accepted = accept_proposals(&lines, &[proposal("Sarah", 0), proposal("Tom", 2)]);
        assert_eq!(voices_for_names(&lines, &accepted), vec![("Speaker 2".to_string(), "Sarah".to_string())]);
    }

    #[test]
    fn heard_names_replace_the_last_pass_in_the_voiceprints() {
        use crate::diarization::voiceprints::{read_voiceprints, write_voiceprints, Voiceprint};
        let folder = tempfile::tempdir().unwrap();
        let print = |label: &str, heard: Option<&str>| Voiceprint {
            label: label.into(), segments: 3, embedding: vec![1.0], name: None, dismissed: false,
            heard_name: heard.map(Into::into), lines: 3,
        };
        write_voiceprints(folder.path(), &[print("Speaker 1", Some("Old")), print("Speaker 2", None)]).unwrap();
        let heard = vec![("Speaker 2".to_string(), "Alyssa".to_string())];
        assert!(store_heard_names(folder.path(), &heard).unwrap());
        let voices = read_voiceprints(folder.path()).unwrap();
        assert_eq!(voices[0].heard_name, None);
        assert_eq!(voices[1].heard_name.as_deref(), Some("Alyssa"));
        assert!(!store_heard_names(folder.path(), &heard).unwrap());
    }
    fn proposal(name: &str, line: usize) -> Proposal {
        Proposal { name: name.to_string(), line }
    }

    #[test]
    fn a_name_in_the_quoted_line_is_kept() {
        let lines = vec![me("Hello?"), others("Hi, this is Sarah from finance."), me("Okay Sarah let's start")];
        assert_eq!(accept_names(&lines, &[proposal("Sarah", 1)]), vec!["Sarah"]);
        assert_eq!(accept_names(&lines, &[proposal("Sarah", 2)]), vec!["Sarah"]);
    }

    #[test]
    fn misquoted_proposals_are_dropped() {
        let lines = vec![others("This is Sarah."), me("Hi.")];
        assert!(accept_names(&lines, &[proposal("Maria", 0), proposal("Sarah", 7)]).is_empty());
    }

    #[test]
    fn ordinary_words_are_not_names() {
        let lines = vec![me("I will, thanks."), others("Great.")];
        assert!(accept_names(&lines, &[proposal("Will", 0)]).is_empty());
    }

    #[test]
    fn the_users_own_name_is_dropped() {
        let lines = vec![me("Hi, I'm Dor."), others("Hi Dor, thanks for joining.")];
        assert!(accept_names(&lines, &[proposal("Dor", 1)]).is_empty());
    }

    #[test]
    fn asking_about_someone_is_not_a_self_introduction() {
        let lines = vec![me("Is Dan here yet?"), others("Hi, I'm Dan.")];
        assert_eq!(accept_names(&lines, &[proposal("Dan", 1)]), vec!["Dan".to_string()]);
        let lines = vec![me("Dan here, can you hear me?"), others("Yes, loud and clear, Dan.")];
        assert!(accept_names(&lines, &[proposal("Dan", 1)]).is_empty());
    }

    #[test]
    fn a_person_dor_introduces_who_then_speaks_is_kept() {
        let lines = vec![me("This is Sarah, our new designer."), others("Hi everyone, I'm Sarah.")];
        assert_eq!(accept_names(&lines, &[proposal("Sarah", 1)]), vec!["Sarah".to_string()]);
    }

    #[test]
    fn replies_with_stray_braces_or_bad_entries_still_parse() {
        let after = r#"{"names":[{"name":"Sarah","line":3}]} Format used: {name, line}"#;
        assert_eq!(parse_reply(after).unwrap().len(), 1);
        assert!(parse_reply("} oops {").is_none());
        let mixed = r#"{"names":[{"name":"Dan","line":"2"},{"name":null,"line":1},{"name":"Ana","line":4}]}"#;
        let names: Vec<String> = parse_reply(mixed).unwrap().into_iter().map(|p| p.name).collect();
        assert_eq!(names, vec!["Dan".to_string(), "Ana".to_string()]);
    }

    #[test]
    fn names_are_cleaned_and_deduplicated() {
        let lines = vec![others("I'm Ana Silva."), me("Ana, welcome."), others("Thanks.")];
        let names = accept_names(&lines, &[proposal(" Ana Silva ", 0), proposal("Ana", 1), proposal("<x>", 0)]);
        assert_eq!(names, vec!["Ana Silva"]);
    }

    #[test]
    fn hebrew_names_are_kept() {
        let lines = vec![me("שלום"), others("היי, אני דנה מהשיווק")];
        assert_eq!(accept_names(&lines, &[proposal("דנה", 1)]), vec!["דנה"]);
    }

    #[test]
    fn reply_json_is_found_inside_reasoning_and_fences() {
        let raw = "<think>Look for names.</think>\n```json\n{\"names\": [{\"name\": \"Sarah\", \"line\": 3}]}\n```";
        let names = parse_reply(raw).unwrap();
        assert_eq!((names[0].name.as_str(), names[0].line), ("Sarah", 3));
        assert!(parse_reply("no names here").is_none());
        assert!(parse_reply("{\"names\": []}").unwrap().is_empty());
    }

    #[test]
    fn long_calls_keep_the_start_and_end() {
        let mut lines: Vec<Line> = (0..400).map(|i| others(&format!("line number {i} with some words"))).collect();
        lines[0] = others("This is Sarah.");
        lines[399] = me("Thanks Sarah, bye.");
        let excerpt = numbered_excerpt(&lines);
        assert!(excerpt.starts_with("[0] Others: This is Sarah."));
        assert!(excerpt.ends_with("[399] Me: Thanks Sarah, bye."));
        assert!(excerpt.contains("[...]"));
    }

    #[tokio::test]
    async fn names_set_by_hand_survive_an_automatic_pass() {
        // One connection: each in-memory connection is its own database.
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        assert!(store_found_names(&pool, "m1", &["Sarah".to_string()]).await.unwrap());
        assert_eq!(get_other_speaker_names(&pool, "m1").await.unwrap(), vec!["Sarah"]);
        // Dor corrects it; a retranscription then finds "Sara" again.
        set_other_speaker_names(&pool, "m1", &["Sarah Cohen".to_string()]).await.unwrap();
        assert!(!store_found_names(&pool, "m1", &["Sara".to_string()]).await.unwrap());
        assert_eq!(get_other_speaker_names(&pool, "m1").await.unwrap(), vec!["Sarah Cohen"]);
        // Names typed by hand are kept as written.
        let typed = set_other_speaker_names(&pool, "m1", &["J. R. Smith".to_string(), " ".to_string()]).await.unwrap();
        assert_eq!(typed, vec!["J. R. Smith"]);
        // Clearing by hand also sticks.
        set_other_speaker_names(&pool, "m1", &[]).await.unwrap();
        assert!(!store_found_names(&pool, "m1", &["Sara".to_string()]).await.unwrap());
        assert!(get_other_speaker_names(&pool, "m1").await.unwrap().is_empty());
    }
}
