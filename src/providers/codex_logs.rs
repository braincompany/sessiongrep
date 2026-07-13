use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{TimeZone, Utc};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};

use crate::models::{ParsedSession, Provider, SessionRecord};
use crate::util::{
    find_repo_root, format_transcript_line, normalize_path, preview_from_text, truncate_for_display,
};

pub const DISCOVERY_SOURCE: &str = "logs-sqlite";
const CHECKPOINT_VERSION: &str = "codex-logs-v2";
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

pub struct LogsRecovery {
    pub sessions: Vec<ParsedSession>,
    pub affected_ids: Vec<String>,
    pub replace_all: bool,
    pub checkpoint: Option<String>,
    pub unchanged: bool,
    pub health: LogsHealth,
}

#[derive(Debug)]
struct UserEvent {
    text: Option<String>,
    cwd: Option<String>,
}

enum UserParse {
    NotUser,
    NonProject,
    Project(UserEvent),
    Malformed,
}

#[derive(Debug)]
struct AssistantEvent {
    item_id: String,
    text: Option<String>,
    completed: bool,
}

enum AssistantParse {
    NotMessage,
    Message(AssistantEvent),
    Malformed,
}

#[derive(Debug, Clone)]
struct Message {
    row_id: i64,
    timestamp: i64,
    role: &'static str,
    text: String,
    completed: bool,
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

