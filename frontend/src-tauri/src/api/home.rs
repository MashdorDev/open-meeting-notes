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
    /// The first sentence of the summary, when there is one.
    pub summary_line: Option<String>,
    /// Action items in the summary; None when there is no summary.
    pub action_items: Option<usize>,
}

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
        let markdown = summary_markdown(pool, &meeting.id).await;
        meetings.push(HomeMeeting {
            summary_line: markdown.as_deref().and_then(summary_line),
            action_items: markdown.as_deref().map(action_item_count),
            meeting,
        });
    }

    // Every meeting with its start, length and summary status, newest first.
    let rows: Vec<(String, String, DateTimeUtc, Option<f64>, Option<String>)> = sqlx::query_as(
        "SELECT m.id, m.title, m.created_at,
                (SELECT MAX(t.audio_end_time) FROM transcripts t WHERE t.meeting_id = m.id),
                (SELECT s.status FROM summary_processes s WHERE s.meeting_id = m.id)
         FROM meetings m ORDER BY m.created_at DESC",
    )
    .fetch_all(pool)
    .await?;

    let mut week_meetings = 0;
    let mut week_seconds = 0.0;
    let mut attention = Vec::new();
    let attention_since = now - chrono::Duration::days(ATTENTION_DAYS);
    // The latest meeting's card already says it has no summary.
    let latest_id = meetings.first().map(|m| m.meeting.id.clone());
    for (id, title, created_at, duration, summary_status) in rows {
        if created_at.0 >= week_start {
            week_meetings += 1;
            week_seconds += duration.unwrap_or(0.0);
        }
        if created_at.0 < attention_since || attention.len() >= MAX_ATTENTION {
            continue;
        }
        // Names first: confirming them also improves the summary's names.
        let open = open_speaker_guesses(pool, &id).await.map(|g| g.len()).unwrap_or(0);
        if open > 0 {
            attention.push(Attention { meeting_id: id.clone(), title: title.clone(), kind: AttentionKind::Names, count: open });
        }
        let kind = match summary_status.as_deref() {
            Some("failed") => Some(AttentionKind::SummaryFailed),
            // Only a meeting with something to summarize, not shown above.
            None if duration.is_some_and(|d| d >= MIN_SUMMARY_SECONDS) && latest_id.as_deref() != Some(id.as_str()) => {
                Some(AttentionKind::NoSummary)
            }
            _ => None,
        };
        if let Some(kind) = kind {
            if attention.len() < MAX_ATTENTION {
                attention.push(Attention { meeting_id: id, title, kind, count: 0 });
            }
        }
    }
    Ok(HomeOverview { meetings, attention, week_meetings, week_seconds })
}

async fn summary_markdown(pool: &SqlitePool, meeting_id: &str) -> Option<String> {
    let result: Option<String> =
        sqlx::query_scalar("SELECT result FROM summary_processes WHERE meeting_id = ? AND status = 'completed'")
            .bind(meeting_id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()
            .flatten();
    let value: serde_json::Value = serde_json::from_str(&result?).ok()?;
    value.get("markdown")?.as_str().map(str::to_string)
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

/// The summary's first sentence, without markdown, cut to a line.
fn summary_line(markdown: &str) -> Option<String> {
    let lines = section(markdown, "summary").unwrap_or_else(|| {
        markdown.lines().filter(|l| heading_text(l).is_none()).collect()
    });
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

/// Rows of the Action Items table, or its list items, that hold a task.
fn action_item_count(markdown: &str) -> usize {
    let Some(lines) = section(markdown, "action item") else { return 0 };
    let table_rows: Vec<&str> = lines.iter().map(|l| l.trim()).filter(|l| l.starts_with('|')).collect();
    if !table_rows.is_empty() {
        let is_separator = |row: &str| row.trim_matches('|').split('|').all(|c| c.trim().chars().all(|ch| matches!(ch, '-' | ':' | ' ')));
        return table_rows
            .iter()
            .skip(1) // header
            .filter(|row| !is_separator(row))
            .filter(|row| !row.trim_matches('|').split('|').all(is_placeholder))
            .count();
    }
    lines
        .iter()
        .map(|l| l.trim())
        .filter_map(|l| {
            l.strip_prefix("- ")
                .or_else(|| l.strip_prefix("* "))
                .or_else(|| l.split_once(". ").filter(|(n, _)| n.chars().all(|c| c.is_ascii_digit())).map(|(_, t)| t))
        })
        .filter(|t| !is_placeholder(t))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUMMARY: &str = "# Quality culture\n\n**Summary**\nThe panel discussed quality culture. It covered audits.\n\n**Action Items**\n\n| **Owner** | Task | Due |\n| --- | --- | --- |\n| Dana | Send the deck | Friday |\n| Tom | Book the room | - |\n\n**Discussion Highlights**\n* Point";

    #[test]
    fn the_summary_line_is_the_first_sentence() {
        assert_eq!(summary_line(SUMMARY).as_deref(), Some("The panel discussed quality culture."));
        assert_eq!(summary_line("## Summary\nNone noted in this section."), None);
    }

    #[test]
    fn action_items_count_table_rows_and_list_items() {
        assert_eq!(action_item_count(SUMMARY), 2);
        let empty = "**Action Items**\n\n| Owner | Task |\n| --- | --- |\n| None noted in this section. | None noted in this section. |";
        assert_eq!(action_item_count(empty), 0);
        assert_eq!(action_item_count("**Action Items**\nNone noted in this section.\n"), 0);
        assert_eq!(action_item_count("## Action items\n- Dana: send the deck\n- Tom: book the room\n1. Ana: call back\n## Notes\n- not a task"), 3);
        assert_eq!(action_item_count("**Summary**\nNo tasks section at all."), 0);
    }

    #[tokio::test]
    async fn the_overview_counts_this_week_and_lists_what_needs_attention() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE meetings (id TEXT PRIMARY KEY, title TEXT, created_at TEXT, folder_path TEXT)").execute(&pool).await.unwrap();
        sqlx::query("CREATE TABLE transcripts (id TEXT PRIMARY KEY, meeting_id TEXT, audio_start_time REAL, audio_end_time REAL, speaker_name TEXT, speaker_label TEXT, speaker_name_pinned INTEGER)").execute(&pool).await.unwrap();
        sqlx::query("CREATE TABLE summary_processes (meeting_id TEXT PRIMARY KEY, status TEXT, result TEXT)").execute(&pool).await.unwrap();
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
        sqlx::query("INSERT INTO summary_processes VALUES ('a', 'completed', ?), ('c', 'failed', NULL)").bind(result).execute(&pool).await.unwrap();

        let now = DateTime::parse_from_rfc3339("2026-11-26T12:00:00Z").unwrap().with_timezone(&Utc);
        let week_start = DateTime::parse_from_rfc3339("2026-11-23T00:00:00Z").unwrap().with_timezone(&Utc);
        let overview = home_overview(&pool, week_start, now).await.unwrap();

        assert_eq!(overview.week_meetings, 4);
        assert_eq!(overview.week_seconds, 1820.0);
        assert_eq!(overview.meetings[0].summary_line.as_deref(), Some("The panel discussed quality culture."));
        assert_eq!(overview.meetings[0].action_items, Some(2));
        assert_eq!(overview.meetings[1].action_items, None);
        let kinds: Vec<_> = overview.attention.iter().map(|a| (a.meeting_id.as_str(), &a.kind)).collect();
        assert_eq!(kinds, [("b", &AttentionKind::NoSummary), ("c", &AttentionKind::SummaryFailed)]);
    }
}
