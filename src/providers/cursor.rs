use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use ignore::WalkBuilder;
use serde_json::{json, Value};

use crate::models::{ParsedSession, Provider, SessionRecord, SourceFile};
use crate::util::{
    find_repo_root, format_transcript_line, minimal_record, normalize_path, preview_from_text,
    substantive_text, truncate_for_display,
};

pub struct CursorAdapter {
    roots: Vec<PathBuf>,
}

impl CursorAdapter {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self { roots }
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
                if path
                    .components()
                    .any(|component| component.as_os_str() == "subagents")
                {
                    continue;
                }
                if !path
                    .components()
                    .any(|component| component.as_os_str() == "agent-transcripts")
                {
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
                        provider: Provider::Cursor,
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
            Err(err) => minimal_record(Provider::Cursor, &source.path, err.to_string()),
        }
    }

    fn parse_inner(&self, path: &Path) -> Result<ParsedSession> {
        let raw = fs::read_to_string(path)?;
        let provider_session_id = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("unknown")
            .to_string();
        let cwd = infer_cursor_workspace(path);
        let mut created_at: Option<DateTime<Utc>> = None;
        let updated_at = file_modified_at(path);
        let mut messages = Vec::new();
        let mut transcript_lines = Vec::new();

        for line in raw.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(line) {
                Ok(value) => value,
                Err(_) => continue,
            };
            let Some(role) = value.get("role").and_then(Value::as_str) else {
                continue;
            };
            if !matches!(role, "user" | "assistant") {
                continue;
            }
            let text = cursor_message_text(&value);
            if !substantive_text(&text) {
                continue;
            }
            if created_at.is_none() {
                created_at = updated_at;
            }
            messages.push((role.to_string(), text.clone()));
            transcript_lines.push(format_transcript_line(role, updated_at, &text));
        }

        let first_user = messages
            .iter()
            .find(|(role, text)| role == "user" && substantive_text(text))
            .map(|(_, text)| text.clone());
        let last_user = messages
            .iter()
            .rev()
            .find(|(role, text)| role == "user" && substantive_text(text))
            .map(|(_, text)| text.clone());
        let title = last_user
            .clone()
            .or_else(|| first_user.clone())
            .map(|text| truncate_for_display(&text, 100));
        let preview = last_user
            .clone()
            .or_else(|| first_user.clone())
            .map(|text| preview_from_text(&text))
            .unwrap_or_else(|| "(no preview available)".to_string());
        let repo_root = cwd.as_deref().and_then(find_repo_root);
        let raw_metadata_json = Some(serde_json::to_string(&json!({
            "line_count": raw.lines().count(),
            "session_path": normalize_path(path),
        }))?);

        let session = SessionRecord {
            id: format!("cursor:{provider_session_id}"),
            provider: Provider::Cursor,
            provider_session_id,
            title,
            summary: first_user.map(|text| truncate_for_display(&text, 180)),
            cwd,
            repo_root,
            created_at,
            updated_at,
            last_message_at: updated_at,
            preview_text: preview,
            source_path: normalize_path(path),
            message_count: Some(messages.len() as i64),
            parse_version: "cursor-v1".to_string(),
            raw_metadata_json,
            parse_warning: None,
            discovery_source: "jsonl".to_string(),
        };

        Ok(ParsedSession {
            session,
            transcript_text: transcript_lines.join("\n\n"),
        })
    }
}

fn cursor_message_text(value: &Value) -> String {
    let Some(message) = value.get("message") else {
        return String::new();
    };
    let mut parts = Vec::new();
    if let Some(content) = message.get("content") {
        collect_text_content(content, &mut parts);
    }
    let text = parts.join("\n");
    extract_tag(&text, "user_query")
        .unwrap_or(text)
        .trim()
        .to_string()
}