    pub fn recover(
        &self,
        durable_ids: &HashSet<String>,
        previous_checkpoint: Option<&str>,
    ) -> Result<LogsRecovery> {
        if !self.path.exists() {
            return Ok(LogsRecovery {
                sessions: Vec::new(),
                affected_ids: Vec::new(),
                replace_all: false,
                checkpoint: None,
                unchanged: true,
                health: LogsHealth {
                    status: "missing (optional)".into(),
                    ..Default::default()
                },
            });
        }

        let conn = open_read_only(&self.path)?;
        validate_schema(&conn)?;
        let (min_row_id, max_row_id): (i64, i64) = conn.query_row(
            "select coalesce(min(id), 0), coalesce(max(id), 0) from logs",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let checkpoint = format_checkpoint(min_row_id, max_row_id);
        let previous = previous_checkpoint.and_then(parse_checkpoint);

        if previous == Some((min_row_id, max_row_id)) {
            return Ok(LogsRecovery {
                sessions: Vec::new(),
                affected_ids: Vec::new(),
                replace_all: false,
                checkpoint: Some(checkpoint),
                unchanged: true,
                health: LogsHealth {
                    status: "unchanged (read-only, WAL-aware)".into(),
                    max_row_id,
                    ..Default::default()
                },
            });
        }

        let tail_after = previous.and_then(|(old_min, old_max)| {
            (old_min == min_row_id && old_max < max_row_id).then_some(old_max)
        });
        let replace_all = tail_after.is_none();
        let affected_ids = if let Some(after) = tail_after {
            thread_ids_after(&conn, after)?
        } else {
            Vec::new()
        };

        let candidate_ids = if replace_all {
            discover_project_ids(&conn, None)?
        } else {
            discover_project_ids(&conn, Some(&affected_ids))?
        };
        let mut parse_failures = candidate_ids.parse_failures;
        let mut sessions = Vec::new();
        let mut health = LogsHealth {
            status: if replace_all {
                "ok (full, read-only, WAL-aware)".into()
            } else {
                "ok (tail, read-only, WAL-aware)".into()
            },
            max_row_id,
            ..Default::default()
        };

        for id in candidate_ids.ids {
            if durable_ids.contains(&id) {
                continue;
            }
            let (session, thread_health) = self.recover_thread(&conn, &id, max_row_id)?;
            parse_failures += thread_health.parse_failures;
            health.retention_limited += usize::from(thread_health.retention_limited);
            health.content_unavailable += usize::from(thread_health.content_unavailable);
            sessions.push(session);
        }

        health.recoverable = sessions.len();
        health.parse_failures = parse_failures;
        let affected_ids = if replace_all {
            Vec::new()
        } else {
            affected_ids
        };

        Ok(LogsRecovery {
            sessions,
            affected_ids,
            replace_all,
            checkpoint: Some(checkpoint),
            unchanged: false,
            health,
        })
    }

    fn recover_thread(
        &self,
        conn: &Connection,
        thread_id: &str,
        max_row_id: i64,
    ) -> Result<(ParsedSession, ThreadHealth)> {
        let mut stmt = conn.prepare(
            "select id, ts, level, target, feedback_log_body, estimated_bytes
             from logs where thread_id = ?1 order by id",
        )?;
        let mut rows = stmt.query([thread_id])?;
        let mut users = Vec::new();
        let mut assistants: HashMap<String, Message> = HashMap::new();
        let mut cwd = None;
        let mut row_count = 0i64;
        let mut estimated_bytes = 0usize;
        let mut parse_failures = 0usize;

        while let Some(row) = rows.next()? {
            let row_id: i64 = row.get(0)?;
            let timestamp: i64 = row.get(1)?;
            let level: String = row.get(2)?;
            let target: String = row.get(3)?;
            let body: Option<String> = row.get(4)?;
            let estimated: i64 = row.get(5)?;
            row_count += 1;
            let Some(body) = body else { continue };
            estimated_bytes =
                estimated_bytes.saturating_add(estimated.max(body.len() as i64).max(0) as usize);

            if level == "DEBUG" && target == "codex_core::session::handlers" {
                match parse_user_event(&body, thread_id) {
                    UserParse::Project(event) => {
                        cwd = cwd.or(event.cwd);
                        if let Some(text) = event.text.filter(|text| !text.trim().is_empty()) {
                            users.push(Message {
                                row_id,
                                timestamp,
                                role: "user",
                                text,
                                completed: true,
                            });
                        }
                    }
                    UserParse::Malformed => parse_failures += 1,
                    UserParse::NotUser | UserParse::NonProject => {}
                }
            }

            if level == "DEBUG" && target == "codex_core::stream_events_utils" {
                match parse_assistant_event(&body) {
                    AssistantParse::Message(event) => {
                        let Some(text) = event.text.filter(|text| !text.trim().is_empty()) else {
                            continue;
                        };
                        let next = Message {
                            row_id,
                            timestamp,
                            role: "assistant",
                            text,
                            completed: event.completed,
                        };
                        assistants
                            .entry(event.item_id)
                            .and_modify(|current| {
                                if message_is_better(&next, current) {
                                    *current = next.clone();
                                }
                            })
                            .or_insert(next);
                    }
                    AssistantParse::Malformed => parse_failures += 1,
                    AssistantParse::NotMessage => {}
                }
            }
        }

        users.sort_by_key(|message| (message.timestamp, message.row_id));
        let first_user = users.first().map(|message| message.text.clone());
        let assistant_content_available = !assistants.is_empty();
        let mut messages = users;
        messages.extend(assistants.into_values());
        messages.sort_by_key(|message| (message.timestamp, message.row_id));

        let created_at = messages
            .first()
            .and_then(|message| Utc.timestamp_opt(message.timestamp, 0).single());
        let updated_at = messages
            .last()
            .and_then(|message| Utc.timestamp_opt(message.timestamp, 0).single());
        let mut recovered_bytes = 0usize;
        let mut capped = false;
        let mut transcript = Vec::new();
        for message in messages {
            if recovered_bytes.saturating_add(message.text.len()) > MAX_THREAD_BYTES {
                capped = true;
                continue;
            }
            recovered_bytes += message.text.len();
            transcript.push(format_transcript_line(
                message.role,
                Utc.timestamp_opt(message.timestamp, 0).single(),
                &message.text,
            ));
        }

        let retention_limited = row_count >= RETENTION_ROWS;
        let content_unavailable = !assistant_content_available;
        let warnings = [
            Some("lossy diagnostic source; this session cannot be resumed".to_string()),
            retention_limited.then(|| {
                "Codex log retention ceiling reached; early content may be missing".to_string()
            }),
            capped.then(|| "recovered content exceeded the 10 MiB per-thread budget".to_string()),
            content_unavailable
                .then(|| "assistant content unavailable (possibly summarized logging)".to_string()),
            (parse_failures > 0).then(|| format!("{parse_failures} malformed log event(s)")),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("; ");
        let synthetic = format!("{}#thread={thread_id}", normalize_path(&self.path));
        let preview = first_user
            .as_deref()
            .map(preview_from_text)
            .unwrap_or_else(|| "(content unavailable)".into());
        let session = ParsedSession {
            session: SessionRecord {
                id: format!("codex:{thread_id}"),
                provider: Provider::Codex,
                provider_session_id: thread_id.to_string(),
                title: first_user
                    .as_deref()
                    .map(|text| truncate_for_display(text, 100)),
                summary: first_user
                    .as_deref()
                    .map(|text| truncate_for_display(text, 180)),
                cwd: cwd.clone(),
                repo_root: cwd.as_deref().and_then(find_repo_root),
                created_at,
                updated_at,
                last_message_at: updated_at,
                preview_text: preview,
                source_path: synthetic,
                message_count: Some(transcript.len() as i64),
                parse_version: CHECKPOINT_VERSION.into(),
                raw_metadata_json: Some(
                    json!({
                        "row_count": row_count,
                        "estimated_bytes": estimated_bytes,
                        "recovered_bytes": recovered_bytes,
                        "max_row_id": max_row_id,
                        "parser_format": "rust-debug-v2",
                        "assistant_content_available": assistant_content_available,
                    })
                    .to_string(),
                ),
                parse_warning: Some(warnings),
                discovery_source: DISCOVERY_SOURCE.into(),
            },
            transcript_text: transcript.join("\n\n"),
        };
        Ok((
            session,
            ThreadHealth {
                retention_limited,
                content_unavailable,
                parse_failures,
            },
        ))
    }
}

#[derive(Default)]
struct ThreadHealth {
    retention_limited: bool,
    content_unavailable: bool,
    parse_failures: usize,
}

struct ProjectIds {
    ids: Vec<String>,
    parse_failures: usize,
}

fn open_read_only(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("open optional Codex log DB {}", path.display()))?;
    conn.busy_timeout(Duration::from_millis(1_000))?;
    conn.pragma_update(None, "query_only", true)?;
    Ok(conn)
}

fn validate_schema(conn: &Connection) -> Result<()> {
    let count: i64 = conn.query_row(
        "select count(*) from pragma_table_info('logs')
         where name in ('id','ts','level','target','thread_id','feedback_log_body','estimated_bytes')",
        [],
        |row| row.get(0),
    )?;
    if count != 7 {
        bail!("incompatible optional Codex log DB schema");
    }
    Ok(())
}

fn thread_ids_after(conn: &Connection, after: i64) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "select distinct thread_id from logs
         where id > ?1 and thread_id is not null order by thread_id",
    )?;
    let rows = stmt.query_map([after], |row| row.get(0))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

