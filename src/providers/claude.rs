use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use ignore::WalkBuilder;
use serde_json::{Value, json};

use crate::models::{ParsedSession, Provider, SessionRecord, SourceFile};
use crate::util::{
    extract_text, find_repo_root, format_transcript_line, last_exchange, minimal_record,
    normalize_path, parse_datetime, preview_from_text, substantive_text, truncate_for_display,
};

pub struct ClaudeAdapter {
    roots: Vec<PathBuf>,
}

impl ClaudeAdapter {
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
                if path.components().any(|component| {
                    let value = component.as_os_str();
                    value == "memory" || value == "subagents"
                }) {
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
                        provider: Provider::Claude,
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
            Err(err) => minimal_record(Provider::Claude, &source.path, err.to_string()),
        }
    }

    fn parse_inner(&self, path: &Path) -> Result<ParsedSession> {
        let raw = fs::read_to_string(path)?;
        let mut provider_session_id = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("unknown")
            .to_string();
        let mut cwd = None;
        let mut created_at: Option<DateTime<Utc>> = None;
        let mut updated_at: Option<DateTime<Utc>> = None;
        let mut messages = Vec::new();
        let mut transcript_lines = Vec::new();
        let mut raw_meta = Vec::new();
        let mut last_prompt = None;
        let mut git_branch: Option<String> = None;

        for line in raw.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(line) {
                Ok(value) => value,
                Err(_) => continue,
            };
            raw_meta.push(value.clone());
            if let Some(session_id) = value.get("sessionId").and_then(Value::as_str) {
                provider_session_id = session_id.to_string();
            }
            if value.get("type").and_then(Value::as_str) == Some("last-prompt") {
                if let Some(prompt) = value.get("lastPrompt").and_then(Value::as_str) {
                    let prompt = prompt.trim();
                    if substantive_text(prompt) {
                        last_prompt = Some(prompt.to_string());
                    }
                }
            }
            if cwd.is_none() {
                cwd = value
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
            }
            // Take the newest branch seen: a session can outlive a checkout, and the branch
            // it ended on is the one that identifies the work.
            if let Some(branch) = value.get("gitBranch").and_then(Value::as_str) {
                if !branch.is_empty() {
                    git_branch = Some(branch.to_string());
                }
            }

            // Subagent turns are a side conversation. Folding them in makes a delegated
            // task's output look like the agent's reply to the user.
            if value
                .get("isSidechain")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                continue;
            }

            let timestamp = value
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(parse_datetime);

            let mut role = value
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_string);
            let mut text = String::new();

            if let Some(message) = value.get("message") {
                role = message
                    .get("role")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or(role);
                text = extract_text(message);
            } else if let Some(message) = value.get("content").and_then(Value::as_str) {
                text = message.to_string();
            }

            if should_skip_message(&value, &text) {
                continue;
            }
            let text = strip_command_markup(&text);

            match role.as_deref() {
                Some("user") | Some("assistant") => {
                    let text = text.trim().to_string();
                    if text.is_empty() {
                        continue;
                    }
                    if created_at.is_none() {
                        created_at = timestamp;
                    }
                    updated_at = timestamp.or(updated_at);
                    messages.push((role.unwrap_or_default(), text.clone(), timestamp));
                    transcript_lines.push(format_transcript_line(
                        messages
                            .last()
                            .map(|(role, _, _)| role.as_str())
                            .unwrap_or("message"),
                        timestamp,
                        &text,
                    ));
                }
                _ => {}
            }
        }

        let first_user = messages
            .iter()
            .find(|(role, text, _)| role == "user" && substantive_text(text))
            .map(|(_, text, _)| text.clone());
        let last_user = messages
            .iter()
            .rev()
            .find(|(role, text, _)| role == "user" && substantive_text(text))
            .map(|(_, text, _)| text.clone());
        let title = last_prompt
            .clone()
            .or_else(|| last_user.clone())
            .or_else(|| first_user.clone())
            .clone()
            .map(|text| truncate_for_display(&text, 100));
        let preview = last_prompt
            .clone()
            .or_else(|| last_user.clone())
            .or_else(|| first_user.clone())
            .map(|text| preview_from_text(&text))
            .unwrap_or_else(|| "(no preview available)".to_string());
        let (last_user_message_at, last_assistant_message_at, last_assistant_text) =
            last_exchange(&messages);
        let repo_root = cwd.as_deref().and_then(find_repo_root);
        let raw_metadata_json = Some(serde_json::to_string(&json!({
            "line_count": raw.lines().count(),
            "session_path": normalize_path(path),
        }))?);

        let session = SessionRecord {
            id: format!("claude:{provider_session_id}"),
            provider: Provider::Claude,
            provider_session_id,
            title,
            summary: first_user.map(|text| truncate_for_display(&text, 180)),
            cwd,
            repo_root,
            created_at,
            updated_at,
            last_message_at: updated_at,
            git_branch,
            last_user_message_at,
            last_assistant_message_at,
            last_assistant_text: last_assistant_text.map(|text| truncate_for_display(&text, 4000)),
            preview_text: preview,
            source_path: normalize_path(path),
            message_count: Some(messages.len() as i64),
            // v2 adds git_branch, the role-split timestamps, last_assistant_text, and the
            // isSidechain filter. Bumped so existing indexes reparse instead of serving rows
            // that silently lack the new fields.
            parse_version: "claude-v2".to_string(),
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

fn tag_content<'a>(text: &'a str, tag: &str) -> &'a str {
    let open = &format!("<{tag}>");
    let close = &format!("</{tag}>");
    text.find(open.as_str())
        .map(|i| &text[i + open.len()..])
        .and_then(|s| s.find(close.as_str()).map(|j| &s[..j]))
        .unwrap_or("")
}

