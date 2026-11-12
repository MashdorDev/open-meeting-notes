//! Finds the names of the people on the other side of a call from what is
//! said in it ("I'm Sarah", "thanks, Dan"), after the transcript is saved.
//!
//! The model only proposes names with the line they come from. A name is kept
//! only if that line really contains it, and only as a self-introduction on
//! an Others line or as Dor addressing someone on a Me line that the other
//! side answers. Mentions of third people ("I talked to John") are dropped.

use crate::summary::title::{ask_llm, visible_reply};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tracing::{info, warn};

// Greetings and introductions sit at the start of a call, thanks at the end.
const HEAD_CHARS: usize = 4500;
const TAIL_CHARS: usize = 1500;
const MAX_NAME_CHARS: usize = 40;
const MAX_NAMES: usize = 5;
// Lines after a Me line in which the other side has to answer.
const REPLY_WINDOW: usize = 2;
// Words right before a name that address that person ("thanks Dan").
const GREETINGS: &[&str] = &[
    "hi", "hey", "hello", "morning", "afternoon", "evening", "thanks", "bye", "welcome", "cheers",
    "היי", "שלום", "תודה", "ביי", "בוקר",
];
const GREETING_PAIRS: &[(&str, &str)] = &[("thank", "you"), ("meet", "you"), ("see", "you")];

const SYSTEM_PROMPT: &str = "You find the names of the people on the other side of a call. \
Each transcript line is numbered and starts with Me: (the user) or Others: (the other side). \
Reply with only JSON: {\"names\": [{\"name\": \"Sarah\", \"line\": 12}]}. \
List a name only if an Others line introduces that person (\"I'm Sarah\", \"this is Dan\") \
or a Me line speaks to someone by name and the other side answers. \
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
    text: String,
}

#[derive(Debug, Deserialize)]
struct Proposal {
    name: String,
    line: usize,
}

#[derive(Debug, Deserialize)]
struct Reply {
    #[serde(default)]
    names: Vec<Proposal>,
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
            let who = if l.side == Side::Me { "Me" } else { "Others" };
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
    let json = &visible[visible.find('{')?..=visible.rfind('}')?];
    serde_json::from_str::<Reply>(json).ok().map(|r| r.names)
}

/// Lowercased words of `text` without surrounding punctuation.
fn words(text: &str) -> Vec<String> {
    tokens(text).into_iter().map(|t| t.word).collect()
}

struct Token {
    /// Lowercased, without surrounding punctuation.
    word: String,
    /// Starts with a capital letter, or is in a script without case (Hebrew).
    capital: bool,
    /// Followed by a comma, question mark or exclamation mark.
    ends_clause: bool,
    /// Followed by a full stop, question mark or exclamation mark.
    ends_sentence: bool,
}

fn tokens(text: &str) -> Vec<Token> {
    text.split_whitespace()
        .filter_map(|raw| {
            let core = raw.trim_matches(|c: char| !c.is_alphanumeric() && c != '\'');
            let first = core.chars().next()?;
            let tail = raw.trim_end_matches(|c: char| c == '"' || c == '\'' || c == ')');
            Some(Token {
                word: core.to_lowercase(),
                capital: !first.is_lowercase(),
                ends_clause: tail.ends_with([',', '?', '!']),
                ends_sentence: tail.ends_with(['.', '?', '!']),
            })
        })
        .collect()
}

/// Position of the name's words inside `words`, if they appear in order.
fn find_name(words: &[String], name: &[String]) -> Option<usize> {
    (0..=words.len().checked_sub(name.len())?).find(|&i| words[i..i + name.len()] == *name)
}

fn is_self_intro(words: &[String], at: usize, name_len: usize) -> bool {
    let before = |phrase: &[&str]| {
        at >= phrase.len() && words[at - phrase.len()..at].iter().zip(phrase).all(|(w, p)| w == p)
    };
    before(&["i'm"])
        || before(&["im"])
        || before(&["i", "am"])
        || before(&["my", "name", "is"])
        || before(&["this", "is"])
        || before(&["אני"])
        || before(&["שמי"])
        || before(&["קוראים", "לי"])
        || words.get(at + name_len).is_some_and(|w| w == "here")
}

/// The name at `at..at + len` speaks to someone: "Dan, ...", "... Dan?" as a
/// sentence of its own, "thanks Dan", or "that works, Dan" at the end.
fn is_vocative(t: &[Token], at: usize, len: usize) -> bool {
    let last = at + len - 1;
    let line_end = last + 1 == t.len();
    let sentence_start = at == 0 || t[at - 1].ends_sentence;
    let greeted = (at >= 1 && GREETINGS.contains(&t[at - 1].word.as_str()))
        || (at >= 2 && GREETING_PAIRS.contains(&(t[at - 2].word.as_str(), t[at - 1].word.as_str())));
    let leads = sentence_start && (t[last].ends_clause || t[last].ends_sentence || line_end);
    let trails = line_end && at >= 1 && t[at - 1].ends_clause;
    greeted || leads || trails
}