fn discover_project_ids(conn: &Connection, only_ids: Option<&[String]>) -> Result<ProjectIds> {
    let mut ids = HashSet::new();
    let mut parse_failures = 0usize;
    if let Some(only_ids) = only_ids {
        let mut stmt = conn.prepare(
            "select feedback_log_body from logs
             where thread_id = ?1 and level = 'DEBUG'
               and target = 'codex_core::session::handlers' order by id",
        )?;
        for thread_id in only_ids {
            let rows = stmt.query_map([thread_id], |row| row.get::<_, Option<String>>(0))?;
            for body in rows {
                let Some(body) = body? else { continue };
                match parse_user_event(&body, thread_id) {
                    UserParse::Project(_) => {
                        ids.insert(thread_id.clone());
                    }
                    UserParse::Malformed => parse_failures += 1,
                    UserParse::NotUser | UserParse::NonProject => {}
                }
            }
        }
    } else {
        let mut stmt = conn.prepare(
            "select thread_id, feedback_log_body from logs
             where thread_id is not null and level = 'DEBUG'
               and target = 'codex_core::session::handlers' order by id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?;
        for row in rows {
            let (thread_id, body) = row?;
            let Some(body) = body else { continue };
            match parse_user_event(&body, &thread_id) {
                UserParse::Project(_) => {
                    ids.insert(thread_id);
                }
                UserParse::Malformed => parse_failures += 1,
                UserParse::NotUser | UserParse::NonProject => {}
            }
        }
    }
    let mut ids: Vec<_> = ids.into_iter().collect();
    ids.sort();
    Ok(ProjectIds {
        ids,
        parse_failures,
    })
}

fn parse_user_event(body: &str, thread_id: &str) -> UserParse {
    let prefix = format!("session_loop{{thread_id={thread_id}}}: Submission sub=Submission {{");
    let Some(submission) = braced_body(body, &prefix) else {
        return UserParse::NotUser;
    };
    let fields = split_top_level_fields(submission);
    let Some(op) = field_value(&fields, "op") else {
        return UserParse::Malformed;
    };
    if !op.starts_with("UserInput {") {
        return UserParse::NotUser;
    }
    let Some(input) = braced_body(op, "UserInput {") else {
        return UserParse::Malformed;
    };
    let input_fields = split_top_level_fields(input);
    let Some(metadata) = field_value(&input_fields, "responsesapi_client_metadata") else {
        return UserParse::Malformed;
    };
    let Some(json_text) = metadata
        .strip_prefix("Some(")
        .and_then(|value| value.strip_suffix(')'))
    else {
        return UserParse::NonProject;
    };
    let Ok(metadata): Result<Value, _> = serde_json::from_str(json_text) else {
        return UserParse::Malformed;
    };
    if metadata.get("workspace_kind").and_then(Value::as_str) != Some("project") {
        return UserParse::NonProject;
    }

    let text = field_value(&input_fields, "items").and_then(first_text_item);
    let cwd = field_value(&input_fields, "thread_settings").and_then(|settings| {
        extract_debug_string(
            settings,
            "legacy_fallback_cwd: AbsolutePathBuf(\"",
            MAX_THREAD_BYTES,
        )
    });
    UserParse::Project(UserEvent { text, cwd })
}

fn parse_assistant_event(body: &str) -> AssistantParse {
    let Some((prefix, item)) = body.split_once(": Output item item=") else {
        return AssistantParse::NotMessage;
    };
    if !item.starts_with("Message {") {
        return AssistantParse::NotMessage;
    }
    let Some(message) = braced_body(item, "Message {") else {
        return AssistantParse::Malformed;
    };
    let fields = split_top_level_fields(message);
    if field_value(&fields, "role") != Some("\"assistant\"") {
        return AssistantParse::NotMessage;
    }
    let Some(item_id) =
        field_value(&fields, "id").and_then(|value| extract_debug_string(value, "Some(\"", 256))
    else {
        return AssistantParse::Malformed;
    };
    let text = field_value(&fields, "content").and_then(first_output_text);
    let completed = prefix
        .rsplit(':')
        .next()
        .is_some_and(|segment| segment == "handle_output_item_done");
    AssistantParse::Message(AssistantEvent {
        item_id,
        text,
        completed,
    })
}

fn message_is_better(next: &Message, current: &Message) -> bool {
    (next.completed && !current.completed)
        || (next.completed == current.completed && next.row_id > current.row_id)
}

fn first_text_item(items: &str) -> Option<String> {
    let inner = bracket_body(items, '[', ']')?;
    for item in split_top_level_fields(inner) {
        let item = item.trim();
        let Some(text_item) = braced_body(item, "Text {") else {
            continue;
        };
        let fields = split_top_level_fields(text_item);
        if let Some(text) = field_value(&fields, "text")
            .and_then(|value| extract_debug_string(value, "\"", MAX_THREAD_BYTES))
        {
            return Some(text);
        }
    }
    None
}

fn first_output_text(content: &str) -> Option<String> {
    let inner = bracket_body(content, '[', ']')?;
    for item in split_top_level_fields(inner) {
        let item = item.trim();
        let Some(output) = braced_body(item, "OutputText {") else {
            continue;
        };
        let fields = split_top_level_fields(output);
        if let Some(text) = field_value(&fields, "text")
            .and_then(|value| extract_debug_string(value, "\"", MAX_THREAD_BYTES))
        {
            return Some(text);
        }
    }
    None
}

fn field_value<'a>(fields: &[&'a str], name: &str) -> Option<&'a str> {
    fields.iter().find_map(|field| {
        let (key, value) = field.trim().split_once(':')?;
        (key == name).then(|| value.trim())
    })
}

