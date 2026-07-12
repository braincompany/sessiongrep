use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{TimeZone, Utc};
use rusqlite::{Connection, OpenFlags};
use serde_json::json;

use crate::models::{ParsedSession, Provider, SessionRecord};
use crate::util::{
    find_repo_root, format_transcript_line, normalize_path, preview_from_text, truncate_for_display,
};

const MAX_THREAD_BYTES: usize = 10 * 1024 * 1024;
const RETENTION_ROWS: i64 = 1_000;

#[derive(Debug, Clone, Default)]
pub struct LogsHealth {
    pub status: String,
    pub recoverable: usize,
    pub retention_limited: usize,
    pub parse_failures: usize,
    pub content_unavailable: usize,
    pub max_row_id: i64,
}

#[derive(Default)]
struct Thread {
    users: Vec<(i64, String)>,
    assistants: HashMap<String, (i64, String)>,
    cwd: Option<String>,
    rows: i64,
    bytes: usize,
    malformed: usize,
}

pub struct CodexLogsAdapter {
    path: PathBuf,
}

impl CodexLogsAdapter {
    pub fn new(codex_home: &Path) -> Self {
        Self {
            path: codex_home.join("logs_2.sqlite"),
        }
    }

    pub fn source_path(&self) -> String {
        normalize_path(&self.path)
    }