/// For slash command invocations, keep only the args text; leave other messages unchanged.
fn strip_command_markup(text: &str) -> String {
    if !text.contains("<command-name>") {
        return text.to_string();
    }
    tag_content(text, "command-args").trim().to_string()
}

fn should_skip_message(value: &Value, text: &str) -> bool {
    let normalized = text.trim();
    let is_meta = value
        .get("isMeta")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let is_local_command_caveat = is_meta
        && (normalized.contains("<local-command-caveat>")
            || normalized.contains(
                "Caveat: The messages below were generated by the user while running local commands.",
            ));
    // Skip slash command invocations that carry no args — pure UI bookkeeping.
    // Invocations with args (e.g. `/brutal-review <url>`) pass through; strip_command_markup
    // then reduces them to just the args text.
    let is_command_bookkeeping = (normalized.contains("<command-name>")
        && tag_content(normalized, "command-args").trim().is_empty())
        || normalized.eq_ignore_ascii_case("resume cancelled");

    is_local_command_caveat || is_command_bookkeeping
}


#[cfg(test)]
mod tests {
    use super::should_skip_message;
    use crate::models::Provider;
    use crate::models::SourceFile;
    use serde_json::json;
    use std::fs;
    use tempfile::tempdir;

    /// Parse a synthetic transcript through the real adapter.
    fn parse_lines(lines: &str) -> crate::models::SessionRecord {
        let temp = tempdir().expect("tempdir");
        let path = temp
            .path()
            .join("11111111-2222-3333-4444-555555555555.jsonl");
        fs::write(&path, lines).expect("write transcript");
        let adapter = super::ClaudeAdapter::new(vec![temp.path().to_path_buf()]);
        adapter
            .parse(&SourceFile {
                provider: Provider::Claude,
                path,
                mtime_ns: 0,
                size_bytes: lines.len() as i64,
            })
            .session
    }

    #[test]
    fn records_branch_and_role_split_timestamps() {
        let session = parse_lines(
            r#"{"type":"user","timestamp":"2026-01-01T10:00:00Z","gitBranch":"feat/towers","cwd":"/repo","message":{"role":"user","content":"first ask"}}
{"type":"assistant","timestamp":"2026-01-01T10:01:00Z","gitBranch":"feat/towers","message":{"role":"assistant","content":[{"type":"text","text":"first answer"}]}}
{"type":"user","timestamp":"2026-01-01T10:05:00Z","gitBranch":"feat/towers","message":{"role":"user","content":"second ask"}}
{"type":"assistant","timestamp":"2026-01-01T10:09:00Z","gitBranch":"feat/towers","message":{"role":"assistant","content":[{"type":"text","text":"second answer"}]}}
"#,
        );
        assert_eq!(session.git_branch.as_deref(), Some("feat/towers"));
        assert_eq!(
            session.last_user_message_at.map(|t| t.to_rfc3339()),
            Some("2026-01-01T10:05:00+00:00".to_string())
        );
        assert_eq!(
            session.last_assistant_message_at.map(|t| t.to_rfc3339()),
            Some("2026-01-01T10:09:00+00:00".to_string())
        );
        assert_eq!(
            session.last_assistant_text.as_deref(),
            Some("second answer")
        );
        assert_eq!(session.parse_version, "claude-v2");
    }