fn braced_body<'a>(input: &'a str, prefix: &str) -> Option<&'a str> {
    let rest = input.strip_prefix(prefix)?;
    let close = matching_close(rest, '{', '}')?;
    Some(&rest[..close])
}

fn bracket_body(input: &str, open: char, close: char) -> Option<&str> {
    let input = input.trim();
    let rest = input.strip_prefix(open)?;
    let close_index = matching_close(rest, open, close)?;
    Some(&rest[..close_index])
}

fn matching_close(input: &str, open: char, close: char) -> Option<usize> {
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for (index, ch) in input.char_indices() {
        if quoted {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                quoted = false;
            }
            continue;
        }
        match ch {
            '"' => quoted = true,
            value if value == open => depth += 1,
            value if value == close => {
                if depth == 0 {
                    return Some(index);
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    None
}

fn split_top_level_fields(input: &str) -> Vec<&str> {
    let mut fields = Vec::new();
    let mut start = 0usize;
    let mut stack = Vec::new();
    let mut quoted = false;
    let mut escaped = false;
    for (index, ch) in input.char_indices() {
        if quoted {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                quoted = false;
            }
            continue;
        }
        match ch {
            '"' => quoted = true,
            '(' | '[' | '{' => stack.push(ch),
            ')' | ']' | '}' => {
                stack.pop();
            }
            ',' if stack.is_empty() => {
                fields.push(&input[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    fields.push(&input[start..]);
    fields
}

fn extract_debug_string(input: &str, marker: &str, max_bytes: usize) -> Option<String> {
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
                    let hex = format!("{}{}", chars.next()?, chars.next()?);
                    out.push(char::from(u8::from_str_radix(&hex, 16).ok()?));
                }
                'u' => {
                    if chars.next()? != '{' {
                        return None;
                    }
                    let mut hex = String::new();
                    loop {
                        let value = chars.next()?;
                        if value == '}' {
                            break;
                        }
                        hex.push(value);
                        if hex.len() > 6 {
                            return None;
                        }
                    }
                    out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
                }
                _ => return None,
            },
            other => out.push(other),
        }
        if out.len() > max_bytes {
            return None;
        }
    }
    None
}

fn format_checkpoint(min_id: i64, max_id: i64) -> String {
    format!("{CHECKPOINT_VERSION}:min-id={min_id}:max-id={max_id}")
}

fn parse_checkpoint(value: &str) -> Option<(i64, i64)> {
    let rest = value.strip_prefix(&format!("{CHECKPOINT_VERSION}:min-id="))?;
    let (min_id, max_id) = rest.split_once(":max-id=")?;
    Some((min_id.parse().ok()?, max_id.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use tempfile::tempdir;

    fn create_db(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "create table logs(
                id integer primary key,
                ts integer not null,
                level text not null,
                target text not null,
                feedback_log_body text,
                thread_id text,
                estimated_bytes integer not null default 0
            );",
        )
        .unwrap();
        conn
    }

    fn insert(conn: &Connection, id: i64, thread: &str, target: &str, body: &str) {
        conn.execute(
            "insert into logs(id,ts,level,target,thread_id,feedback_log_body,estimated_bytes)
             values(?1,1700000000 + ?1,'DEBUG',?2,?3,?4,100)",
            params![id, target, thread, body],
        )
        .unwrap();
    }

    fn project(thread: &str, text: &str) -> String {
        format!(
            r#"session_loop{{thread_id={thread}}}: Submission sub=Submission {{ id: "s", op: UserInput {{ items: [Text {{ text: "{text}", text_elements: [] }}], final_output_json_schema: None, responsesapi_client_metadata: Some({{"workspace_kind": "project"}}), additional_context: {{}}, thread_settings: ThreadSettingsOverrides {{ environments: Some(TurnEnvironmentSelections {{ legacy_fallback_cwd: AbsolutePathBuf("/tmp/work") }}) }} }} }}"#
        )
    }

    fn assistant(item: &str, text: &str, completed: bool) -> String {
        let prefix = if completed {
            "span:handle_output_item_done"
        } else {
            "span:run_sampling_request"
        };
        format!(
            r#"{prefix}: Output item item=Message {{ id: Some("{item}"), role: "assistant", content: [OutputText {{ text: "{text}" }}], phase: Some(Commentary) }}"#
        )
    }

    #[test]
    fn decodes_rust_debug_escapes_and_rejects_unknown_ones() {
        assert_eq!(
            extract_debug_string("x text: \"a\\n\\x42\\u{1f642}\\\"c\" z", "text: \"", 100,)
                .unwrap(),
            "a\nB🙂\"c"
        );
        assert!(extract_debug_string("text: \"bad\\q\"", "text: \"", 100).is_none());
    }

    #[test]
    fn requires_exact_outer_events_and_prefers_completed_assistant_content() {
        let dir = tempdir().unwrap();
        let conn = create_db(&dir.path().join("logs_2.sqlite"));
        insert(
            &conn,
            1,
            "project",
            "codex_core::session::handlers",
            &project(
                "project",
                r#"hello\n\u{1f642} op: UserInput { responsesapi_client_metadata: Some({\"workspace_kind\": \"project\"}) }"#,
            ),
        );
        insert(
            &conn,
            2,
            "false-user",
            "codex_core::session::handlers",
            r#"session_loop{thread_id=false-user}: Submission sub=Submission { id: "s", op: Interrupt, note: "op: UserInput { responsesapi_client_metadata: Some({\"workspace_kind\": \"project\"}) }" }"#,
        );
        insert(
            &conn,
            3,
            "project",
            "codex_core::stream_events_utils",
            &assistant("m1", "completed", true),
        );
        insert(
            &conn,
            4,
            "project",
            "codex_core::stream_events_utils",
            &assistant("m1", "later-but-incomplete", false),
        );
        insert(
            &conn,
            5,
            "project",
            "codex_core::stream_events_utils",
            r#"span: Output item item=FunctionCall { arguments: ": Output item item=Message { id: Some(\"fake\"), role: \"assistant\", content: [OutputText { text: \"nested\" }] }" }"#,
        );
        drop(conn);

        let recovery = CodexLogsAdapter::new(dir.path())
            .recover(&HashSet::new(), None)
            .unwrap();
        assert!(recovery.replace_all);
        assert_eq!(recovery.sessions.len(), 1);
        let session = &recovery.sessions[0];
        assert_eq!(session.session.id, "codex:project");
        assert_eq!(session.session.cwd.as_deref(), Some("/tmp/work"));
        assert!(session.transcript_text.contains("hello\n🙂"));
        assert!(session.transcript_text.contains("completed"));
        assert!(!session.transcript_text.contains("later-but-incomplete"));
        assert!(!session.transcript_text.contains("nested"));
        assert!(session.session.created_at.is_some());
    }

    #[test]
    fn growth_reparses_only_affected_threads_and_shrink_resets() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("logs_2.sqlite");
        let conn = create_db(&path);
        insert(
            &conn,
            1,
            "one",
            "codex_core::session::handlers",
            &project("one", "first"),
        );
        insert(
            &conn,
            2,
            "two",
            "codex_core::session::handlers",
            &project("two", "second"),
        );
        drop(conn);
        let adapter = CodexLogsAdapter::new(dir.path());
        let first = adapter.recover(&HashSet::new(), None).unwrap();
        let checkpoint = first.checkpoint.unwrap();

        let conn = Connection::open(&path).unwrap();
        insert(
            &conn,
            3,
            "one",
            "codex_core::stream_events_utils",
            &assistant("m1", "tail", true),
        );
        drop(conn);
        let tail = adapter.recover(&HashSet::new(), Some(&checkpoint)).unwrap();
        assert!(!tail.replace_all);
        assert_eq!(tail.affected_ids, vec!["one"]);
        assert_eq!(tail.sessions.len(), 1);
        assert!(tail.sessions[0].transcript_text.contains("tail"));

        let conn = Connection::open(&path).unwrap();
        conn.execute("delete from logs where id = 1", []).unwrap();
        drop(conn);
        let reset = adapter
            .recover(&HashSet::new(), tail.checkpoint.as_deref())
            .unwrap();
        assert!(reset.replace_all);
    }

    #[test]
    fn durable_sessions_win_and_summary_only_logging_is_diagnosed() {
        let dir = tempdir().unwrap();
        let conn = create_db(&dir.path().join("logs_2.sqlite"));
        insert(
            &conn,
            1,
            "summary",
            "codex_core::session::handlers",
            &project("summary", "recover me"),
        );
        insert(
            &conn,
            2,
            "durable",
            "codex_core::session::handlers",
            &project("durable", "do not recover"),
        );
        drop(conn);
        let durable = HashSet::from(["durable".to_string()]);
        let recovery = CodexLogsAdapter::new(dir.path())
            .recover(&durable, None)
            .unwrap();
        assert_eq!(recovery.sessions.len(), 1);
        assert_eq!(recovery.sessions[0].session.id, "codex:summary");
        assert_eq!(recovery.health.content_unavailable, 1);
        assert!(recovery.sessions[0]
            .session
            .parse_warning
            .as_deref()
            .unwrap()
            .contains("assistant content unavailable"));
    }
}
