use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use ignore::WalkBuilder;
use regex::Regex;
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use serde_json::{json, Value};

use crate::models::{ParsedSession, Provider, SessionRecord, SourceFile};
use crate::util::{
    consists_of_tag_blocks, extract_text, find_repo_root, format_transcript_line, minimal_record,
    normalize_path, parse_datetime, parse_unix_seconds, preview_from_text, truncate_for_display,
};

#[derive(Debug, Clone, Default, Serialize)]
struct CodexMetadata {
    title: Option<String>,
    cwd: Option<String>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    rollout_path: Option<String>,
    first_user_message: Option<String>,
}

pub struct CodexAdapter {
    roots: Vec<PathBuf>,
    threads: HashMap<String, CodexMetadata>,
    index_titles: HashMap<String, String>,
    id_re: Regex,
}

impl CodexAdapter {
    pub fn new(roots: Vec<PathBuf>, codex_home: PathBuf) -> Self {
        let threads = latest_state_db(&codex_home)
            .and_then(|path| load_threads(&path).ok())
            .unwrap_or_default();
        let index_titles =
            load_index_titles(&codex_home.join("session_index.jsonl")).unwrap_or_default();
        Self {
            roots,
            threads,
            index_titles,
            id_re: Regex::new(r"([0-9a-f]{8}-[0-9a-f-]{27})\.jsonl$").expect("valid regex"),
        }
    }