    #[test]
    fn last_branch_wins_when_the_session_switches_checkout() {
        let session = parse_lines(
            r#"{"type":"user","timestamp":"2026-01-01T10:00:00Z","gitBranch":"main","message":{"role":"user","content":"start"}}
{"type":"assistant","timestamp":"2026-01-01T10:01:00Z","gitBranch":"feat/later","message":{"role":"assistant","content":[{"type":"text","text":"done"}]}}
"#,
        );
        assert_eq!(session.git_branch.as_deref(), Some("feat/later"));
    }

    #[test]
    fn subagent_turns_do_not_become_the_agents_reply() {
        // The sidechain reply is both last in the file and newest. Without filtering it
        // would be reported as what the agent told the user.
        let session = parse_lines(
            r#"{"type":"user","timestamp":"2026-01-01T10:00:00Z","message":{"role":"user","content":"delegate this"}}
{"type":"assistant","timestamp":"2026-01-01T10:01:00Z","message":{"role":"assistant","content":[{"type":"text","text":"main answer"}]}}
{"type":"user","timestamp":"2026-01-01T10:02:00Z","isSidechain":true,"message":{"role":"user","content":"subagent prompt"}}
{"type":"assistant","timestamp":"2026-01-01T10:03:00Z","isSidechain":true,"message":{"role":"assistant","content":[{"type":"text","text":"subagent output"}]}}
"#,
        );
        assert_eq!(session.last_assistant_text.as_deref(), Some("main answer"));
        assert_eq!(
            session.last_assistant_message_at.map(|t| t.to_rfc3339()),
            Some("2026-01-01T10:01:00+00:00".to_string())
        );
        assert_eq!(
            session.last_user_message_at.map(|t| t.to_rfc3339()),
            Some("2026-01-01T10:00:00+00:00".to_string()),
            "sidechain prompts are not the user's last message either"
        );
        assert_eq!(session.message_count, Some(2));
    }

    #[test]
    fn unanswered_prompt_leaves_the_assistant_side_empty() {
        // Ctrl-C or a crash mid-turn: the gap this exposes is the point of the split.
        let session = parse_lines(
            r#"{"type":"user","timestamp":"2026-01-01T10:00:00Z","message":{"role":"user","content":"are you there"}}
"#,
        );
        assert!(session.last_user_message_at.is_some());
        assert!(session.last_assistant_message_at.is_none());
        assert!(session.last_assistant_text.is_none());
    }

    #[test]
    fn skips_local_command_caveat_meta_messages() {
        let value = json!({
            "isMeta": true,
            "message": {
                "role": "user",
                "content": "<local-command-caveat>Caveat: The messages below were generated by the user while running local commands.</local-command-caveat>"
            }
        });
        let text = "<local-command-caveat>Caveat: The messages below were generated by the user while running local commands.</local-command-caveat>";
        assert!(should_skip_message(&value, text));
    }

    #[test]
    fn keeps_normal_user_messages() {
        let value = json!({
            "isMeta": false,
            "message": {
                "role": "user",
                "content": "real prompt"
            }
        });
        assert!(!should_skip_message(&value, "real prompt"));
    }

    #[test]
    fn skips_no_arg_slash_commands() {
        for cmd in &["/exit", "/resume", "/clear", "/compact", "/mcp", "/config", "/help"] {
            let text = format!("<command-name>{cmd}</command-name><command-message>{cmd}</command-message><command-args></command-args>");
            let value = json!({ "isMeta": false });
            assert!(should_skip_message(&value, &text), "should skip {cmd} (no args)");
        }
    }

    #[test]
    fn keeps_slash_commands_with_args() {
        let text = "<command-name>/brutal-review</command-name><command-message>brutal-review</command-message><command-args>https://github.com/braincompany/sessiongrep/pull/15</command-args>";
        let value = json!({ "isMeta": false });
        assert!(!should_skip_message(&value, text));
    }

    #[test]
    fn strip_command_markup_extracts_args() {
        let text = "<command-name>/brutal-review</command-name><command-message>brutal-review</command-message><command-args>https://example.com/pr/1</command-args>";
        assert_eq!(super::strip_command_markup(text), "https://example.com/pr/1");
    }

    #[test]
    fn strip_command_markup_leaves_normal_messages() {
        assert_eq!(super::strip_command_markup("fix the bug in db.rs"), "fix the bug in db.rs");
    }
}