fn collect_text_content(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) => {
            if !text.trim().is_empty() {
                out.push(text.trim().to_string());
            }
        }
        Value::Array(items) => {
            for item in items {
                if item.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        if !text.trim().is_empty() {
                            out.push(text.trim().to_string());
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

fn extract_tag(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    Some(text[start..end].trim().to_string())
}

fn file_modified_at(path: &Path) -> Option<DateTime<Utc>> {
    path.metadata()
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .map(DateTime::<Utc>::from)
}

fn infer_cursor_workspace(path: &Path) -> Option<String> {
    let mut previous: Option<String> = None;
    for component in path.components() {
        if component.as_os_str() == "agent-transcripts" {
            return previous
                .as_deref()
                .and_then(decode_cursor_project_dir)
                .map(|path| path.to_string_lossy().to_string());
        }
        previous = component.as_os_str().to_str().map(ToOwned::to_owned);
    }
    None
}

/// Filesystem checks allowed per decoded folder name. Real workspace paths need
/// a few dozen; the cap only bites on trees full of overlapping hyphenated
/// directory names (`a`, `a-a`, `a-a-a`, ...), where the session gets no cwd.
const MAX_PATH_PROBES: usize = 4096;

/// Cursor names project folders after the workspace path with `/` replaced by
/// `-` (e.g. `Users-me-src-my-app`), which is ambiguous when directory names
/// contain hyphens. Rebuild the path one component at a time, only descending
/// into directories that exist, within a fixed budget of filesystem checks.
fn decode_cursor_project_dir(encoded: &str) -> Option<PathBuf> {
    if encoded.is_empty() || encoded == "empty-window" {
        return None;
    }
    // Consecutive hyphens yield empty parts; they rejoin into names like `a--b`.
    let parts: Vec<&str> = encoded.split('-').collect();
    let mut budget = MAX_PATH_PROBES;
    resolve_hyphenated(Path::new("/"), &parts, &mut budget)
}

fn resolve_hyphenated(base: &Path, parts: &[&str], budget: &mut usize) -> Option<PathBuf> {
    if parts.is_empty() {
        return Some(base.to_path_buf());
    }
    for end in 1..=parts.len() {
        if *budget == 0 {
            return None;
        }
        *budget -= 1;
        let name = parts[..end].join("-");
        if name.is_empty() {
            continue;
        }
        let candidate = base.join(name);
        if end == parts.len() {
            if candidate.exists() {
                return Some(candidate);
            }
        } else if candidate.is_dir() {
            if let Some(found) = resolve_hyphenated(&candidate, &parts[end..], budget) {
                return Some(found);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{cursor_message_text, decode_cursor_project_dir, extract_tag, CursorAdapter};
    use crate::models::Provider;
    use serde_json::json;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn discovers_and_parses_cursor_parent_transcripts() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("projects");
        let session_id = "9f3b844f-072d-4c2a-bdb8-474fe89dbca1";
        let transcript_dir = root
            .join("Users-adamzhao-Desktop-sessiongrep")
            .join("agent-transcripts")
            .join(session_id);
        fs::create_dir_all(&transcript_dir).expect("create transcript dir");
        let transcript_path = transcript_dir.join(format!("{session_id}.jsonl"));
        fs::write(
            &transcript_path,
            r#"{"role":"user","message":{"content":[{"type":"text","text":"<timestamp>Tuesday</timestamp>\n<user_query>\nMake Cursor threads searchable\n</user_query>"}]}}
{"role":"assistant","message":{"content":[{"type":"text","text":"I will wire a Cursor provider."},{"type":"tool_use","name":"ReadFile","input":{"path":"/tmp/nope"}}]}}
{"role":"user","message":{"content":[{"type":"text","text":"Great, add tests too"}]}}
"#,
        )
        .expect("write transcript");

        let subagent_dir = transcript_dir.join("subagents");
        fs::create_dir_all(&subagent_dir).expect("create subagent dir");
        fs::write(
            subagent_dir.join("subagent.jsonl"),
            r#"{"role":"user","message":{"content":[{"type":"text","text":"subagent"}]}}"#,
        )
        .expect("write subagent transcript");

        let adapter = CursorAdapter::new(vec![root]);
        let sources = adapter.discover();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].provider, Provider::Cursor);
        assert_eq!(sources[0].path, transcript_path);

        let parsed = adapter.parse(&sources[0]);
        assert_eq!(parsed.session.id, format!("cursor:{session_id}"));
        assert_eq!(parsed.session.provider_session_id, session_id);
        assert_eq!(
            parsed.session.title.as_deref(),
            Some("Great, add tests too")
        );
        assert_eq!(
            parsed.session.summary.as_deref(),
            Some("Make Cursor threads searchable")
        );
        assert_eq!(parsed.session.message_count, Some(3));
        assert!(parsed
            .transcript_text
            .contains("Make Cursor threads searchable"));
        assert!(parsed
            .transcript_text
            .contains("I will wire a Cursor provider."));
        assert!(!parsed.transcript_text.contains("ReadFile"));
        assert!(!parsed.transcript_text.contains("subagent"));
    }

    #[test]
    fn extracts_user_query_from_cursor_message() {
        let value = json!({
            "role": "user",
            "message": {
                "content": [{
                    "type": "text",
                    "text": "<timestamp>Tuesday</timestamp>\n<user_query>\nFind the billing bug\n</user_query>"
                }]
            }
        });
        assert_eq!(cursor_message_text(&value), "Find the billing bug");
    }

    #[test]
    fn ignores_tool_use_payloads_in_cursor_messages() {
        let value = json!({
            "role": "assistant",
            "message": {
                "content": [
                    {"type": "text", "text": "I found it."},
                    {"type": "tool_use", "name": "ReadFile", "input": {"path": "/tmp/secret"}}
                ]
            }
        });
        assert_eq!(cursor_message_text(&value), "I found it.");
    }

    #[test]
    fn extracts_tag_body() {
        assert_eq!(
            extract_tag("prefix <user_query>hello</user_query> suffix", "user_query"),
            Some("hello".to_string())
        );
    }

    #[test]
    fn skips_empty_window_workspace() {
        assert!(decode_cursor_project_dir("empty-window").is_none());
    }

    #[test]
    fn decodes_hyphenated_workspace_paths_on_any_platform() {
        let temp = tempdir().expect("tempdir");
        let workspace = temp.path().join("my-app").join("sub").join("deep-er");
        fs::create_dir_all(&workspace).expect("create workspace");
        fs::create_dir_all(temp.path().join("my")).expect("decoy dir");
        let encoded = workspace
            .to_string_lossy()
            .trim_start_matches('/')
            .replace('/', "-");

        assert_eq!(decode_cursor_project_dir(&encoded), Some(workspace));
        assert_eq!(
            decode_cursor_project_dir(&format!("{encoded}-missing")),
            None
        );

        // Directory names may themselves contain consecutive hyphens.
        let doubled = temp.path().join("review--x1").join("sub");
        fs::create_dir_all(&doubled).expect("create doubled-hyphen workspace");
        let encoded_doubled = doubled
            .to_string_lossy()
            .trim_start_matches('/')
            .replace('/', "-");
        assert_eq!(decode_cursor_project_dir(&encoded_doubled), Some(doubled));

        // A workspace directly under the filesystem root is a single component.
        assert_eq!(
            decode_cursor_project_dir("tmp"),
            Some(std::path::PathBuf::from("/tmp"))
        );
        assert_eq!(decode_cursor_project_dir(""), None);

        // Many hyphens no longer means exponentially many candidates.
        let long = format!("{}{}", encoded, "-x".repeat(40));
        assert_eq!(decode_cursor_project_dir(&long), None);
    }

    #[test]
    fn path_resolution_stops_when_the_probe_budget_runs_out() {
        let temp = tempdir().expect("tempdir");
        let workspace = temp.path().join("my-app").join("sub");
        fs::create_dir_all(&workspace).expect("create workspace");
        let encoded = workspace
            .to_string_lossy()
            .trim_start_matches('/')
            .replace('/', "-");
        let parts: Vec<&str> = encoded.split('-').collect();
        let root = std::path::Path::new("/");

        let mut budget = super::MAX_PATH_PROBES;
        assert_eq!(
            super::resolve_hyphenated(root, &parts, &mut budget),
            Some(workspace)
        );
        assert!(budget < super::MAX_PATH_PROBES);

        let mut tiny = 3;
        assert_eq!(super::resolve_hyphenated(root, &parts, &mut tiny), None);
        assert_eq!(tiny, 0);
    }

    #[test]
    fn overlapping_hyphenated_directories_exhaust_the_budget() {
        // Every level holds both `a` and `a-a`, so each way of splitting the
        // name is a real directory and a dead-end lookup would check them all.
        let temp = tempdir().expect("tempdir");
        fn build(dir: &std::path::Path, depth: usize) {
            if depth == 0 {
                return;
            }
            for name in ["a", "a-a"] {
                let child = dir.join(name);
                fs::create_dir_all(&child).expect("create dir");
                build(&child, depth - 1);
            }
        }
        build(temp.path(), 8);
        let encoded = format!(
            "{}{}-missing",
            temp.path()
                .to_string_lossy()
                .trim_start_matches('/')
                .replace('/', "-"),
            "-a".repeat(30)
        );
        let parts: Vec<&str> = encoded.split('-').collect();

        let mut budget = super::MAX_PATH_PROBES;
        let found = super::resolve_hyphenated(std::path::Path::new("/"), &parts, &mut budget);

        assert_eq!(found, None);
        assert_eq!(budget, 0, "search should stop at the probe budget");
    }
}