/// Whether the name at `at..at + len` on line `idx` names the other side.
fn backed(lines: &[Line], idx: usize, at: usize, len: usize) -> bool {
    let t = tokens(&lines[idx].text);
    // A lowercase "will" or "may" is a word, not a name.
    if !t[at].capital {
        return false;
    }
    let w: Vec<String> = t.iter().map(|t| t.word.clone()).collect();
    match lines[idx].side {
        Side::Others => is_self_intro(&w, at, len),
        Side::Me => {
            let answered = lines[idx + 1..].iter().take(REPLY_WINDOW).any(|l| l.side == Side::Others);
            answered && !is_self_intro(&w, at, len) && is_vocative(&t, at, len)
        }
    }
}

/// Cheap check before asking the model: does any word sit where a name of
/// the other side could? Most calls have no introductions or greetings by
/// name, and the model call would hold up the summary.
fn has_name_candidates(lines: &[Line]) -> bool {
    lines.iter().enumerate().any(|(idx, line)| {
        let t = tokens(&line.text);
        (0..t.len()).any(|at| {
            t[at].word.chars().all(char::is_alphabetic) && t[at].word != "i" && backed(lines, idx, at, 1)
        })
    })
}

fn clean_name(name: &str) -> Option<String> {
    let name = name.trim().trim_matches(|c: char| !c.is_alphanumeric());
    let ok = !name.is_empty()
        && name.chars().count() <= MAX_NAME_CHARS
        && name.split_whitespace().count() <= 3
        && name.chars().all(|c| c.is_alphabetic() || c == ' ' || c == '-' || c == '\'');
    ok.then(|| name.to_string())
}

/// Keeps the proposals the transcript backs up, in order, without duplicates.
fn accept_names(lines: &[Line], proposals: &[Proposal]) -> Vec<String> {
    // Names Dor introduces himself with are his own, never the other side's.
    let own_name = |name_words: &[String]| {
        lines.iter().filter(|l| l.side == Side::Me).any(|l| {
            let w = words(&l.text);
            find_name(&w, name_words).is_some_and(|at| is_self_intro(&w, at, name_words.len()))
        })
    };

    let mut accepted: Vec<String> = Vec::new();
    for proposal in proposals {
        let Some(name) = clean_name(&proposal.name) else { continue };
        let Some(line) = lines.get(proposal.line) else { continue };
        let name_words = words(&name);
        let Some(at) = find_name(&words(&line.text), &name_words) else { continue };
        if !backed(lines, proposal.line, at, name_words.len()) || own_name(&name_words) {
            continue;
        }
        // "Ana" and "Ana Silva" are the same person.
        let same_person = |a: &String| {
            let a = words(a);
            find_name(&a, &name_words).is_some() || find_name(&name_words, &a).is_some()
        };
        if !accepted.iter().any(same_person) {
            accepted.push(name);
        }
        if accepted.len() == MAX_NAMES {
            break;
        }
    }
    accepted
}

async fn load_lines(pool: &SqlitePool, meeting_id: &str) -> Result<Vec<Line>, sqlx::Error> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT speaker, transcript FROM transcripts
         WHERE meeting_id = ? AND speaker IS NOT NULL
         ORDER BY audio_start_time, timestamp",
    )
    .bind(meeting_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(speaker, text)| {
            let text = text.trim().to_string();
            Some(Line { side: side_of(&speaker)?, text }).filter(|l| !l.text.is_empty())
        })
        .collect())
}

