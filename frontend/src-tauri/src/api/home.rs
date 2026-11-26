//! What the home page shows while nothing is recording: the latest meetings
//! with a line of their summary and how many action items it has, the
//! meetings that still need something from Dor, and this week's totals.

use crate::api::api::{open_speaker_guesses, recent_meetings, RecentMeeting};
use crate::database::models::DateTimeUtc;
use crate::state::AppState;
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::SqlitePool;

/// The latest meeting plus the recent ones under it.
const HOME_MEETINGS: u32 = 4;
/// How far back "Needs attention" looks.
const ATTENTION_DAYS: i64 = 14;
const MAX_ATTENTION: usize = 3;
/// A recording shorter than this is usually a test or a false start; it
/// isn't worth asking for its summary.
const MIN_SUMMARY_SECONDS: f64 = 60.0;
const SUMMARY_LINE_CHARS: usize = 160;

#[derive(Debug, Serialize)]
pub struct HomeMeeting {
    #[serde(flatten)]
    pub meeting: RecentMeeting,
    /// A summary is stored, whatever its status (a cancelled or failed
    /// regeneration keeps the old one, and the meeting page shows it).
    pub has_summary: bool,
    /// The first sentence of the Summary section, when it has one.
    pub summary_line: Option<String>,
    /// Tasks in the summary's task section; None when it has none (some
    /// templates have no such section) or there is no summary.
    pub action_items: Option<usize>,
}

/// A summary run stuck in progress this long is taken as abandoned (the app
/// was closed or crashed mid-run), so the meeting shows as unsummarized.
const STALE_RUN_MINUTES: i64 = 60;

/// Headings that hold the tasks, across the bundled templates.
const TASK_SECTIONS: &[&str] = &["action item", "next step", "to do", "todo", "tasks"];

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AttentionKind {
    /// Suggested speaker names waiting to be confirmed.
    Names,
    /// A transcript with no summary yet.
    NoSummary,
    SummaryFailed,
}

#[derive(Debug, Serialize)]
pub struct Attention {
    pub meeting_id: String,
    pub title: String,
    pub kind: AttentionKind,
    /// How many suggested names, for `Names`.
    pub count: usize,
}

#[derive(Debug, Serialize)]
pub struct HomeOverview {
    pub meetings: Vec<HomeMeeting>,
    pub attention: Vec<Attention>,
    pub week_meetings: usize,
    pub week_seconds: f64,
}

/// The home page's data. `week_start` is the start of this week in the
/// viewer's time zone, as RFC 3339.
#[tauri::command]
pub async fn api_get_home_overview(
    state: tauri::State<'_, AppState>,
    week_start: String,
) -> Result<HomeOverview, String> {
    let week_start = DateTime::parse_from_rfc3339(&week_start)
        .map_err(|e| format!("Bad week start: {e}"))?
        .with_timezone(&Utc);
    home_overview(state.db_manager.pool(), week_start, Utc::now())
        .await
        .map_err(|e| format!("Database error: {e}"))
}

