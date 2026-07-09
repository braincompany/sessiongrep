//! Repo-scoped session timelines for MCP — metadata only, day-bucketed in UTC.

use chrono::{DateTime, Utc};

use crate::models::SessionRecord;
use crate::util::truncate_for_display;

pub const DEFAULT_LIMIT: usize = 30;
pub const MAX_LIMIT: usize = 200;
const TITLE_DISPLAY_LEN: usize = 100;
const CWD_DISPLAY_LEN: usize = 80;

#[derive(Debug, Clone)]
pub struct TimelineDay {
    pub date: String,
    pub sessions: Vec<SessionRecord>,
}

/// Clamp user-provided limit to the allowed range.
pub fn clamp_limit(limit: usize) -> usize {
    limit.clamp(1, MAX_LIMIT)
}

/// Whether `cwd` or `repo_root` starts with `repo_prefix` (ASCII case-insensitive).
pub fn matches_repo_prefix(session: &SessionRecord, repo_prefix: &str) -> bool {
    let prefix = repo_prefix.to_ascii_lowercase();
    if prefix.is_empty() {
        return false;
    }
    session
        .cwd
        .as_deref()
        .is_some_and(|path| path.to_ascii_lowercase().starts_with(&prefix))
        || session
            .repo_root
            .as_deref()
            .is_some_and(|path| path.to_ascii_lowercase().starts_with(&prefix))
}

