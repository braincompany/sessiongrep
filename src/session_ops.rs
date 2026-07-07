//! Local session analysis helpers for MCP cross-session reasoning tools.
//!
//! All functions are extractive and deterministic — no cloud LLM calls.

use chrono::{DateTime, Utc};

use crate::models::{SessionRecord, SessionWithTranscript};
use crate::util::{compact_whitespace, truncate_for_display};

/// Bounded local summary of a session transcript.
#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub session_id: String,
    pub provider: String,
    pub title: String,
    pub cwd: String,
    pub message_lines: usize,
    pub tool_like_lines: usize,
    pub first_user_intent: Option<String>,
    pub last_user_intent: Option<String>,
    pub started_at: Option<String>,
    pub updated_at: Option<String>,
}

/// Metadata + transcript delta between two sessions.
#[derive(Debug, Clone)]
pub struct SessionDiff {
    pub session_a_id: String,
    pub session_b_id: String,
    pub metadata_changes: Vec<String>,
    pub lines_only_in_a: Vec<String>,
    pub lines_only_in_b: Vec<String>,
    pub line_count_a: usize,
    pub line_count_b: usize,
}

/// One day-bucket in a repo timeline.
#[derive(Debug, Clone)]
pub struct TimelineDay {
    pub date: String,
    pub sessions: Vec<TimelineEntry>,
}

#[derive(Debug, Clone)]
pub struct TimelineEntry {
    pub session_id: String,
    pub provider: String,
    pub title: String,
    pub updated_at: String,
    pub cwd: String,
}

pub fn summarize_session(session: &SessionWithTranscript, max_snippet_len: usize) -> SessionSummary {
    let lines: Vec<&str> = session.transcript_text.lines().collect();
    let tool_like_lines = lines
        .iter()
        .filter(|line| looks_like_tool_line(line))
        .count();

    let user_intents = extract_user_intents(&session.transcript_text);
    let title = session
        .session
        .title
        .as_deref()
        .map(|t| truncate_for_display(t, max_snippet_len))
        .unwrap_or_else(|| "(untitled)".to_string());

    SessionSummary {
        session_id: session.session.id.clone(),
        provider: session.session.provider.to_string(),
        title,
        cwd: session
            .session
            .cwd
            .as_deref()
            .unwrap_or("-")
            .to_string(),
        message_lines: lines.len(),
        tool_like_lines,
        first_user_intent: user_intents.first().cloned(),
        last_user_intent: user_intents.last().cloned(),
        started_at: session
            .session
            .created_at
            .map(format_timestamp),
        updated_at: session
            .session
            .updated_at
            .map(format_timestamp),
    }
}

pub fn format_summary(summary: &SessionSummary) -> String {
    let mut out = format!(
        "# Session summary: {}\n\n- Provider: {}\n- Title: {}\n- CWD: {}\n- Transcript lines: {}\n- Tool-like lines: {}\n",
        summary.session_id,
        summary.provider,
        summary.title,
        summary.cwd,
        summary.message_lines,
        summary.tool_like_lines,
    );
    if let Some(started) = &summary.started_at {
        out.push_str(&format!("- Started: {started}\n"));
    }
    if let Some(updated) = &summary.updated_at {
        out.push_str(&format!("- Updated: {updated}\n"));
    }
    if let Some(first) = &summary.first_user_intent {
        out.push_str(&format!("\n## First user intent\n{first}\n"));
    }
    if let Some(last) = &summary.last_user_intent {
        if summary.first_user_intent.as_deref() != Some(last.as_str()) {
            out.push_str(&format!("\n## Last user intent\n{last}\n"));
        }
    }
    out
}