async fn home_overview(pool: &SqlitePool, week_start: DateTime<Utc>, now: DateTime<Utc>) -> Result<HomeOverview, sqlx::Error> {
    let mut meetings: Vec<HomeMeeting> = Vec::new();
    for meeting in recent_meetings(pool, HOME_MEETINGS).await? {
        let stored = stored_summary(pool, &meeting.id).await;
        let markdown = stored.as_ref().and_then(summary_markdown);
        meetings.push(HomeMeeting {
            has_summary: stored.is_some(),
            summary_line: markdown.as_deref().and_then(summary_line),
            action_items: markdown.as_deref().and_then(action_item_count),
            meeting,
        });
    }

    // Every meeting with its start, length and summary status, newest first.
    let rows: Vec<(String, String, DateTimeUtc, Option<f64>, Option<String>, Option<bool>, Option<DateTimeUtc>)> = sqlx::query_as(
        "SELECT m.id, m.title, m.created_at,
                (SELECT MAX(t.audio_end_time) FROM transcripts t WHERE t.meeting_id = m.id),
                LOWER(s.status), s.result IS NOT NULL, s.updated_at
         FROM meetings m LEFT JOIN summary_processes s ON s.meeting_id = m.id
         ORDER BY m.created_at DESC",
    )
    .fetch_all(pool)
    .await?;

    let mut week_meetings = 0;
    let mut week_seconds = 0.0;
    let mut attention = Vec::new();
    let attention_since = now - chrono::Duration::days(ATTENTION_DAYS);
    // The latest meeting's card already says it has no summary.
    let latest_id = meetings.first().map(|m| m.meeting.id.clone());
    for (id, title, created_at, duration, status, has_result, updated_at) in rows {
        if created_at.0 >= week_start {
            week_meetings += 1;
            week_seconds += duration.unwrap_or(0.0);
        }
        if created_at.0 < attention_since || attention.len() >= MAX_ATTENTION {
            continue;
        }
        // One row per meeting. Names first: confirming them also gives the
        // summary the right names.
        let open = open_speaker_guesses(pool, &id).await.map(|g| g.len()).unwrap_or(0);
        if open > 0 {
            attention.push(Attention { meeting_id: id, title, kind: AttentionKind::Names, count: open });
            continue;
        }
        let worth_summarizing = duration.is_some_and(|d| d >= MIN_SUMMARY_SECONDS);
        let stale = updated_at.is_none_or(|t| now - t.0 > chrono::Duration::minutes(STALE_RUN_MINUTES));
        let kind = match (status.as_deref(), has_result.unwrap_or(false)) {
            // A stored summary counts, whatever happened to a later run.
            (_, true) => None,
            (Some("failed"), false) => Some(AttentionKind::SummaryFailed),
            (Some("pending" | "processing"), false) if !stale => None,
            _ if worth_summarizing => Some(AttentionKind::NoSummary),
            _ => None,
        };
        // The latest meeting's card already says it has no summary.
        if kind == Some(AttentionKind::NoSummary) && latest_id.as_deref() == Some(id.as_str()) {
            continue;
        }
        if let Some(kind) = kind {
            attention.push(Attention { meeting_id: id, title, kind, count: 0 });
        }
    }
    Ok(HomeOverview { meetings, attention, week_meetings, week_seconds })
}

/// The stored summary, whatever the run's status, as the meeting page reads it.
async fn stored_summary(pool: &SqlitePool, meeting_id: &str) -> Option<serde_json::Value> {
    let result: Option<String> = sqlx::query_scalar("SELECT result FROM summary_processes WHERE meeting_id = ?")
        .bind(meeting_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .flatten();
    serde_json::from_str(&result?).ok()
}

/// The summary as markdown to read the line and tasks from: the English copy
/// when the summary was translated (the headings are found by their English
/// names), else the markdown. Edited summaries saved only as editor blocks,
/// and legacy ones, have neither.
fn summary_markdown(stored: &serde_json::Value) -> Option<String> {
    stored
        .pointer("/english_cache/markdown")
        .or_else(|| stored.get("markdown"))?
        .as_str()
        .map(str::to_string)
}

/// A heading line's text: "## Action Items", "**Action Items**" or
/// "**Action Items:**" give "action items".
fn heading_text(line: &str) -> Option<String> {
    let t = line.trim();
    let inner = if let Some(rest) = t.strip_prefix('#') {
        rest.trim_start_matches('#').trim()
    } else if t.starts_with("**") && t.ends_with("**") && t.len() > 4 {
        &t[2..t.len() - 2]
    } else {
        return None;
    };
    Some(inner.trim().trim_end_matches(':').trim_matches('*').trim().to_lowercase())
}

/// The lines under the first heading that contains `name`, up to the next
/// heading.
fn section<'a>(markdown: &'a str, name: &str) -> Option<Vec<&'a str>> {
    let mut lines = markdown.lines();
    lines.find(|l| heading_text(l).is_some_and(|h| h.contains(name)))?;
    Some(lines.take_while(|l| heading_text(l).is_none()).collect())
}

