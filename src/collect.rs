//! `sessiongrep collect`: dump full sessions as JSONL for router research.
//!
//! One record per transcript line. `raw` is the untouched line (prompts, replies,
//! tool calls, tool output). `model` and `usage` are lifted out so cost analysis
//! needs no provider-specific parsing. Strings in `raw` pass through
//! best-effort secret redaction.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::LazyLock;

use anyhow::Result;
use regex::{Captures, Regex};
use serde_json::{json, Value};

use crate::models::Provider;

pub fn collect_file(
    provider: Provider,
    session_id: &str,
    path: &Path,
    out: &mut impl Write,
) -> Result<usize> {
    let raw = fs::read_to_string(path)?;
    let lines: Vec<Value> = raw
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let last_usage = last_usage_by_message(provider, &lines);
    let mut model: Option<String> = None;
    let mut cwd: Option<String> = None;

    for (seq, line) in lines.iter().enumerate() {
        if let Some(found) = model_of(provider, line) {
            model = Some(found);
        }
        cwd = cwd.or_else(|| cwd_of(line));
        let usage = match provider {
            Provider::Claude => {
                claude_usage(line).filter(|_| last_usage.get(&message_id(line)) == Some(&seq))
            }
            Provider::Codex => codex_usage(line),
            _ => None,
        };
        let mut raw = line.clone();
        redact(&mut raw);
        let record = json!({
            "provider": provider,
            "session_id": session_id,
            "source_path": path,
            "cwd": cwd,
            "seq": seq,
            "timestamp": line.get("timestamp"),
            "model": model,
            "usage": usage,
            "raw": raw,
        });
        serde_json::to_writer(&mut *out, &record)?;
        out.write_all(b"\n")?;
    }
    Ok(lines.len())
}

/// Known token formats, replaced outright.
static SECRET_PATTERNS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
        r"|\bAKIA[0-9A-Z]{16}\b",
        r"|\b(?:gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{30,})",
        r"|\bsk-[A-Za-z0-9_-]{20,}",
        r"|\bxox[abprs]-[A-Za-z0-9-]{10,}",
        r"|\bAIza[0-9A-Za-z_-]{35}",
        r"|\beyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}",
        r"|(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{20,}",
    ))
    .expect("valid regex")
});

/// `NAME_SECRET=value` style assignments. The value is only redacted when it
/// looks random (long, letters and digits), so code like `token = next()` survives.
static SECRET_ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)([\w-]*(?:api[_-]?key|secret|token|passw(?:or)?d|access[_-]?key)[\w-]*["']?\s*[:=]\s*["']?)([A-Za-z0-9_\-./+=]{16,})"#,
    )
    .expect("valid regex")
});

/// JSON field names that hold secrets, e.g. `{"api_key": "..."}`.
static SECRET_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)api[_-]?key|secret|token|passw(?:or)?d|access[_-]?key").expect("valid regex")
});

const REDACTED: &str = "[REDACTED]";

fn looks_random(value: &str) -> bool {
    value.len() >= 16
        && value.chars().any(|c| c.is_ascii_digit())
        && value.chars().any(|c| c.is_ascii_alphabetic())
}

fn redact(value: &mut Value) {
    match value {
        Value::String(text) => *text = redact_text(text),
        Value::Array(items) => items.iter_mut().for_each(redact),
        Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                match value {
                    Value::String(text) if SECRET_KEY.is_match(key) && looks_random(text) => {
                        *text = REDACTED.to_string();
                    }
                    _ => redact(value),
                }
            }
        }
        _ => {}
    }
}

fn redact_text(text: &str) -> String {
    let text = SECRET_PATTERNS.replace_all(text, REDACTED);
    SECRET_ASSIGNMENT
        .replace_all(&text, |caps: &Captures| {
            if looks_random(&caps[2]) {
                format!("{}{REDACTED}", &caps[1])
            } else {
                caps[0].to_string()
            }
        })
        .into_owned()
}