pub fn diff_sessions(
    a: &SessionWithTranscript,
    b: &SessionWithTranscript,
    max_unique_lines: usize,
) -> SessionDiff {
    let mut metadata_changes = Vec::new();
    if a.session.provider != b.session.provider {
        metadata_changes.push(format!(
            "provider: {} → {}",
            a.session.provider, b.session.provider
        ));
    }
    if a.session.title != b.session.title {
        metadata_changes.push(format!(
            "title: {:?} → {:?}",
            a.session.title, b.session.title
        ));
    }
    if a.session.cwd != b.session.cwd {
        metadata_changes.push(format!("cwd: {:?} → {:?}", a.session.cwd, b.session.cwd));
    }
    if a.session.repo_root != b.session.repo_root {
        metadata_changes.push(format!(
            "repo_root: {:?} → {:?}",
            a.session.repo_root, b.session.repo_root
        ));
    }

    let lines_a: Vec<String> = normalize_transcript_lines(&a.transcript_text);
    let lines_b: Vec<String> = normalize_transcript_lines(&b.transcript_text);

    let set_b: std::collections::HashSet<&str> =
        lines_b.iter().map(String::as_str).collect();
    let set_a: std::collections::HashSet<&str> =
        lines_a.iter().map(String::as_str).collect();

    let mut only_a: Vec<String> = lines_a
        .iter()
        .filter(|line| !set_b.contains(line.as_str()))
        .take(max_unique_lines)
        .cloned()
        .collect();
    let mut only_b: Vec<String> = lines_b
        .iter()
        .filter(|line| !set_a.contains(line.as_str()))
        .take(max_unique_lines)
        .cloned()
        .collect();

    only_a.sort();
    only_b.sort();

    SessionDiff {
        session_a_id: a.session.id.clone(),
        session_b_id: b.session.id.clone(),
        metadata_changes,
        lines_only_in_a: only_a,
        lines_only_in_b: only_b,
        line_count_a: lines_a.len(),
        line_count_b: lines_b.len(),
    }
}

pub fn format_diff(diff: &SessionDiff) -> String {
    let mut out = format!(
        "# Session diff\n\n- A: {} ({} lines)\n- B: {} ({} lines)\n",
        diff.session_a_id, diff.line_count_a, diff.session_b_id, diff.line_count_b
    );
    if diff.metadata_changes.is_empty() {
        out.push_str("\n## Metadata\nNo metadata differences.\n");
    } else {
        out.push_str("\n## Metadata changes\n");
        for change in &diff.metadata_changes {
            out.push_str(&format!("- {change}\n"));
        }
    }
    out.push_str("\n## Lines only in A\n");
    if diff.lines_only_in_a.is_empty() {
        out.push_str("(none within cap)\n");
    } else {
        for line in &diff.lines_only_in_a {
            out.push_str(&format!("- {}\n", truncate_for_display(line, 160)));
        }
    }
    out.push_str("\n## Lines only in B\n");
    if diff.lines_only_in_b.is_empty() {
        out.push_str("(none within cap)\n");
    } else {
        for line in &diff.lines_only_in_b {
            out.push_str(&format!("- {}\n", truncate_for_display(line, 160)));
        }
    }
    out
}

pub fn build_repo_timeline(sessions: &[SessionRecord], repo_prefix: &str, limit: usize) -> Vec<TimelineDay> {
    let prefix_lower = repo_prefix.to_ascii_lowercase();
    let mut matching: Vec<&SessionRecord> = sessions
        .iter()
        .filter(|s| session_matches_repo_prefix(s, &prefix_lower))
        .collect();

    matching.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    matching.truncate(limit);

    let mut buckets: Vec<TimelineDay> = Vec::new();
    for session in matching {
        let date = session
            .updated_at
            .or(session.created_at)
            .map(|dt| dt.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "unknown".to_string());

        let entry = TimelineEntry {
            session_id: session.id.clone(),
            provider: session.provider.to_string(),
            title: session
                .title
                .as_deref()
                .map(|t| truncate_for_display(t, 100))
                .unwrap_or_else(|| "(untitled)".to_string()),
            updated_at: session
                .updated_at
                .map(format_timestamp)
                .unwrap_or_else(|| "-".to_string()),
            cwd: session.cwd.as_deref().unwrap_or("-").to_string(),
        };

        if let Some(bucket) = buckets.iter_mut().find(|b| b.date == date) {
            bucket.sessions.push(entry);
        } else {
            buckets.push(TimelineDay {
                date,
                sessions: vec![entry],
            });
        }
    }
    buckets
}

pub fn format_timeline(repo_prefix: &str, days: &[TimelineDay]) -> String {
    if days.is_empty() {
        return format!("No sessions found for repo prefix '{repo_prefix}'.");
    }
    let mut out = format!("# Timeline for repo: {repo_prefix}\n\n");
    for day in days {
        out.push_str(&format!("## {}\n", day.date));
        for session in &day.sessions {
            out.push_str(&format!(
                "- **{}** [{}] {} | {} | ID: {}\n",
                session.title,
                session.provider,
                session.updated_at,
                session.cwd,
                session.session_id,
            ));
        }
        out.push('\n');
    }
    out
}