fn is_placeholder(text: &str) -> bool {
    let t = text.trim().trim_matches('*').trim().trim_end_matches('.').to_lowercase();
    t.is_empty() || t.starts_with("none noted") || t == "none" || t == "n/a" || t == "-"
}

/// The first sentence of the Summary section, without markdown, cut to a
/// line. None without that section: the first other line is often a field
/// such as a date.
fn summary_line(markdown: &str) -> Option<String> {
    let lines = section(markdown, "summary")?;
    let paragraph = lines.iter().map(|l| l.trim()).find(|l| !l.is_empty() && !l.starts_with('|'))?;
    let plain = paragraph.trim_start_matches(['-', '*', ' ']).replace("**", "");
    if is_placeholder(&plain) {
        return None;
    }
    let sentence = match plain.find(". ") {
        Some(end) => &plain[..=end],
        None => plain.as_str(),
    };
    let mut line: String = sentence.chars().take(SUMMARY_LINE_CHARS).collect();
    if line.chars().count() < sentence.chars().count() {
        line = format!("{}…", line.trim_end());
    }
    Some(line.trim().to_string())
}

/// Rows of the task table, or its list items, that hold a task. None when
/// the summary has no task section at all.
fn action_item_count(markdown: &str) -> Option<usize> {
    let lines = TASK_SECTIONS.iter().find_map(|name| section(markdown, name))?;
    let table_rows: Vec<&str> = lines.iter().map(|l| l.trim()).filter(|l| l.starts_with('|')).collect();
    if !table_rows.is_empty() {
        let is_separator = |row: &str| row.trim_matches('|').split('|').all(|c| c.trim().chars().all(|ch| matches!(ch, '-' | ':' | ' ')));
        return Some(
            table_rows
                .iter()
                .skip(1) // header
                .filter(|row| !is_separator(row))
                .filter(|row| !row.trim_matches('|').split('|').all(is_placeholder))
                .count(),
        );
    }
    let count = lines
        .iter()
        .map(|l| l.trim())
        .filter_map(|l| {
            l.strip_prefix("- ")
                .or_else(|| l.strip_prefix("* "))
                .or_else(|| l.split_once(". ").filter(|(n, _)| n.chars().all(|c| c.is_ascii_digit())).map(|(_, t)| t))
        })
        .filter(|t| !is_placeholder(t))
        .count();
    Some(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUMMARY: &str = "# Quality culture\n\n**Summary**\nThe panel discussed quality culture. It covered audits.\n\n**Action Items**\n\n| **Owner** | Task | Due |\n| --- | --- | --- |\n| Dana | Send the deck | Friday |\n| Tom | Book the room | - |\n\n**Discussion Highlights**\n* Point";

    #[test]
    fn the_summary_line_is_the_first_sentence() {
        assert_eq!(summary_line(SUMMARY).as_deref(), Some("The panel discussed quality culture."));
        assert_eq!(summary_line("## Summary\nNone noted in this section."), None);
        // The daily standup template has no Summary section; its first line is the date.
        assert_eq!(summary_line("**Date**\n2026-11-26\n\n**Today**\n- Ship it"), None);
    }

    #[test]
    fn action_items_count_table_rows_and_list_items() {
        assert_eq!(action_item_count(SUMMARY), Some(2));
        let empty = "**Action Items**\n\n| Owner | Task |\n| --- | --- |\n| None noted in this section. | None noted in this section. |";
        assert_eq!(action_item_count(empty), Some(0));
        assert_eq!(action_item_count("**Action Items**\nNone noted in this section.\n"), Some(0));
        assert_eq!(action_item_count("## Action items\n- Dana: send the deck\n- Tom: book the room\n1. Ana: call back\n## Notes\n- not a task"), Some(3));
        assert_eq!(action_item_count("**Summary**\nNo tasks section at all."), None);
        // The sales call template keeps its tasks under Next Steps.
        assert_eq!(action_item_count("**Next Steps**\n- Send pricing\n- Book a demo"), Some(2));
    }

    #[test]
    fn a_translated_summary_is_read_from_its_english_copy() {
        let stored = serde_json::json!({ "markdown": "**סיכום**\nטקסט", "english_cache": { "markdown": SUMMARY } });
        let md = summary_markdown(&stored).unwrap();
        assert_eq!(action_item_count(&md), Some(2));
        assert!(summary_markdown(&serde_json::json!({ "summary_json": [] })).is_none());
    }

    #[tokio::test]
    async fn the_overview_counts_this_week_and_lists_what_needs_attention() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE meetings (id TEXT PRIMARY KEY, title TEXT, created_at TEXT, folder_path TEXT)").execute(&pool).await.unwrap();
        sqlx::query("CREATE TABLE transcripts (id TEXT PRIMARY KEY, meeting_id TEXT, audio_start_time REAL, audio_end_time REAL, speaker_name TEXT, speaker_label TEXT, speaker_name_pinned INTEGER)").execute(&pool).await.unwrap();
        sqlx::query("CREATE TABLE summary_processes (meeting_id TEXT PRIMARY KEY, status TEXT, result TEXT, updated_at TEXT)").execute(&pool).await.unwrap();
        for (id, title, at, end) in [
            ("a", "Summarized", "2026-11-26T09:00:00Z", 600.0),
            ("b", "Unsummarized", "2026-11-25T09:00:00Z", 600.0),
            ("c", "Failed", "2026-11-24T09:00:00Z", 600.0),
            ("e", "Short test", "2026-11-24T08:00:00Z", 20.0),
            ("d", "Old", "2026-10-01T09:00:00Z", 600.0),
        ] {
            sqlx::query("INSERT INTO meetings (id, title, created_at) VALUES (?, ?, ?)").bind(id).bind(title).bind(at).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO transcripts (id, meeting_id, audio_start_time, audio_end_time) VALUES (?, ?, 0, ?)").bind(format!("t{id}")).bind(id).bind(end).execute(&pool).await.unwrap();
        }
        let result = serde_json::json!({ "markdown": SUMMARY }).to_string();
        // "c" failed with nothing stored; "e" is short. A regeneration of "a"
        // that was cancelled keeps the old summary, so "a" still has one.
        sqlx::query("INSERT INTO summary_processes VALUES ('a', 'CANCELLED', ?, '2026-11-26T09:30:00Z'), ('c', 'FAILED', NULL, '2026-11-24T10:00:00Z')").bind(result).execute(&pool).await.unwrap();

        let now = DateTime::parse_from_rfc3339("2026-11-26T12:00:00Z").unwrap().with_timezone(&Utc);
        let week_start = DateTime::parse_from_rfc3339("2026-11-23T00:00:00Z").unwrap().with_timezone(&Utc);
        let overview = home_overview(&pool, week_start, now).await.unwrap();

        assert_eq!(overview.week_meetings, 4);
        assert_eq!(overview.week_seconds, 1820.0);
        assert!(overview.meetings[0].has_summary);
        assert_eq!(overview.meetings[0].summary_line.as_deref(), Some("The panel discussed quality culture."));
        assert_eq!(overview.meetings[0].action_items, Some(2));
        assert!(!overview.meetings[1].has_summary);
        assert_eq!(overview.meetings[1].action_items, None);
        let kinds: Vec<_> = overview.attention.iter().map(|a| (a.meeting_id.as_str(), &a.kind)).collect();
        assert_eq!(kinds, [("b", &AttentionKind::NoSummary), ("c", &AttentionKind::SummaryFailed)]);
    }
}