/// UTC calendar day for bucketing, or `unknown` when no timestamp exists.
pub fn session_day_bucket(session: &SessionRecord) -> String {
    session_timestamp(session)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn session_timestamp(session: &SessionRecord) -> Option<DateTime<Utc>> {
    session.updated_at.or(session.created_at)
}

/// Sort key: newest first; tie-break `session_id` ascending for determinism when reversed.
pub fn sort_sessions_for_timeline(sessions: &mut [SessionRecord]) {
    sessions.sort_by(|a, b| {
        let tb = session_timestamp(b);
        let ta = session_timestamp(a);
        tb.cmp(&ta).then_with(|| a.id.cmp(&b.id))
    });
}

/// Bucket sorted sessions by day. `unknown` days are collected and appended last.
pub fn bucket_sessions_by_day(sessions: &[SessionRecord]) -> Vec<TimelineDay> {
    let mut known: Vec<TimelineDay> = Vec::new();
    let mut unknown: Vec<SessionRecord> = Vec::new();

    for session in sessions {
        let day = session_day_bucket(session);
        if day == "unknown" {
            unknown.push(session.clone());
            continue;
        }
        if let Some(bucket) = known.iter_mut().find(|b| b.date == day) {
            bucket.sessions.push(session.clone());
        } else {
            known.push(TimelineDay {
                date: day,
                sessions: vec![session.clone()],
            });
        }
    }

    known.sort_by(|a, b| b.date.cmp(&a.date));
    for bucket in &mut known {
        sort_sessions_for_timeline(&mut bucket.sessions);
    }
    if !unknown.is_empty() {
        sort_sessions_for_timeline(&mut unknown);
        known.push(TimelineDay {
            date: "unknown".to_string(),
            sessions: unknown,
        });
    }
    known
}

/// Build markdown timeline from metadata records (already prefix-filtered).
pub fn format_timeline_markdown(repo_prefix: &str, days: &[TimelineDay]) -> String {
    if days.is_empty() {
        return format!("No sessions found for prefix '{repo_prefix}'.");
    }
    let mut out = format!("# Timeline for repo: {repo_prefix}\n\n");
    for day in days {
        out.push_str(&format!("## {}\n", day.date));
        for session in &day.sessions {
            let title = session
                .title
                .as_deref()
                .map(|t| truncate_for_display(t, TITLE_DISPLAY_LEN))
                .unwrap_or_else(|| "(untitled)".to_string());
            let cwd = session
                .cwd
                .as_deref()
                .map(|c| truncate_for_display(c, CWD_DISPLAY_LEN))
                .unwrap_or_else(|| "-".to_string());
            let updated = session_timestamp(session)
                .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
                .unwrap_or_else(|| "-".to_string());
            out.push_str(&format!(
                "- **{title}** [{provider}] {updated} | CWD: {cwd} | ID: {id}\n",
                provider = session.provider,
                id = session.id,
            ));
        }
        out.push('\n');
    }
    out
}

/// Filter, sort, cap, and format a repo-scoped timeline.
pub fn build_repo_timeline(
    sessions: Vec<SessionRecord>,
    repo_prefix: &str,
    limit: usize,
) -> String {
    let limit = clamp_limit(limit);
    let mut matched: Vec<SessionRecord> = sessions
        .into_iter()
        .filter(|s| matches_repo_prefix(s, repo_prefix))
        .collect();
    sort_sessions_for_timeline(&mut matched);
    matched.truncate(limit);
    let days = bucket_sessions_by_day(&matched);
    format_timeline_markdown(repo_prefix, &days)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Provider, SessionRecord};

    fn record(
        id: &str,
        cwd: Option<&str>,
        repo_root: Option<&str>,
        updated: Option<DateTime<Utc>>,
        created: Option<DateTime<Utc>>,
    ) -> SessionRecord {
        SessionRecord {
            id: id.to_string(),
            provider: Provider::Claude,
            provider_session_id: id.to_string(),
            title: Some(format!("title-{id}")),
            summary: None,
            cwd: cwd.map(str::to_string),
            repo_root: repo_root.map(str::to_string),
            created_at: created,
            updated_at: updated,
            last_message_at: updated,
            preview_text: String::new(),
            source_path: String::new(),
            message_count: None,
            parse_version: String::new(),
            raw_metadata_json: None,
            parse_warning: None,
            discovery_source: String::new(),
        }
    }

    #[test]
    fn same_day_bucketing() {
        let day = Utc::now();
        let sessions = vec![
            record("b", Some("/Users/me/proj"), None, Some(day), None),
            record("a", Some("/Users/me/proj"), None, Some(day), None),
        ];
        let days = bucket_sessions_by_day(&sessions);
        assert_eq!(days.len(), 1);
        assert_eq!(days[0].sessions.len(), 2);
        assert_eq!(days[0].sessions[0].id, "a");
        assert_eq!(days[0].sessions[1].id, "b");
    }

    #[test]
    fn multi_day_ordering_newest_first() {
        let older = DateTime::parse_from_rfc3339("2026-07-05T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let newer = DateTime::parse_from_rfc3339("2026-07-07T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let sessions = vec![
            record("old", Some("/repo"), None, Some(older), None),
            record("new", Some("/repo"), None, Some(newer), None),
        ];
        let days = bucket_sessions_by_day(&sessions);
        assert_eq!(days.len(), 2);
        assert_eq!(days[0].date, "2026-07-07");
        assert_eq!(days[1].date, "2026-07-05");
    }

    #[test]
    fn prefix_mismatch_excluded() {
        let session = record("x", Some("/other/path"), None, Some(Utc::now()), None);
        assert!(!matches_repo_prefix(&session, "/Users/me/proj"));
    }

    #[test]
    fn case_insensitive_prefix_matching() {
        let session = record(
            "x",
            Some("/Users/Me/Projects/Foo"),
            None,
            Some(Utc::now()),
            None,
        );
        assert!(matches_repo_prefix(&session, "/users/me/projects"));
        assert!(matches_repo_prefix(&session, "/USERS/ME/PROJECTS/FOO"));
    }

    #[test]
    fn limit_cap_behavior() {
        assert_eq!(clamp_limit(0), 1);
        assert_eq!(clamp_limit(30), 30);
        assert_eq!(clamp_limit(999), MAX_LIMIT);
    }

    #[test]
    fn empty_result_message() {
        let out = build_repo_timeline(vec![], "/missing", 30);
        assert_eq!(out, "No sessions found for prefix '/missing'.");
    }

    #[test]
    fn unknown_day_bucket_last() {
        let dated = record("dated", Some("/repo"), None, Some(Utc::now()), None);
        let unknown = record("unknown", Some("/repo"), None, None, None);
        let days = bucket_sessions_by_day(&[unknown, dated]);
        assert_eq!(days.last().unwrap().date, "unknown");
    }

    #[test]
    fn total_session_cap_not_per_day() {
        let day1 = DateTime::parse_from_rfc3339("2026-07-07T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let day2 = DateTime::parse_from_rfc3339("2026-07-06T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let sessions = vec![
            record("a", Some("/repo"), None, Some(day1), None),
            record("b", Some("/repo"), None, Some(day1), None),
            record("c", Some("/repo"), None, Some(day2), None),
        ];
        let out = build_repo_timeline(sessions, "/repo", 2);
        assert!(out.contains("2026-07-07"));
        let line_count = out.lines().filter(|l| l.starts_with("- **")).count();
        assert_eq!(line_count, 2);
    }
}
