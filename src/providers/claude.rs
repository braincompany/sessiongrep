use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use ignore::WalkBuilder;
use serde_json::{json, Value};

use crate::models::{ParsedSession, Provider, SessionRecord, SourceFile};
use crate::util::{
    consists_of_tag_blocks, find_repo_root, format_transcript_line, minimal_record, normalize_path,
    parse_datetime, preview_from_text, substantive_text, truncate_for_display,
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
        let mut last_prompt = None;
        let mut custom_title = None;
        let mut ai_title = None;

        for line in raw.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(line) {
                Ok(value) => value,
                Err(_) => continue,
            };
            if let Some(session_id) = value.get("sessionId").and_then(Value::as_str) {
                provider_session_id = session_id.to_string();
            }
            match value.get("type").and_then(Value::as_str) {
                Some("last-prompt") => {
                    if let Some(prompt) = non_empty_str(&value, "lastPrompt") {
                        if substantive_text(&prompt) {
                            last_prompt = Some(prompt);
                        }
                    }
                }
                // Titles can change during a session (e.g. `/rename`); the last one wins.
                Some("custom-title") => {
                    custom_title = non_empty_str(&value, "customTitle").or(custom_title);
                }
                Some("ai-title") => ai_title = non_empty_str(&value, "aiTitle").or(ai_title),
                _ => {}
            }
            if cwd.is_none() {
                cwd = value
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
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
                text = message_text(message);
            } else if let Some(message) = value.get("content").and_then(Value::as_str) {
                text = message.to_string();
            }

            let text = strip_system_reminders(&text);
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
        // Prefer Claude Code's own session names, then what the session started with.
        // The preview shows where the session left off instead.
        let title = custom_title
            .or(ai_title)
            .or_else(|| first_user.clone())
            .or_else(|| last_prompt.clone())
            .map(|text| truncate_for_display(&text, 100));
        let preview = last_prompt
            .clone()
            .or_else(|| last_user.clone())
            .or_else(|| first_user.clone())
            .map(|text| preview_from_text(&text))
            .unwrap_or_else(|| "(no preview available)".to_string());
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
            preview_text: preview,
            source_path: normalize_path(path),
            message_count: Some(messages.len() as i64),
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

/// Conversational text of a Claude message: the string content, or the `text`
/// blocks of block content. Tool calls, thinking, images, and tool results are
/// dropped so file dumps and command output don't masquerade as user turns;
/// the exception is tool results that carry what the user typed or chose.
fn message_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.trim().to_string(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| match block.get("type").and_then(Value::as_str) {
                Some("text") => block
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                Some("tool_result") => user_input_in_tool_result(block),
                _ => None,
            })
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Answers to the agent's questions and feedback given when rejecting a tool
/// call arrive as tool results, but the user wrote or picked them.
fn user_input_in_tool_result(block: &Value) -> Option<String> {
    let text = match block.get("content")? {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    const ANSWER_PREFIXES: &[&str] = &[
        "Your questions have been answered:",
        "User has answered your questions:",
    ];
    if ANSWER_PREFIXES
        .iter()
        .any(|prefix| text.starts_with(prefix))
    {
        return Some(text);
    }
    // Only inside Claude Code's rejection message: tool output (e.g. a file
    // read) can contain "the user said:" anywhere.
    if !text.starts_with("The user doesn't want to proceed with this tool use.") {
        return None;
    }
    let (_, feedback) = text.split_once("the user said:")?;
    Some(feedback.trim().to_string())
}

fn non_empty_str(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(ToOwned::to_owned)
}

/// Remove `<system-reminder>...</system-reminder>` blocks, which Claude Code
/// injects at the start of a message or line. A tag mentioned mid-sentence, or
/// an opening tag with no closing tag, is ordinary text and is kept.
fn strip_system_reminders(text: &str) -> String {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = find_at_line_start(rest, OPEN) {
        let Some(len) = rest[start..].find(CLOSE) else {
            break;
        };
        out.push_str(&rest[..start]);
        rest = &rest[start + len + CLOSE.len()..];
    }
    out.push_str(rest);
    out
}

/// Byte offset of the first `needle` preceded only by whitespace on its line.
fn find_at_line_start(text: &str, needle: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(found) = text[from..].find(needle) {
        let pos = from + found;
        let line_start = text[..pos].rfind('\n').map_or(0, |newline| newline + 1);
        if text[line_start..pos].trim().is_empty() {
            return Some(pos);
        }
        from = pos + needle.len();
    }
    None
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

/// Tags wrapping output of local commands and background tasks, which Claude
/// Code records as user messages even though the user didn't type them.
const GENERATED_USER_TAGS: &[&str] = &[
    "local-command-stdout",
    "local-command-stderr",
    "bash-stdout",
    "bash-stderr",
    "task-notification",
];

fn should_skip_message(value: &Value, text: &str) -> bool {
    let normalized = text.trim();
    let flag = |key: &str| value.get(key).and_then(Value::as_bool).unwrap_or(false);
    // Injected by Claude Code rather than typed: skill bodies, local command
    // caveats, and compaction summaries of earlier turns.
    if flag("isMeta") || flag("isCompactSummary") {
        return true;
    }
    // Only messages made entirely of these blocks; a prompt that merely
    // starts with such a tag (e.g. asking what one means) is the user's.
    if consists_of_tag_blocks(normalized, GENERATED_USER_TAGS) {
        return true;
    }
    // Skip slash command invocations that carry no args — pure UI bookkeeping.
    // Invocations with args (e.g. `/brutal-review <url>`) pass through; strip_command_markup
    // then reduces them to just the args text.
    let is_command_bookkeeping = (normalized.contains("<command-name>")
        && tag_content(normalized, "command-args").trim().is_empty())
        || normalized.eq_ignore_ascii_case("resume cancelled");

    is_command_bookkeeping
}

#[cfg(test)]
mod tests {
    use super::should_skip_message;
    use serde_json::json;

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
        for cmd in &[
            "/exit", "/resume", "/clear", "/compact", "/mcp", "/config", "/help",
        ] {
            let text = format!("<command-name>{cmd}</command-name><command-message>{cmd}</command-message><command-args></command-args>");
            let value = json!({ "isMeta": false });
            assert!(
                should_skip_message(&value, &text),
                "should skip {cmd} (no args)"
            );
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
        assert_eq!(
            super::strip_command_markup(text),
            "https://example.com/pr/1"
        );
    }

    #[test]
    fn strip_command_markup_leaves_normal_messages() {
        assert_eq!(
            super::strip_command_markup("fix the bug in db.rs"),
            "fix the bug in db.rs"
        );
    }

    fn parse_fixture(lines: &[serde_json::Value]) -> crate::models::ParsedSession {
        let temp = tempfile::tempdir().unwrap();
        let path = temp
            .path()
            .join("11111111-2222-3333-4444-555555555555.jsonl");
        let body = lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, body).unwrap();
        let adapter = super::ClaudeAdapter::new(vec![temp.path().to_path_buf()]);
        let sources = adapter.discover();
        assert_eq!(sources.len(), 1);
        adapter.parse(&sources[0])
    }

    #[test]
    fn transcript_keeps_conversation_and_drops_tool_payloads_and_injected_text() {
        let ts = "2026-09-01T10:00:00Z";
        let parsed = parse_fixture(&[
            json!({"type": "user", "timestamp": ts, "cwd": "/tmp/demo", "sessionId": "s1",
                   "message": {"role": "user", "content":
                       "<system-reminder>internal note</system-reminder>\nWhy does login fail?"}}),
            json!({"type": "assistant", "timestamp": ts, "message": {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "private reasoning"},
                {"type": "text", "text": "Let me read the handler."},
                {"type": "tool_use", "name": "Write", "input": {"file_path": "a.rs", "content": "WRITTEN FILE BODY"}}
            ]}}),
            json!({"type": "user", "timestamp": ts, "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "TOOL OUTPUT DUMP"}
            ]}}),
            json!({"type": "user", "timestamp": ts, "isMeta": true, "message": {"role": "user",
                   "content": "Base directory for this skill: /skills/x SKILL BODY"}}),
            json!({"type": "user", "timestamp": ts, "message": {"role": "user",
                   "content": "<local-command-stdout>Total cost: $1.23</local-command-stdout>"}}),
            json!({"type": "user", "timestamp": ts, "isCompactSummary": true, "message": {"role": "user",
                   "content": "This session is being continued from a previous conversation."}}),
            json!({"type": "assistant", "timestamp": ts, "message": {"role": "assistant", "content": [
                {"type": "text", "text": "The token expiry check is inverted."}
            ]}}),
            json!({"type": "user", "timestamp": ts, "message": {"role": "user", "content": "Fix it please"}}),
        ]);

        let transcript = &parsed.transcript_text;
        for expected in [
            "Why does login fail?",
            "Let me read the handler.",
            "The token expiry check is inverted.",
            "Fix it please",
        ] {
            assert!(
                transcript.contains(expected),
                "missing {expected:?} in:\n{transcript}"
            );
        }
        for unexpected in [
            "internal note",
            "private reasoning",
            "WRITTEN FILE BODY",
            "TOOL OUTPUT DUMP",
            "SKILL BODY",
            "Total cost",
            "continued from a previous",
        ] {
            assert!(
                !transcript.contains(unexpected),
                "leaked {unexpected:?} into:\n{transcript}"
            );
        }
        assert_eq!(parsed.session.message_count, Some(4));
        assert_eq!(
            parsed.session.title.as_deref(),
            Some("Why does login fail?")
        );
        assert_eq!(parsed.session.preview_text, "Fix it please");
    }

    #[test]
    fn system_reminder_mentions_in_prose_are_kept() {
        assert_eq!(
            super::strip_system_reminders("Fix it<system-reminder>x</system-reminder>"),
            "Fix it<system-reminder>x</system-reminder>"
        );
        assert_eq!(
            super::strip_system_reminders(
                "Fix it\n<system-reminder>\nnote\n</system-reminder>\nthanks"
            ),
            "Fix it\n\nthanks"
        );
        assert_eq!(
            super::strip_system_reminders("<system-reminder>a</system-reminder>Real prompt"),
            "Real prompt"
        );
        let prose = "Why does sessiongrep index <system-reminder> blocks? It should drop them";
        assert_eq!(super::strip_system_reminders(prose), prose);
        let unclosed = "<system-reminder> is the tag Claude Code uses";
        assert_eq!(super::strip_system_reminders(unclosed), unclosed);
    }

    #[test]
    fn answers_and_rejection_feedback_in_tool_results_are_kept() {
        let ts = "2026-09-01T10:00:00Z";
        let parsed = parse_fixture(&[
            json!({"type": "user", "timestamp": ts, "message": {"role": "user", "content": "Pin litellm"}}),
            json!({"type": "user", "timestamp": ts, "message": {"role": "user", "content": [
                {"type": "tool_result", "content": "Your questions have been answered: \"How to pin?\"=\"Stage now\"."}
            ]}}),
            json!({"type": "user", "timestamp": ts, "message": {"role": "user", "content": [
                {"type": "tool_result", "content": [{"type": "text", "text":
                    "The user doesn't want to proceed with this tool use. The tool use was rejected (eg. if it was a file edit, the new_string was NOT written to the file). To tell you how to proceed, the user said:\nuse the stable release"}]}
            ]}}),
            json!({"type": "user", "timestamp": ts, "message": {"role": "user", "content": [
                {"type": "tool_result", "content": "     1\tlet marker = \"the user said:\";\n     2\tFILE_PAYLOAD"}
            ]}}),
            json!({"type": "user", "timestamp": ts, "message": {"role": "user", "content": [
                {"type": "tool_result", "content": "The user doesn't want to proceed with this tool use."}
            ]}}),
        ]);

        let transcript = &parsed.transcript_text;
        assert!(
            transcript.contains("\"How to pin?\"=\"Stage now\""),
            "{transcript}"
        );
        assert!(
            transcript.contains("use the stable release"),
            "{transcript}"
        );
        assert!(
            !transcript.contains("doesn't want to proceed"),
            "{transcript}"
        );
        assert!(
            !transcript.contains("To tell you how to proceed"),
            "{transcript}"
        );
        assert!(!transcript.contains("FILE_PAYLOAD"), "{transcript}");
    }

    #[test]
    fn prompts_that_only_start_with_a_generated_tag_are_kept() {
        let value = json!({});
        assert!(should_skip_message(
            &value,
            "<bash-stdout>ok</bash-stdout><bash-stderr></bash-stderr>"
        ));
        assert!(!should_skip_message(
            &value,
            "<bash-stdout> is what tag? explain"
        ));
        assert!(!should_skip_message(
            &value,
            "<local-command-stdout>x</local-command-stdout>\nwhy did /cost print this?"
        ));
    }

    #[test]
    fn titles_prefer_custom_then_ai_titles() {
        let prompt = json!({"type": "user", "timestamp": "2026-09-01T10:00:00Z", "sessionId": "s1",
                            "message": {"role": "user", "content": "first prompt"}});
        let ai = json!({"type": "ai-title", "sessionId": "s1", "aiTitle": "Generated title"});
        let custom =
            |title: &str| json!({"type": "custom-title", "sessionId": "s1", "customTitle": title});

        let parsed = parse_fixture(&[prompt.clone(), ai.clone()]);
        assert_eq!(parsed.session.title.as_deref(), Some("Generated title"));

        let parsed = parse_fixture(&[prompt, custom("Old name"), ai, custom("Renamed")]);
        assert_eq!(parsed.session.title.as_deref(), Some("Renamed"));
    }
}