fn message_id(line: &Value) -> String {
    line.pointer("/message/id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Claude logs one assistant message as several lines that all repeat `usage`.
/// Only the last line of each message keeps it, so summing never double counts.
fn last_usage_by_message(provider: Provider, lines: &[Value]) -> HashMap<String, usize> {
    let mut last = HashMap::new();
    if provider == Provider::Claude {
        for (seq, line) in lines.iter().enumerate() {
            if claude_usage(line).is_some() {
                last.insert(message_id(line), seq);
            }
        }
    }
    last
}

fn model_of(provider: Provider, line: &Value) -> Option<String> {
    let pointer = match provider {
        Provider::Claude => "/message/model",
        _ => "/payload/model",
    };
    line.pointer(pointer)
        .and_then(Value::as_str)
        .filter(|model| *model != "<synthetic>")
        .map(str::to_string)
}

fn cwd_of(line: &Value) -> Option<String> {
    line.get("cwd")
        .or_else(|| line.pointer("/payload/cwd"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn count(value: &Value, key: &str) -> i64 {
    value.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn claude_usage(line: &Value) -> Option<Value> {
    let usage = line.pointer("/message/usage")?;
    Some(json!({
        "input": count(usage, "input_tokens"),
        "cache_write": count(usage, "cache_creation_input_tokens"),
        "cache_read": count(usage, "cache_read_input_tokens"),
        "output": count(usage, "output_tokens"),
    }))
}

/// Codex reports `input_tokens` including the cached part; split it so the
/// fields mean the same as Claude's.
fn codex_usage(line: &Value) -> Option<Value> {
    if line.pointer("/payload/type").and_then(Value::as_str) != Some("token_count") {
        return None;
    }
    let usage = line.pointer("/payload/info/last_token_usage")?;
    let cached = count(usage, "cached_input_tokens");
    Some(json!({
        "input": count(usage, "input_tokens") - cached,
        "cache_write": 0,
        "cache_read": cached,
        "output": count(usage, "output_tokens"),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(provider: Provider, body: &str) -> Vec<Value> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("abc.jsonl");
        fs::write(&path, body).unwrap();
        let mut out = Vec::new();
        collect_file(provider, "abc", &path, &mut out).unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn claude_usage_counted_once_per_message() {
        let body = r#"{"cwd":"/r","message":{"id":"m1","model":"claude-x","usage":{"input_tokens":1,"cache_read_input_tokens":5,"output_tokens":2}}}
{"message":{"id":"m1","model":"claude-x","usage":{"input_tokens":1,"cache_read_input_tokens":5,"output_tokens":9}}}"#;
        let records = run(Provider::Claude, body);
        assert!(records[0]["usage"].is_null());
        assert_eq!(records[1]["usage"]["output"], 9);
        assert_eq!(records[1]["model"], "claude-x");
        assert_eq!(records[1]["cwd"], "/r");
    }

    #[test]
    fn codex_cached_tokens_split_out_and_model_carried() {
        let body = r#"{"type":"turn_context","payload":{"model":"gpt-x","cwd":"/r"}}
{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":80,"output_tokens":7}}}}"#;
        let records = run(Provider::Codex, body);
        assert_eq!(records[1]["usage"]["input"], 20);
        assert_eq!(records[1]["usage"]["cache_read"], 80);
        assert_eq!(records[1]["model"], "gpt-x");
    }

    #[test]
    fn secrets_redacted_but_code_kept() {
        let text = "key ghp_abcdefghijklmnopqrstuvwxyz0123456789 and\nAPI_KEY=a1b2c3d4e5f6g7h8i9j0\nlet token = next_token_value();";
        let redacted = redact_text(text);
        assert!(!redacted.contains("ghp_"));
        assert!(redacted.contains("API_KEY=[REDACTED]"));
        assert!(redacted.contains("let token = next_token_value();"));
    }

    #[test]
    fn json_secret_fields_redacted_by_key_name() {
        let mut value = json!({"api_key": "a1b2c3d4e5f6g7h8i9j0", "note": "a1b2c3d4e5f6g7h8i9j0", "token": "short"});
        redact(&mut value);
        assert_eq!(value["api_key"], REDACTED);
        assert_eq!(value["note"], "a1b2c3d4e5f6g7h8i9j0");
        assert_eq!(value["token"], "short");
    }
}