    pub fn discover(&self) -> Vec<SourceFile> {
        let mut files = Vec::new();
        for root in &self.roots {
            if !root.exists() {
                continue;
            }
            let walker = WalkBuilder::new(root)
                .hidden(false)
                .ignore(false)
                .git_ignore(false)
                .git_exclude(false)
                .parents(false)
                .build();
            for entry in walker.flatten() {
                let path = entry.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                    continue;
                }
                if let Ok(metadata) = entry.metadata() {
                    let mtime_ns = metadata
                        .modified()
                        .ok()
                        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|value| value.as_nanos() as i64)
                        .unwrap_or_default();
                    files.push(SourceFile {
                        provider: Provider::Codex,
                        path: path.to_path_buf(),
                        mtime_ns,
                        size_bytes: metadata.len() as i64,
                    });
                }
            }
        }
        files
    }

    /// Parse a rollout file. Returns `None` for sub-agent sessions (guardian
    /// reviews, spawned agents): they replay their parent's history and would
    /// duplicate it in search results.
    pub fn parse(&self, source: &SourceFile) -> Option<ParsedSession> {
        match self.parse_inner(&source.path) {
            Ok(parsed) => parsed,
            Err(err) => Some(minimal_record(
                Provider::Codex,
                &source.path,
                err.to_string(),
            )),
        }
    }

    pub fn metadata_fingerprint(&self, source: &SourceFile) -> String {
        let provider_session_id = self.extract_id(&source.path);
        let metadata = provider_session_id
            .as_deref()
            .and_then(|id| self.threads.get(id));
        let explicit_title = provider_session_id
            .as_deref()
            .and_then(|id| self.index_titles.get(id));
        let bytes = serde_json::to_vec(&("codex-metadata-v2", metadata, explicit_title))
            .expect("Codex metadata should serialize");
        stable_fingerprint(&bytes)
    }

    fn parse_inner(&self, path: &Path) -> Result<Option<ParsedSession>> {
        // Stream the rollout: sub-agents are recognised from the first line, so
        // their (often large) replayed history is never read.
        let reader = BufReader::new(File::open(path)?);
        let mut line_count = 0usize;
        let mut provider_session_id = self
            .extract_id(path)
            .unwrap_or_else(|| "unknown".to_string());
        let mut cwd = None;
        let mut created_at = None;
        let mut updated_at = None;
        let mut transcript_lines = Vec::new();
        let mut message_count = 0i64;
        let mut first_user = None;
        let mut last_user = None;

        for line in reader.lines() {
            let line = line?;
            line_count += 1;
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(&line) {
                Ok(value) => value,
                Err(_) => continue,
            };
            let timestamp = value
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(parse_datetime);
            match value.get("type").and_then(Value::as_str) {
                Some("session_meta") => {
                    if let Some(payload) = value.get("payload") {
                        // Codex marks sub-agents with source.subagent and links them to
                        // their parent; either marker is enough.
                        let has_parent = payload
                            .get("parent_thread_id")
                            .is_some_and(|parent| !parent.is_null());
                        if has_parent || payload.pointer("/source/subagent").is_some() {
                            return Ok(None);
                        }
                        if let Some(id) = payload.get("id").and_then(Value::as_str) {
                            provider_session_id = id.to_string();
                        }
                        if cwd.is_none() {
                            cwd = payload
                                .get("cwd")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned);
                        }
                        if created_at.is_none() {
                            created_at = payload
                                .get("timestamp")
                                .and_then(Value::as_str)
                                .and_then(parse_datetime);
                        }
                    }
                }
                Some("response_item") => {
                    if let Some(payload) = value.get("payload") {
                        let item_type = payload.get("type").and_then(Value::as_str);
                        let role = payload.get("role").and_then(Value::as_str);
                        if item_type == Some("message")
                            && matches!(role, Some("user" | "assistant"))
                        {
                            let text = message_text(payload);
                            if text.trim().is_empty() {
                                continue;
                            }
                            message_count += 1;
                            if role == Some("user") {
                                if first_user.is_none() {
                                    first_user = Some(text.clone());
                                }
                                last_user = Some(text.clone());
                            }
                            updated_at = timestamp.or(updated_at);
                            transcript_lines.push(format_transcript_line(
                                role.unwrap_or("message"),
                                timestamp,
                                &text,
                            ));
                        }
                    }
                }
                _ => {}
            }
        }

        let meta = self
            .threads
            .get(&provider_session_id)
            .cloned()
            .unwrap_or_default();
        let title = self
            .index_titles
            .get(&provider_session_id)
            .cloned()
            .or_else(|| meta.title.filter(|title| !title.trim().is_empty()))
            .or_else(|| first_user.clone())
            .map(|text| truncate_for_display(&text, 100));
        let summary = meta
            .first_user_message
            .or_else(|| first_user.clone())
            .map(|text| truncate_for_display(&text, 180));
        let cwd = cwd.or(meta.cwd);
        let repo_root = cwd.as_deref().and_then(find_repo_root);
        let created_at = created_at.or(meta.created_at);
        let updated_at = updated_at.or(meta.updated_at);
        let preview = last_user
            .clone()
            .or_else(|| first_user.clone())
            .or_else(|| summary.clone())
            .map(|text| preview_from_text(&text))
            .unwrap_or_else(|| "(no preview available)".to_string());
        let raw_metadata_json = Some(serde_json::to_string(&json!({
            "line_count": line_count,
            "rollout_path": meta.rollout_path,
            "session_path": normalize_path(path),
        }))?);

        let session = SessionRecord {
            id: format!("codex:{provider_session_id}"),
            provider: Provider::Codex,
            provider_session_id,
            title,
            summary,
            cwd,
            repo_root,
            created_at,
            updated_at,
            last_message_at: updated_at,
            preview_text: preview,
            source_path: normalize_path(path),
            message_count: Some(message_count),
            parse_version: "codex-v3".to_string(),
            raw_metadata_json,
            parse_warning: None,
            discovery_source: "jsonl+sqlite".to_string(),
        };

        Ok(Some(ParsedSession {
            session,
            transcript_text: transcript_lines.join("\n\n"),
        }))
    }

    fn extract_id(&self, path: &Path) -> Option<String> {
        let value = path.to_string_lossy();
        self.id_re
            .captures(&value)
            .and_then(|captures| captures.get(1))
            .map(|match_| match_.as_str().to_string())
    }
}

/// Tags of context Codex injects into the conversation as user-role messages:
/// environment details, instructions, and turn bookkeeping. None of it was
/// typed by the user.
const INJECTED_TAGS: &[&str] = &[
    "environment_context",
    "user_instructions",
    "permissions instructions",
    "recommended_plugins",
    "turn_aborted",
];