    pub fn max_row_id(&self) -> Result<Option<i64>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let conn = Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.busy_timeout(Duration::from_millis(1_000))?;
        conn.pragma_update(None, "query_only", true)?;
        validate_schema(&conn)?;
        Ok(Some(conn.query_row(
            "select coalesce(max(id), 0) from logs",
            [],
            |r| r.get(0),
        )?))
    }

    pub fn recover(
        &self,
        durable_ids: &HashSet<String>,
    ) -> Result<(Vec<ParsedSession>, LogsHealth)> {
        if !self.path.exists() {
            return Ok((
                Vec::new(),
                LogsHealth {
                    status: "missing (optional)".into(),
                    ..Default::default()
                },
            ));
        }
        let conn = Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("open optional Codex log DB {}", self.path.display()))?;
        conn.busy_timeout(Duration::from_millis(1_000))?;
        conn.pragma_update(None, "query_only", true)?;
        validate_schema(&conn)?;
        let max_row_id =
            conn.query_row("select coalesce(max(id), 0) from logs", [], |r| r.get(0))?;
        let mut stmt = conn.prepare(
            "select id, ts, level, target, thread_id, feedback_log_body, estimated_bytes
             from logs where thread_id is not null order by id",
        )?;
        let mut rows = stmt.query([])?;
        let mut threads: HashMap<String, Thread> = HashMap::new();
        let mut project_ids = HashSet::new();
        let mut parse_failures = 0usize;
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let ts: i64 = row.get(1)?;
            let level: String = row.get(2)?;
            let target: String = row.get(3)?;
            let thread_id: String = row.get(4)?;
            let body: Option<String> = row.get(5)?;
            let estimated: i64 = row.get(6)?;
            let Some(body) = body else { continue };
            let thread = threads.entry(thread_id.clone()).or_default();
            thread.rows += 1;
            thread.bytes = thread
                .bytes
                .saturating_add(estimated.max(body.len() as i64) as usize);
            if target == "codex_core::session::handlers"
                && body.contains(": Submission sub=Submission {")
                && body.contains("op: UserInput {")
                && body.contains(
                    "responsesapi_client_metadata: Some({\"workspace_kind\": \"project\"})",
                )
            {
                project_ids.insert(thread_id.clone());
                if thread.cwd.is_none() {
                    thread.cwd = extract_cwd(&body);
                }
                match extract_debug_string(&body, "items: [Text { text: \"") {
                    Some(text) if !text.trim().is_empty() => thread.users.push((ts, text)),
                    _ => {
                        thread.malformed += 1;
                        parse_failures += 1;
                    }
                }
            }
            if level == "DEBUG" && target == "codex_core::stream_events_utils" {
                if let Some(boundary) = body.find(": Output item item=Message {") {
                    let outer = &body[boundary..];
                    let item_id = extract_debug_string(outer, "id: Some(\"");
                    let text = extract_debug_string(outer, "content: [OutputText { text: \"");
                    if let (Some(item_id), Some(text)) = (item_id.as_ref(), text) {
                        if !text.trim().is_empty() {
                            thread.assistants.insert(item_id.clone(), (ts, text));
                        }
                    } else if item_id.is_some() {
                        thread.malformed += 1;
                        parse_failures += 1;
                    }
                }
            }
            let _ = id;
        }

        let mut recovered = Vec::new();
        let mut health = LogsHealth {
            status: "ok (read-only, WAL-aware)".into(),
            parse_failures,
            max_row_id,
            ..Default::default()
        };
        for id in project_ids {
            if durable_ids.contains(&id) {
                continue;
            }
            let Some(mut thread) = threads.remove(&id) else {
                continue;
            };
            thread.users.sort_by_key(|v| v.0);
            let mut assistants: Vec<_> = thread.assistants.into_values().collect();
            assistants.sort_by_key(|v| v.0);
            let mut messages: Vec<(i64, &str, String)> = thread
                .users
                .into_iter()
                .map(|(t, s)| (t, "user", s))
                .collect();
            messages.extend(assistants.into_iter().map(|(t, s)| (t, "assistant", s)));
            messages.sort_by_key(|v| v.0);
            let mut used = 0usize;
            let mut transcript = Vec::new();
            for (ts, role, text) in messages {
                if used.saturating_add(text.len()) > MAX_THREAD_BYTES {
                    break;
                }
                used += text.len();
                transcript.push(format_transcript_line(
                    role,
                    Utc.timestamp_opt(ts, 0).single(),
                    &text,
                ));
            }
            let first_user = transcript
                .iter()
                .find_map(|line| line.split_once("\n").map(|v| v.1.to_string()));
            let content_unavailable = transcript.is_empty();
            if content_unavailable {
                health.content_unavailable += 1;
            }
            if thread.rows >= RETENTION_ROWS {
                health.retention_limited += 1;
            }
            let warnings = [
                Some("lossy diagnostic source; this session cannot be resumed".to_string()),
                (thread.rows >= RETENTION_ROWS).then(|| {
                    "Codex log retention ceiling reached; early content may be missing".to_string()
                }),
                (thread.bytes > MAX_THREAD_BYTES)
                    .then(|| "recovered content exceeded the 10 MiB per-thread budget".to_string()),
                content_unavailable.then(|| {
                    "assistant/user content unavailable (possibly summarized logging)".to_string()
                }),
                (thread.malformed > 0)
                    .then(|| format!("{} malformed log event(s)", thread.malformed)),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("; ");
            let cwd = thread.cwd;
            let updated = transcript
                .last()
                .and_then(|line| line.get(1..20))
                .and_then(|s| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").ok())
                .map(|d| d.and_utc());
            let synthetic = format!("{}#thread={id}", normalize_path(&self.path));
            let parsed = ParsedSession {
                session: SessionRecord {
                    id: format!("codex:{id}"), provider: Provider::Codex, provider_session_id: id,
                    title: first_user.as_deref().map(|s| truncate_for_display(s, 100)), summary: first_user.as_deref().map(|s| truncate_for_display(s, 180)),
                    cwd: cwd.clone(), repo_root: cwd.as_deref().and_then(find_repo_root), created_at: None,
                    updated_at: updated, last_message_at: updated,
                    preview_text: first_user.as_deref().map(preview_from_text).unwrap_or_else(|| "(content unavailable)".into()),
                    source_path: synthetic, message_count: Some(transcript.len() as i64), parse_version: "codex-logs-v1".into(),
                    raw_metadata_json: Some(json!({"row_count": thread.rows, "estimated_bytes": thread.bytes, "max_row_id": max_row_id, "parser_format": "rust-debug-v1", "assistant_content_available": transcript.iter().any(|s| s.contains("] assistant\n"))}).to_string()),
                    parse_warning: Some(warnings), discovery_source: "logs-sqlite".into(),
                }, transcript_text: transcript.join("\n\n")
            };
            recovered.push(parsed);
        }
        health.recoverable = recovered.len();
        Ok((recovered, health))
    }
}

fn validate_schema(conn: &Connection) -> Result<()> {
    let count: i64 = conn.query_row("select count(*) from pragma_table_info('logs') where name in ('id','ts','level','target','thread_id','feedback_log_body','estimated_bytes')", [], |r| r.get(0))?;
    if count != 7 {
        bail!("incompatible optional Codex log DB schema");
    }
    Ok(())
}

fn extract_cwd(body: &str) -> Option<String> {
    extract_debug_string(body, "legacy_fallback_cwd: AbsolutePathBuf(\"")
}

