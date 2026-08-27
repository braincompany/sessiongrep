use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use ignore::WalkBuilder;
use regex::Regex;
use rusqlite::Connection;
use serde::Serialize;
use serde_json::{Value, json};

use crate::models::{ParsedSession, Provider, SessionRecord, SourceFile};
use crate::util::{
    extract_text, find_repo_root, format_transcript_line, minimal_record, normalize_path,
    parse_datetime, parse_unix_seconds, preview_from_text, truncate_for_display,
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
        let threads = load_threads(&codex_home.join("state_5.sqlite")).unwrap_or_default();
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

    pub fn parse(&self, source: &SourceFile) -> ParsedSession {
        match self.parse_inner(&source.path) {
            Ok(parsed) => parsed,
            Err(err) => minimal_record(Provider::Codex, &source.path, err.to_string()),
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

    fn parse_inner(&self, path: &Path) -> Result<ParsedSession> {
        let raw = fs::read_to_string(path)?;
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

        for line in raw.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(line) {
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
                            let text = extract_text(payload);
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
            "line_count": raw.lines().count(),
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
            parse_version: "codex-v2".to_string(),
            raw_metadata_json,
            parse_warning: None,
            discovery_source: "jsonl+sqlite".to_string(),
        };

        Ok(ParsedSession {
            session,
            transcript_text: transcript_lines.join("\n\n"),
        })
    }

    fn extract_id(&self, path: &Path) -> Option<String> {
        let value = path.to_string_lossy();
        self.id_re
            .captures(&value)
            .and_then(|captures| captures.get(1))
            .map(|match_| match_.as_str().to_string())
    }
}

fn stable_fingerprint(bytes: &[u8]) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn load_threads(path: &Path) -> Result<HashMap<String, CodexMetadata>> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let conn = Connection::open(path)?;
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
    use rusqlite::{Connection, params};
    use std::fs;
    use tempfile::tempdir;

    const SESSION_ID: &str = "019f337f-adda-7271-9924-f43714dd8c8e";

    fn write_rollout(root: &std::path::Path) -> std::path::PathBuf {
        fs::create_dir_all(root).unwrap();
        let path = root.join(format!(
            "rollout-2026-08-01T12-00-00-{SESSION_ID}.jsonl"
        ));
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
        let parsed = adapter.parse(&sources[0]);

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
        let parsed = adapter.parse(&sources[0]);

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