/// Whether a content item is injected context rather than user text: nothing
/// but complete injected tag blocks, or an AGENTS.md block (which would
/// otherwise match every session in its repo). A prompt that merely starts
/// with one of these tags is kept.
fn is_injected(text: &str) -> bool {
    let agents_md = text.starts_with("# AGENTS.md instructions for ")
        && text.trim_end().ends_with("</INSTRUCTIONS>");
    agents_md || consists_of_tag_blocks(text, INJECTED_TAGS)
}

/// Text of a Codex message item, minus injected context blocks.
fn message_text(payload: &Value) -> String {
    let Some(items) = payload.get("content").and_then(Value::as_array) else {
        return extract_text(payload);
    };
    items
        .iter()
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .map(str::trim)
        .filter(|text| !text.is_empty() && !is_injected(text))
        .collect::<Vec<_>>()
        .join("\n")
}

fn stable_fingerprint(bytes: &[u8]) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

/// Codex keeps thread metadata in `state_<N>.sqlite`, bumping N on schema
/// changes. Use the highest version present.
fn latest_state_db(codex_home: &Path) -> Option<PathBuf> {
    let state_re = Regex::new(r"^state_(\d+)\.sqlite$").expect("valid regex");
    fs::read_dir(codex_home)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let version: u32 = state_re.captures(name.to_str()?)?[1].parse().ok()?;
            Some((version, entry.path()))
        })
        .max_by_key(|(version, _)| *version)
        .map(|(_, path)| path)
}

/// Read Codex's thread table without creating or changing files in its directory.
///
/// While Codex runs, its database is in WAL mode with `-wal`/`-shm` files beside
/// it; a read-only connection shares them and sees uncheckpointed writes. When
/// they're absent, every write is in the main file, and SQLite must not create
/// them (a read-only connection would, and leave them behind), so open it
/// immutable instead. Immutable reads take no locks, so if Codex starts while
/// we read, the snapshot may be stale; read again through its WAL.
fn load_threads(path: &Path) -> Result<HashMap<String, CodexMetadata>> {
    let read_only = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let wal_in_use = || {
        ["-wal", "-shm"].iter().all(|suffix| {
            let mut name = path.as_os_str().to_owned();
            name.push(suffix);
            PathBuf::from(name).exists()
        })
    };
    if !wal_in_use() {
        let uri = immutable_uri(&path.to_string_lossy(), cfg!(windows));
        let result = Connection::open_with_flags(uri, read_only | OpenFlags::SQLITE_OPEN_URI)
            .map_err(anyhow::Error::from)
            .and_then(|conn| query_threads(&conn));
        if !wal_in_use() {
            return result;
        }
    }
    query_threads(&Connection::open_with_flags(path, read_only)?)
}

/// SQLite URI opening `path` immutably. Absolute paths get an empty authority
/// (`file:///...`) so a path starting with `//` isn't read as a host name, and
/// Windows paths use forward slashes with a leading `/` before the drive.
fn immutable_uri(path: &str, windows: bool) -> String {
    let mut escaped = String::new();
    for ch in path.chars() {
        match ch {
            '%' => escaped.push_str("%25"),
            '?' => escaped.push_str("%3f"),
            '#' => escaped.push_str("%23"),
            '\\' if windows => escaped.push('/'),
            _ => escaped.push(ch),
        }
    }
    if windows && escaped.as_bytes().get(1) == Some(&b':') {
        escaped.insert(0, '/');
    }
    let authority = if escaped.starts_with('/') { "//" } else { "" };
    format!("file:{authority}{escaped}?immutable=1")
}

fn query_threads(conn: &Connection) -> Result<HashMap<String, CodexMetadata>> {
    let mut stmt = conn.prepare(
        "select id, title, cwd, created_at, updated_at, rollout_path, first_user_message from threads",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            CodexMetadata {
                title: row.get::<_, Option<String>>(1)?,
                cwd: row.get::<_, Option<String>>(2)?,
                created_at: row.get::<_, Option<i64>>(3)?.and_then(parse_unix_seconds),
                updated_at: row.get::<_, Option<i64>>(4)?.and_then(parse_unix_seconds),
                rollout_path: row.get::<_, Option<String>>(5)?,
                first_user_message: row.get::<_, Option<String>>(6)?,
            },
        ))
    })?;

    let mut map = HashMap::new();
    for row in rows {
        let (id, meta) = row?;
        map.insert(id, meta);
    }
    Ok(map)
}