fn extract_debug_string(input: &str, marker: &str) -> Option<String> {
    let mut chars = input.get(input.find(marker)? + marker.len()..)?.chars();
    let mut out = String::new();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                '\\' => out.push('\\'),
                '"' => out.push('"'),
                '0' => out.push('\0'),
                'x' => {
                    let h = format!("{}{}", chars.next()?, chars.next()?);
                    out.push(char::from(u8::from_str_radix(&h, 16).ok()?));
                }
                'u' => {
                    if chars.next()? != '{' {
                        return None;
                    }
                    let mut h = String::new();
                    loop {
                        let c = chars.next()?;
                        if c == '}' {
                            break;
                        }
                        h.push(c);
                        if h.len() > 6 {
                            return None;
                        }
                    }
                    out.push(char::from_u32(u32::from_str_radix(&h, 16).ok()?)?);
                }
                other => out.push(other),
            },
            other => out.push(other),
        }
        if out.len() > MAX_THREAD_BYTES {
            return None;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    #[test]
    fn decodes_rust_debug_escapes() {
        assert_eq!(
            extract_debug_string("x text: \"a\\n\\x42\\u{1f642}\\\"c\" z", "text: \"").unwrap(),
            "a\nB🙂\"c"
        );
    }
    #[test]
    fn requires_terminated_string() {
        assert!(extract_debug_string("text: \"oops", "text: \"").is_none());
    }

    #[test]
    fn recovers_only_exact_project_events_and_deduplicates_assistant_items() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("logs_2.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("create table logs(id integer primary key, ts integer not null, level text not null, target text not null, feedback_log_body text, thread_id text, estimated_bytes integer not null default 0);").unwrap();
        let project = r#"session_loop{thread_id=p}: Submission sub=Submission { id: "s", op: UserInput { items: [Text { text: "hello\n\u{1f642}", text_elements: [] }], responsesapi_client_metadata: Some({"workspace_kind": "project"}), thread_settings: ThreadSettingsOverrides { environments: Some(TurnEnvironmentSelections { legacy_fallback_cwd: AbsolutePathBuf("/tmp/work") }) } } }"#;
        let internal = r#"session_loop{thread_id=i}: Submission sub=Submission { op: UserInput { items: [Text { text: "secret", text_elements: [] }], responsesapi_client_metadata: Some({"workspace_kind": "projectless"}) } }"#;
        let assistant_empty = r#"prefix: Output item item=Message { id: Some("m1"), role: "assistant", content: [OutputText { text: "" }] }"#;
        let assistant_done = r#"prefix: Output item item=Message { id: Some("m1"), role: "assistant", content: [OutputText { text: "world\x21" }] }"#;
        for (id, ts, level, target, thread, body) in [
            (
                1,
                1_700_000_000,
                "INFO",
                "codex_core::session::handlers",
                "p",
                project,
            ),
            (
                2,
                1_700_000_001,
                "INFO",
                "codex_core::session::handlers",
                "i",
                internal,
            ),
            (
                3,
                1_700_000_002,
                "DEBUG",
                "codex_core::stream_events_utils",
                "p",
                assistant_empty,
            ),
            (
                4,
                1_700_000_003,
                "DEBUG",
                "codex_core::stream_events_utils",
                "p",
                assistant_done,
            ),
        ] {
            conn.execute("insert into logs(id,ts,level,target,thread_id,feedback_log_body,estimated_bytes) values(?1,?2,?3,?4,?5,?6,100)", rusqlite::params![id,ts,level,target,thread,body]).unwrap();
        }
        drop(conn);
        let (sessions, health) = CodexLogsAdapter::new(dir.path())
            .recover(&HashSet::new())
            .unwrap();
        assert_eq!(health.recoverable, 1);
        assert_eq!(sessions[0].session.id, "codex:p");
        assert_eq!(sessions[0].session.cwd.as_deref(), Some("/tmp/work"));
        assert!(sessions[0].transcript_text.contains("hello\n🙂"));
        assert!(sessions[0].transcript_text.contains("world!"));
        assert!(!sessions[0].transcript_text.contains("secret"));
        assert_eq!(sessions[0].session.message_count, Some(2));

        let durable = HashSet::from(["p".to_string()]);
        assert!(CodexLogsAdapter::new(dir.path())
            .recover(&durable)
            .unwrap()
            .0
            .is_empty());
    }
}