/// Names live in their own table, created on first use instead of by a
/// migration: sqlx refuses to open a database that has a migration it does
/// not know, which would lock out an upstream Meetily build on the same data.
async fn ensure_table(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS meeting_speaker_names (
            meeting_id TEXT PRIMARY KEY,
            names TEXT NOT NULL
        )",
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_other_speaker_names(pool: &SqlitePool, meeting_id: &str) -> Result<Vec<String>, sqlx::Error> {
    ensure_table(pool).await?;
    let stored: Option<String> =
        sqlx::query_scalar("SELECT names FROM meeting_speaker_names WHERE meeting_id = ?")
            .bind(meeting_id)
            .fetch_optional(pool)
            .await?;
    Ok(stored.and_then(|json| serde_json::from_str(&json).ok()).unwrap_or_default())
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
    if !lines.iter().any(|l| l.side == Side::Others) || !has_name_candidates(&lines) {
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
        warn!("Speaker names for {meeting_id}: unusable model output {raw:?}");
        return;
    };
    let names = accept_names(&lines, &proposals);
    info!("Speaker names for {meeting_id}: proposed {proposals:?}, kept {names:?}");
    if names.is_empty() {
        return;
    }

    let saved = match ensure_table(&pool).await {
        Ok(()) => sqlx::query("INSERT OR REPLACE INTO meeting_speaker_names (meeting_id, names) VALUES (?, ?)")
            .bind(&meeting_id)
            .bind(serde_json::to_string(&names).unwrap_or_default())
            .execute(&pool)
            .await
            .map(|_| ()),
        Err(e) => Err(e),
    };
    match saved {
        Ok(()) => {
            let _ = app.emit("meeting-speakers-identified", MeetingSpeakersIdentified { meeting_id, names });
        }
        Err(e) => warn!("Speaker names for {meeting_id}: failed to save: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me(text: &str) -> Line {
        Line { side: Side::Me, text: text.to_string() }
    }
    fn others(text: &str) -> Line {
        Line { side: Side::Others, text: text.to_string() }
    }
    fn proposal(name: &str, line: usize) -> Proposal {
        Proposal { name: name.to_string(), line }
    }

    #[test]
    fn self_introduction_on_the_other_side_is_kept() {
        let lines = vec![me("Hello?"), others("Hi, this is Sarah from finance."), me("Great.")];
        assert_eq!(accept_names(&lines, &[proposal("Sarah", 1)]), vec!["Sarah"]);
    }

    #[test]
    fn addressing_someone_who_answers_is_kept() {
        let lines = vec![me("Dan, what do you think?"), others("I think we should wait.")];
        assert_eq!(accept_names(&lines, &[proposal("Dan", 0)]), vec!["Dan"]);
        let lines = vec![me("That works, thanks Dan."), others("Sure.")];
        assert_eq!(accept_names(&lines, &[proposal("Dan", 0)]), vec!["Dan"]);
    }

    #[test]
    fn people_only_talked_about_are_dropped() {
        let lines = vec![me("I spoke with John about the budget yesterday afternoon."), others("Okay.")];
        assert!(accept_names(&lines, &[proposal("John", 0)]).is_empty());
        for (line, name) in [
            ("So John called me about the contract", "John"),
            ("I'll check with Sarah", "Sarah"),
            ("Did Mark send it?", "Mark"),
        ] {
            let lines = vec![me(line), others("Not yet.")];
            assert!(accept_names(&lines, &[proposal(name, 0)]).is_empty(), "{line}");
        }
        let lines = vec![others("Honestly it's John who decides."), me("Right.")];
        assert!(accept_names(&lines, &[proposal("John", 0)]).is_empty());
    }

    #[test]
    fn ordinary_words_are_not_names() {
        let lines = vec![me("I will, thanks."), others("Great.")];
        assert!(accept_names(&lines, &[proposal("Will", 0)]).is_empty());
    }

    #[test]
    fn calls_without_candidates_skip_the_model() {
        let lines = vec![me("So what about pricing?"), others("Too high for small teams."), me("Let's ask Maria from sales."), others("Good idea.")];
        assert!(!has_name_candidates(&lines));
        let lines = vec![me("Hello?"), others("Hi, this is Sarah.")];
        assert!(has_name_candidates(&lines));
        let lines = vec![me("Nice. Dan, did you see the report?"), others("Yes.")];
        assert!(has_name_candidates(&lines));
        let lines = vec![others("Maybe ask Lisa about the budget next week."), me("Sure.")];
        assert!(accept_names(&lines, &[proposal("Lisa", 0)]).is_empty());
    }

    #[test]
    fn the_other_side_greeting_the_user_does_not_name_them() {
        let lines = vec![me("Hi, I'm Dor."), others("Hi Dor, thanks for joining.")];
        assert!(accept_names(&lines, &[proposal("Dor", 1)]).is_empty());
        // Even when the model points at a line where Dor is addressed.
        let lines = vec![me("Hi, I'm Dor."), others("Nice."), me("Dor here, can you hear me?"), others("Yes.")];
        assert!(accept_names(&lines, &[proposal("Dor", 2)]).is_empty());
    }

    #[test]
    fn unanswered_or_misquoted_proposals_are_dropped() {
        let lines = vec![me("Thanks Dan."), me("Anyway.")];
        assert!(accept_names(&lines, &[proposal("Dan", 0)]).is_empty());
        let lines = vec![others("This is Sarah."), me("Hi.")];
        assert!(accept_names(&lines, &[proposal("Maria", 0), proposal("Sarah", 7)]).is_empty());
    }

    #[test]
    fn names_are_cleaned_and_deduplicated() {
        let lines = vec![others("I'm Ana Silva."), me("Ana, welcome."), others("Thanks.")];
        let names = accept_names(&lines, &[proposal(" Ana Silva ", 0), proposal("ana", 1), proposal("<x>", 0)]);
        assert_eq!(names, vec!["Ana Silva"]);
    }

    #[test]
    fn hebrew_self_introduction_is_kept() {
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
}
