//! `sessiongrep collect`: dump full sessions as JSONL for router research.
//!
//! One record per transcript line. `raw` is the untouched line (prompts, replies,
//! tool calls, tool output). `model` and `usage` are lifted out so cost analysis
//! needs no provider-specific parsing.

use std::collections::HashMap;
use std::fs;
use std::io::Write;

use anyhow::Result;
use serde_json::{json, Value};

use crate::models::{Provider, SourceFile};

pub fn collect_file(source: &SourceFile, out: &mut impl Write) -> Result<usize> {
    let raw = fs::read_to_string(&source.path)?;
    let lines: Vec<Value> = raw
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let session_id = source
        .path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("unknown");
    let last_usage = last_usage_by_message(source.provider, &lines);
    let mut model: Option<String> = None;
    let mut cwd: Option<String> = None;

    for (seq, line) in lines.iter().enumerate() {
        if let Some(found) = model_of(source.provider, line) {
            model = Some(found);
        }
        cwd = cwd.or_else(|| cwd_of(line));
        let usage = match source.provider {
            Provider::Claude => {
                claude_usage(line).filter(|_| last_usage.get(&message_id(line)) == Some(&seq))
            }
            Provider::Codex => codex_usage(line),
            _ => None,
        };
        let record = json!({
            "provider": source.provider,
            "session_id": session_id,
            "source_path": source.path,
            "cwd": cwd,
            "seq": seq,
            "timestamp": line.get("timestamp"),
            "model": model,
            "usage": usage,
            "raw": line,
        });
        serde_json::to_writer(&mut *out, &record)?;
        out.write_all(b"\n")?;
    }
    Ok(lines.len())
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
    use std::path::PathBuf;

    fn run(provider: Provider, body: &str) -> Vec<Value> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("abc.jsonl");
        fs::write(&path, body).unwrap();
        let source = SourceFile {
            provider,
            path: PathBuf::from(&path),
            mtime_ns: 0,
            size_bytes: 0,
        };
        let mut out = Vec::new();
        collect_file(&source, &mut out).unwrap();
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
}