fn session_matches_repo_prefix(session: &SessionRecord, prefix_lower: &str) -> bool {
    session
        .repo_root
        .as_deref()
        .is_some_and(|r| r.to_ascii_lowercase().starts_with(prefix_lower))
        || session
            .cwd
            .as_deref()
            .is_some_and(|c| c.to_ascii_lowercase().starts_with(prefix_lower))
}

fn format_timestamp(dt: DateTime<Utc>) -> String {
    dt.format("%Y-%m-%d %H:%M UTC").to_string()
}

fn normalize_transcript_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(compact_whitespace)
        .filter(|line| !line.is_empty())
        .collect()
}

fn looks_like_tool_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("tool_use")
        || lower.contains("tool_result")
        || lower.contains("readfile")
        || lower.contains("bash")
        || lower.contains("applypatch")
}

fn extract_user_intents(transcript: &str) -> Vec<String> {
    let lines: Vec<&str> = transcript.lines().collect();
    let mut intents = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index].trim();
        if line.ends_with(" user") && line.starts_with('[') {
            if let Some(body) = lines.get(index + 1) {
                let compact = compact_whitespace(body);
                if !compact.is_empty() {
                    intents.push(truncate_for_display(&compact, 200));
                }
            }
        }
        index += 1;
    }
    intents
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Provider;
    use chrono::Utc;

    fn mk_session(id: &str, transcript: &str, cwd: Option<&str>) -> SessionWithTranscript {
        SessionWithTranscript {
            session: SessionRecord {
                id: id.to_string(),
                provider: Provider::Claude,
                provider_session_id: id.to_string(),
                title: Some("test title".to_string()),
                summary: None,
                cwd: cwd.map(str::to_string),
                repo_root: cwd.map(str::to_string),
                created_at: Some(Utc::now()),
                updated_at: Some(Utc::now()),
                last_message_at: None,
                preview_text: String::new(),
                source_path: "/tmp/x.jsonl".to_string(),
                message_count: Some(2),
                parse_version: "test".to_string(),
                raw_metadata_json: None,
                parse_warning: None,
                discovery_source: "test".to_string(),
            },
            transcript_text: transcript.to_string(),
        }
    }

    #[test]
    fn summarizes_user_intents() {
        let transcript = "[2026-01-01 00:00:00 UTC] user\nfix auth bug\n\n[2026-01-01 00:01:00 UTC] assistant\nok\n\n[2026-01-01 00:02:00 UTC] user\nadd tests";
        let summary = summarize_session(&mk_session("claude:a", transcript, None), 100);
        assert_eq!(
            summary.first_user_intent.as_deref(),
            Some("fix auth bug")
        );
        assert_eq!(summary.last_user_intent.as_deref(), Some("add tests"));
    }

    #[test]
    fn diffs_unique_lines() {
        let a = mk_session("claude:a", "line one\nline two", None);
        let b = mk_session("claude:b", "line one\nline three", None);
        let diff = diff_sessions(&a, &b, 10);
        assert!(diff.lines_only_in_a.iter().any(|l| l.contains("line two")));
        assert!(diff.lines_only_in_b.iter().any(|l| l.contains("line three")));
    }

    #[test]
    fn timeline_groups_by_day() {
        let now = Utc::now();
        let s1 = SessionRecord {
            id: "claude:1".to_string(),
            provider: Provider::Claude,
            provider_session_id: "1".to_string(),
            title: Some("first".to_string()),
            summary: None,
            cwd: Some("/Users/me/projects/foo".to_string()),
            repo_root: Some("/Users/me/projects/foo".to_string()),
            created_at: Some(now),
            updated_at: Some(now),
            last_message_at: None,
            preview_text: String::new(),
            source_path: String::new(),
            message_count: None,
            parse_version: String::new(),
            raw_metadata_json: None,
            parse_warning: None,
            discovery_source: String::new(),
        };
        let days = build_repo_timeline(&[s1], "/users/me/projects/foo", 10);
        assert_eq!(days.len(), 1);
        assert_eq!(days[0].sessions.len(), 1);
        assert_eq!(days[0].date, now.format("%Y-%m-%d").to_string());
    }
}