fn load_index_titles(path: &Path) -> Result<HashMap<String, String>> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let raw = fs::read_to_string(path)?;
    let mut map = HashMap::new();
    for line in raw.lines() {
        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if let (Some(id), Some(title)) = (
            value.get("id").and_then(Value::as_str),
            value.get("thread_name").and_then(Value::as_str),
        ) {
            let title = title.trim();
            if title.is_empty() {
                map.remove(id);
            } else {
                map.insert(id.to_string(), title.to_string());
            }
        }
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::CodexAdapter;
    use rusqlite::{params, Connection};
    use std::fs;
    use tempfile::tempdir;

    const SESSION_ID: &str = "019f337f-adda-7271-9924-f43714dd8c8e";

    fn write_rollout(root: &std::path::Path) -> std::path::PathBuf {
        fs::create_dir_all(root).unwrap();
        let path = root.join(format!("rollout-2026-08-01T12-00-00-{SESSION_ID}.jsonl"));
        fs::write(
            &path,
            format!(
                r#"{{"timestamp":"2026-08-01T12:00:00Z","type":"session_meta","payload":{{"id":"{SESSION_ID}","cwd":"/tmp/demo","timestamp":"2026-08-01T12:00:00Z"}}}}
{{"timestamp":"2026-08-01T12:00:01Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"Generated from first prompt"}}]}}}}
"#
            ),
        )
        .unwrap();
        path
    }

    #[test]
    fn injected_context_is_not_treated_as_user_turns() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("sessions");
        fs::create_dir_all(&root).unwrap();
        let path = root.join(format!("rollout-2026-08-01T12-00-00-{SESSION_ID}.jsonl"));
        let user = |texts: &[&str]| {
            let content: Vec<_> = texts
                .iter()
                .map(|text| serde_json::json!({"type": "input_text", "text": text}))
                .collect();
            serde_json::json!({"timestamp": "2026-08-01T12:00:01Z", "type": "response_item",
                "payload": {"type": "message", "role": "user", "content": content}})
            .to_string()
        };
        let lines = [
            user(&["# AGENTS.md instructions for /repo\n\n<INSTRUCTIONS>AGENTS BODY</INSTRUCTIONS>"]),
            user(&["<environment_context>\n  <cwd>/repo</cwd>\n</environment_context>"]),
            user(&["Investigate the flaky test"]),
            user(&["<turn_aborted>\nThe user interrupted the previous turn on purpose.\n</turn_aborted>"]),
        ];
        fs::write(&path, lines.join("\n")).unwrap();

        let adapter = CodexAdapter::new(vec![root], temp.path().join("codex-home"));
        let parsed = adapter.parse(&adapter.discover()[0]).unwrap();

        assert!(!parsed.transcript_text.contains("AGENTS BODY"));
        assert!(!parsed.transcript_text.contains("environment_context"));
        assert!(!parsed.transcript_text.contains("interrupted"));
        assert_eq!(parsed.session.message_count, Some(1));
        assert_eq!(
            parsed.session.title.as_deref(),
            Some("Investigate the flaky test")
        );
        assert_eq!(parsed.session.preview_text, "Investigate the flaky test");
    }

    #[test]
    fn subagent_sessions_are_skipped() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("sessions");
        fs::create_dir_all(&root).unwrap();
        let path = root.join(format!("rollout-2026-08-01T12-00-00-{SESSION_ID}.jsonl"));
        fs::write(
            &path,
            format!(
                r#"{{"timestamp":"2026-08-01T12:00:00Z","type":"session_meta","payload":{{"id":"{SESSION_ID}","parent_thread_id":"parent","source":{{"subagent":{{"other":"guardian"}}}}}}}}
{{"timestamp":"2026-08-01T12:00:01Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"The following is the Codex agent history"}}]}}}}
"#
            ),
        )
        .unwrap();

        let adapter = CodexAdapter::new(vec![root.clone()], temp.path().join("codex-home"));
        assert!(adapter.parse(&adapter.discover()[0]).is_none());

        // A parent link alone also marks a sub-agent.
        fs::write(
            &path,
            format!(
                r#"{{"timestamp":"2026-08-01T12:00:00Z","type":"session_meta","payload":{{"id":"{SESSION_ID}","parent_thread_id":"parent","source":"vscode"}}}}
{{"timestamp":"2026-08-01T12:00:01Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"replayed parent history"}}]}}}}
"#
            ),
        )
        .unwrap();
        let adapter = CodexAdapter::new(vec![root.clone()], temp.path().join("codex-home"));
        assert!(adapter.parse(&adapter.discover()[0]).is_none());

        // The rest of a sub-agent rollout is never read: bytes that aren't valid
        // UTF-8 after the first line would otherwise surface as a parse failure.
        let mut bytes = format!(
            r#"{{"timestamp":"2026-08-01T12:00:00Z","type":"session_meta","payload":{{"id":"{SESSION_ID}","source":{{"subagent":"guardian"}}}}}}"#
        )
        .into_bytes();
        bytes.extend_from_slice(b"\n\xff\xfe not utf-8\n");
        fs::write(&path, bytes).unwrap();
        let adapter = CodexAdapter::new(vec![root], temp.path().join("codex-home"));
        assert!(adapter.parse(&adapter.discover()[0]).is_none());
    }

    #[test]
    fn reads_wal_state_db_without_writing_to_it() {
        let temp = tempdir().unwrap();
        write_state_db(temp.path());
        let db = temp.path().join("state_5.sqlite");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch("pragma journal_mode = wal").unwrap();
        }
        // Last connection closed: SQLite removed -wal/-shm, as when Codex isn't running.
        assert!(!temp.path().join("state_5.sqlite-shm").exists());
        let before = fs::read(&db).unwrap();

        let threads = super::load_threads(&db).unwrap();

        assert!(threads.contains_key(SESSION_ID));
        assert_eq!(fs::read(&db).unwrap(), before);
        let mut names: Vec<_> = fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, vec!["state_5.sqlite"]);
    }

    #[test]
    fn reads_uncheckpointed_writes_while_codex_holds_the_db_open() {
        let temp = tempdir().unwrap();
        let db = temp.path().join("state_5.sqlite");
        let codex = Connection::open(&db).unwrap();
        codex
            .execute_batch(
                "pragma journal_mode = wal; pragma wal_autocheckpoint = 0;
                 create table threads (id text primary key, title text, cwd text,
                     created_at integer, updated_at integer, rollout_path text,
                     first_user_message text);
                 insert into threads (id, title) values ('live', 'Only in the WAL');",
            )
            .unwrap();

        let threads = super::load_threads(&db).unwrap();

        assert_eq!(
            threads.get("live").and_then(|meta| meta.title.as_deref()),
            Some("Only in the WAL")
        );
        drop(codex);
    }

    #[test]
    fn reads_state_db_under_a_double_slash_path() {
        let temp = tempdir().unwrap();
        write_state_db(temp.path());
        let doubled =
            std::path::PathBuf::from(format!("/{}", temp.path().display())).join("state_5.sqlite");
        assert!(super::load_threads(&doubled)
            .unwrap()
            .contains_key(SESSION_ID));
    }

    #[test]
    fn immutable_uris_handle_unix_and_windows_paths() {
        use super::immutable_uri;
        assert_eq!(
            immutable_uri("/home/me/state_5.sqlite", false),
            "file:///home/me/state_5.sqlite?immutable=1"
        );
        assert_eq!(
            immutable_uri("//odd/path#1?.sqlite", false),
            "file:////odd/path%231%3f.sqlite?immutable=1"
        );
        assert_eq!(
            immutable_uri(r"C:\Users\me\.codex\state_5.sqlite", true),
            "file:///C:/Users/me/.codex/state_5.sqlite?immutable=1"
        );
        // Backslashes are ordinary filename characters on Unix.
        assert_eq!(
            immutable_uri(r"rel\x.sqlite", false),
            r"file:rel\x.sqlite?immutable=1"
        );
    }

    #[test]
    fn prompts_that_only_start_with_an_injected_tag_are_kept() {
        assert!(super::is_injected(
            "<turn_aborted>\nThe user interrupted.\n</turn_aborted>"
        ));
        assert!(super::is_injected(
            "# AGENTS.md instructions for /repo\n\n<INSTRUCTIONS>\nbody\n</INSTRUCTIONS>"
        ));
        assert!(!super::is_injected(
            "<environment_context> -- what is this tag?"
        ));
        assert!(!super::is_injected(
            "<turn_aborted>x</turn_aborted> why did my turn abort?"
        ));
        assert!(!super::is_injected(
            "# AGENTS.md instructions for /repo: should I add one?"
        ));
    }

    #[test]
    fn picks_the_newest_state_db_version() {
        let temp = tempdir().unwrap();
        for name in [
            "state_5.sqlite",
            "state_12.sqlite",
            "state_9.sqlite-wal",
            "logs_20.sqlite",
        ] {
            fs::write(temp.path().join(name), "").unwrap();
        }
        assert_eq!(
            super::latest_state_db(temp.path()),
            Some(temp.path().join("state_12.sqlite"))
        );
    }

    fn write_state_db(home: &std::path::Path) {
        let conn = Connection::open(home.join("state_5.sqlite")).unwrap();
        conn.execute_batch(
            "create table threads (
                id text primary key,
                title text,
                cwd text,
                created_at integer,
                updated_at integer,
                rollout_path text,
                first_user_message text
            );",
        )
        .unwrap();
        conn.execute(
            "insert into threads (
                id, title, cwd, created_at, updated_at, rollout_path, first_user_message
             ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                SESSION_ID,
                "Generated from first prompt",
                "/tmp/demo",
                1_754_048_000_i64,
                1_754_048_001_i64,
                "/tmp/rollout.jsonl",
                "Generated from first prompt",
            ],
        )
        .unwrap();
    }

    fn append_name(home: &std::path::Path, name: &str) {
        use std::io::Write;

        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(home.join("session_index.jsonl"))
            .unwrap();
        writeln!(
            file,
            r#"{{"id":"{SESSION_ID}","thread_name":"{name}","updated_at":"2026-08-01T12:00:02Z"}}"#
        )
        .unwrap();
    }

    #[test]
    fn explicit_session_name_takes_precedence_over_generated_title() {
        let temp = tempdir().unwrap();
        let home = temp.path();
        let sessions = home.join("sessions");
        write_rollout(&sessions);
        write_state_db(home);
        append_name(home, "Old session name");
        append_name(home, "Meaningful session name");

        let adapter = CodexAdapter::new(vec![sessions], home.to_path_buf());
        let sources = adapter.discover();
        let parsed = adapter.parse(&sources[0]).unwrap();

        assert_eq!(
            parsed.session.title.as_deref(),
            Some("Meaningful session name")
        );
    }

    #[test]
    fn blank_latest_name_restores_generated_title() {
        let temp = tempdir().unwrap();
        let home = temp.path();
        let sessions = home.join("sessions");
        write_rollout(&sessions);
        write_state_db(home);
        append_name(home, "Old session name");
        append_name(home, "   ");

        let adapter = CodexAdapter::new(vec![sessions], home.to_path_buf());
        let sources = adapter.discover();
        let parsed = adapter.parse(&sources[0]).unwrap();

        assert_eq!(
            parsed.session.title.as_deref(),
            Some("Generated from first prompt")
        );
    }

    #[test]
    fn metadata_fingerprint_changes_when_only_session_name_changes() {
        let temp = tempdir().unwrap();
        let home = temp.path();
        let sessions = home.join("sessions");
        write_rollout(&sessions);
        write_state_db(home);
        append_name(home, "Old name");

        let old_adapter = CodexAdapter::new(vec![sessions.clone()], home.to_path_buf());
        let old_source = old_adapter.discover().remove(0);
        let old_fingerprint = old_adapter.metadata_fingerprint(&old_source);

        append_name(home, "New name");

        let new_adapter = CodexAdapter::new(vec![sessions], home.to_path_buf());
        let new_source = new_adapter.discover().remove(0);
        let new_fingerprint = new_adapter.metadata_fingerprint(&new_source);

        assert_eq!(old_source.mtime_ns, new_source.mtime_ns);
        assert_eq!(old_source.size_bytes, new_source.size_bytes);
        assert_ne!(old_fingerprint, new_fingerprint);
    }
}
