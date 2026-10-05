use std::cell::Cell;
use std::io::{self, BufRead, Write};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use sessiongrep::config::Config;
use sessiongrep::db::Db;
use sessiongrep::indexer;
use sessiongrep::models::{Provider, SearchFilters};
use sessiongrep::timeline;
use sessiongrep::util::{current_repo, resume_plan, truncate_for_display};

/// Minimum gap between incremental reindexes triggered by MCP tool calls.
/// Agents often burst-call us (search → get → search again); the throttle
/// keeps that cheap while still surfacing new sessions promptly. The
/// incremental scan itself is dominated by `stat()` calls and is fast when
/// nothing has changed, so this is mostly a guard against pathological bursts.
const MIN_REINDEX_INTERVAL: Duration = Duration::from_millis(1500);

fn main() {
    let config = Config::load().expect("failed to load config");
    let db = Db::open(&config.db_path()).expect("failed to open database");

    // Eagerly bring the index up to date on startup so the first tool call
    // doesn't pay for whatever the user has appended since the last CLI run.
    // Errors are logged but non-fatal: a stale index is still useful.
    if let Err(err) = indexer::reindex(&config, &db, false, None) {
        eprintln!("sessiongrep-mcp: startup reindex failed: {err:#}");
    }
    let last_reindex: Cell<Instant> = Cell::new(Instant::now());

    let stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();

    for line in stdin.lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => break,
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        let (id, method, params) = match parse_incoming(&line) {
            Incoming::Request { id, method, params } => (id, method, params),
            Incoming::Ignore => continue,
            Incoming::Reject(response) => {
                write_message(&mut stdout, &response);
                continue;
            }
        };

        let response = match method.as_str() {
            "initialize" => handle_initialize(Some(id)),
            "tools/list" => handle_tools_list(Some(id)),
            "tools/call" => {
                maybe_reindex(&config, &db, &last_reindex);
                handle_tools_call(Some(id), &params, &config, &db)
            }
            "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
            _ => error_response(id, -32601, &format!("unknown method: {method}")),
        };
        write_message(&mut stdout, &response);
    }
}

/// One line from the client, classified per JSON-RPC 2.0.
enum Incoming {
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// Notifications (no `id`) and responses: never answered.
    Ignore,
    /// Unparseable or malformed requests, answered with this error.
    Reject(Value),
}

fn parse_incoming(line: &str) -> Incoming {
    let message: Value = match serde_json::from_str(line) {
        Ok(message) => message,
        Err(err) => {
            return Incoming::Reject(error_response(
                Value::Null,
                -32700,
                &format!("parse error: {err}"),
            ))
        }
    };
    if !message.is_object() {
        // Includes JSON-RPC batches, which MCP's current protocol version dropped.
        return Incoming::Reject(error_response(
            Value::Null,
            -32600,
            "invalid request: expected a single JSON-RPC object",
        ));
    }
    let Some(id) = message.get("id").cloned() else {
        return Incoming::Ignore;
    };
    if message.get("result").is_some() || message.get("error").is_some() {
        return Incoming::Ignore;
    }
    match message.get("method").and_then(Value::as_str) {
        Some(method) => Incoming::Request {
            id,
            method: method.to_string(),
            params: message.get("params").cloned().unwrap_or(json!({})),
        },
        None => Incoming::Reject(error_response(
            id,
            -32600,
            "invalid request: missing method",
        )),
    }
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn write_message(stdout: &mut impl Write, message: &Value) {
    let out = serde_json::to_string(message).expect("failed to serialize response");
    let _ = writeln!(stdout, "{out}");
    let _ = stdout.flush();
}

/// Run an incremental reindex unless we already did one in the last
/// `MIN_REINDEX_INTERVAL`. Failures are logged to stderr and swallowed so a
/// transient filesystem issue can't take the MCP server down or break a tool
/// call that could otherwise have been served from the existing index.
fn maybe_reindex(config: &Config, db: &Db, last_reindex: &Cell<Instant>) {
    if last_reindex.get().elapsed() < MIN_REINDEX_INTERVAL {
        return;
    }
    if let Err(err) = indexer::reindex(config, db, false, None) {
        eprintln!("sessiongrep-mcp: reindex failed: {err:#}");
    }
    last_reindex.set(Instant::now());
}

fn handle_initialize(id: Option<Value>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {}
            },
            "serverInfo": {
                "name": "sessiongrep",
                "version": env!("CARGO_PKG_VERSION")
            }
        }
    })
}

fn handle_tools_list(id: Option<Value>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "tools": [
                {
                    "name": "search_sessions",
                    "description": "Search across all indexed AI coding sessions (Claude Code, Codex, Cursor, Antigravity, Pi) by keyword. Returns matching sessions ranked by relevance. Use this to find past work, conversations, or context from previous sessions.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "Search query (keywords, phrases, or code snippets)"
                            },
                            "provider": {
                                "type": "string",
                                "enum": ["claude", "codex", "cursor", "antigravity", "pi"],
                                "description": "Filter by provider (optional)"
                            },
                            "limit": {
                                "type": "integer",
                                "description": "Max results to return (default 10)",
                                "default": 10
                            }
                        },
                        "required": ["query"]
                    }
                },
                {
                    "name": "get_session",
                    "description": "Get the transcript and metadata for a session by its ID or ID prefix. Long transcripts are returned in pages (about 40k characters by default); the response says which lines it covers and what offset to request next.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "session_id": {
                                "type": "string",
                                "description": "Session ID or unique prefix (e.g. 'claude:abc123' or just 'abc123')"
                            },
                            "offset": {
                                "type": "integer",
                                "description": "Transcript line to start from (default 0). Use the offset given at the end of a truncated response to continue.",
                                "default": 0
                            },
                            "char_offset": {
                                "type": "integer",
                                "description": "Characters to skip at the start of the offset line (default 0). Set from the footer when a very long line spans pages.",
                                "default": 0
                            },
                            "max_lines": {
                                "type": "integer",
                                "description": "Max transcript lines to return (default: as many as fit in max_chars)."
                            },
                            "max_chars": {
                                "type": "integer",
                                "description": "Character budget for the transcript page (default 40000, max 200000).",
                                "default": 40000
                            }
                        },
                        "required": ["session_id"]
                    }
                },
                {
                    "name": "list_sessions",
                    "description": "List recent AI coding sessions, optionally filtered by provider or path. Returns sessions sorted by most recently updated.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "provider": {
                                "type": "string",
                                "enum": ["claude", "codex", "cursor", "antigravity", "pi"],
                                "description": "Filter by provider (optional)"
                            },
                            "path_prefix": {
                                "type": "string",
                                "description": "Filter sessions by working directory prefix (optional)"
                            },
                            "limit": {
                                "type": "integer",
                                "description": "Max results (default 20)",
                                "default": 20
                            }
                        }
                    }
                },
                {
                    "name": "timeline_for_repo",
                    "description": "Day-bucketed view of sessions for a repo path prefix — a named affordance for \"what changed over time?\" in this codebase. Returns metadata only (no transcripts), grouped by UTC calendar day.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "repo_prefix": {
                                "type": "string",
                                "description": "Repo or working-directory path prefix (matches cwd or repo_root, case-insensitive)"
                            },
                            "limit": {
                                "type": "integer",
                                "description": "Max sessions to include (default 30, max 200)",
                                "default": 30
                            }
                        },
                        "required": ["repo_prefix"]
                    }
                },
                {
                    "name": "get_resume_command",
                    "description": "Get the CLI command needed to resume a specific session in its native tool (Claude Code, Codex, or Pi). Cursor and Antigravity resume are not currently supported.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "session_id": {
                                "type": "string",
                                "description": "Session ID or unique prefix"
                            }
                        },
                        "required": ["session_id"]
                    }
                }
            ]
        }
    })
}

fn handle_tools_call(id: Option<Value>, params: &Value, config: &Config, db: &Db) -> Value {
    let tool_name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let result = match tool_name {
        "search_sessions" => tool_search_sessions(&args, config, db),
        "get_session" => tool_get_session(&args, db),
        "list_sessions" => tool_list_sessions(&args, db),
        "timeline_for_repo" => tool_timeline_for_repo(&args, db),
        "get_resume_command" => tool_get_resume_command(&args, db),
        _ => Err(format!("unknown tool: {tool_name}")),
    };

    match result {
        Ok(content) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "content": [{ "type": "text", "text": content }]
            }
        }),
        Err(err) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "isError": true,
                "content": [{ "type": "text", "text": err }]
            }
        }),
    }
}

fn tool_search_sessions(args: &Value, config: &Config, db: &Db) -> Result<String, String> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .ok_or("missing required parameter: query")?;
    let limit = result_limit(args, 10);
    let provider = args
        .get("provider")
        .and_then(Value::as_str)
        .map(|p| p.parse::<Provider>())
        .transpose()
        .map_err(|e| e.to_string())?;

    let filters = SearchFilters {
        provider,
        path_prefix: None,
        since: None,
        limit,
        warnings_only: false,
    };
    let repo = current_repo(config);
    let hits = db
        .search(query, &filters, repo.as_deref())
        .map_err(|e| e.to_string())?;

    if hits.is_empty() {
        return Ok("No sessions found matching the query.".to_string());
    }

    let mut out = String::new();
    for hit in &hits {
        let s = &hit.session;
        let title = s
            .title
            .as_deref()
            .map(|t| truncate_for_display(t, 120))
            .unwrap_or_else(|| "(untitled)".to_string());
        let cwd = s.cwd.as_deref().unwrap_or("-");
        let updated = s
            .updated_at
            .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "-".to_string());

        out.push_str(&format!(
            "## {} [{}] (score: {})\n- ID: {}\n- Provider: {}\n- CWD: {}\n- Updated: {}\n- Match: {} — {}\n\n",
            title,
            s.provider,
            hit.score,
            s.id,
            s.provider,
            cwd,
            updated,
            hit.match_source,
            hit.match_snippet,
        ));
    }
    Ok(out)
}

fn tool_get_session(args: &Value, db: &Db) -> Result<String, String> {
    let session_id = args
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or("missing required parameter: session_id")?;
    let usize_arg = |key: &str| args.get(key).and_then(Value::as_u64).map(|v| v as usize);
    let offset = usize_arg("offset").unwrap_or(0);
    let char_offset = usize_arg("char_offset").unwrap_or(0);
    let max_lines = usize_arg("max_lines");
    let max_chars = usize_arg("max_chars")
        .unwrap_or(DEFAULT_TRANSCRIPT_CHARS)
        .clamp(1, MAX_TRANSCRIPT_CHARS);

    let full = db.resolve_session(session_id).map_err(|e| e.to_string())?;
    let s = &full.session;
    let transcript = page_transcript(
        &full.transcript_text,
        offset,
        char_offset,
        max_lines,
        max_chars,
    );

    let title = s.title.as_deref().unwrap_or("(untitled)");
    let cwd = s.cwd.as_deref().unwrap_or("-");
    let updated = s
        .updated_at
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "-".to_string());

    Ok(format!(
        "# {title}\n\n- ID: {}\n- Provider: {}\n- Provider Session ID: {}\n- CWD: {cwd}\n- Updated: {updated}\n- Messages: {}\n\n## Transcript\n\n{transcript}",
        s.id,
        s.provider,
        s.provider_session_id,
        s.message_count.unwrap_or(0),
    ))
}

/// The `limit` argument of search/list tools, clamped to a sane range.
fn result_limit(args: &Value, default: usize) -> usize {
    args.get("limit")
        .and_then(Value::as_u64)
        .map_or(default, |limit| limit.clamp(1, 500) as usize)
}

/// Default and maximum transcript characters per `get_session` call. Agents'
/// MCP clients cap tool output (Claude Code: ~25k tokens by default), so whole
/// transcripts, which can run to megabytes, are paged.
const DEFAULT_TRANSCRIPT_CHARS: usize = 40_000;
const MAX_TRANSCRIPT_CHARS: usize = 200_000;

/// Return transcript text from line `offset` (skipping its first `char_offset`
/// characters), stopping at `max_lines` or before exceeding `max_chars`, plus a
/// footer saying where to continue. A line longer than the budget is split
/// across calls via `char_offset`, so every character stays reachable.
fn page_transcript(
    transcript: &str,
    offset: usize,
    char_offset: usize,
    max_lines: Option<usize>,
    max_chars: usize,
) -> String {
    let lines: Vec<&str> = transcript.lines().collect();
    let total = lines.len();
    if total == 0 {
        return String::new();
    }
    if offset >= total {
        return format!("[offset {offset} is past the end of the transcript ({total} lines)]");
    }
    let line_limit = max_lines
        .filter(|&n| n > 0)
        .map_or(total, |n| offset.saturating_add(n).min(total));

    let mut page = String::new();
    let mut used_chars = 0;
    let mut end = offset;
    // Set when the page stops partway through line `end`: characters of it read so far.
    let mut resume_char = None;
    for (index, line) in lines[offset..line_limit].iter().enumerate() {
        let skip = if index == 0 { char_offset } else { 0 };
        let rest = line
            .char_indices()
            .nth(skip)
            .map_or("", |(byte, _)| &line[byte..]);
        let rest_chars = rest.chars().count();
        if used_chars + rest_chars + 1 > max_chars {
            if page.is_empty() {
                page.extend(rest.chars().take(max_chars));
                page.push('\n');
                resume_char = Some(skip + max_chars);
            }
            break;
        }
        page.push_str(rest);
        page.push('\n');
        used_chars += rest_chars + 1;
        end += 1;
    }

    if offset == 0 && char_offset == 0 && end == total {
        return page;
    }
    let last_shown = if resume_char.is_some() { end + 1 } else { end };
    let mut footer = format!("\n[Transcript lines {}-{last_shown} of {total}", offset + 1);
    match resume_char {
        Some(chars) => footer.push_str(&format!(
            "; line {last_shown} continues. Call get_session with offset={end} and char_offset={chars} to continue."
        )),
        None if end < total => footer.push_str(&format!(
            ". Call get_session with offset={end} to continue."
        )),
        None => {}
    }
    footer.push(']');
    page + &footer
}

fn tool_list_sessions(args: &Value, db: &Db) -> Result<String, String> {
    let limit = result_limit(args, 20);
    let provider = args
        .get("provider")
        .and_then(Value::as_str)
        .map(|p| p.parse::<Provider>())
        .transpose()
        .map_err(|e| e.to_string())?;
    let path_prefix = args
        .get("path_prefix")
        .and_then(Value::as_str)
        .map(String::from);

    let filters = SearchFilters {
        provider,
        path_prefix,
        since: None,
        limit,
        warnings_only: false,
    };
    let sessions = db.list_recent(&filters).map_err(|e| e.to_string())?;

    if sessions.is_empty() {
        return Ok("No sessions found.".to_string());
    }

    let mut out = String::new();
    for s in &sessions {
        let title = s
            .title
            .as_deref()
            .map(|t| truncate_for_display(t, 120))
            .unwrap_or_else(|| "(untitled)".to_string());
        let cwd = s.cwd.as_deref().unwrap_or("-");
        let updated = s
            .updated_at
            .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "-".to_string());

        out.push_str(&format!(
            "- **{}** [{}] — {} | CWD: {} | ID: {}\n",
            title, s.provider, updated, cwd, s.id,
        ));
    }
    Ok(out)
}

fn tool_timeline_for_repo(args: &Value, db: &Db) -> Result<String, String> {
    let repo_prefix = args
        .get("repo_prefix")
        .and_then(Value::as_str)
        .ok_or("missing required parameter: repo_prefix")?;
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .map(|v| v as usize)
        .unwrap_or(timeline::DEFAULT_LIMIT);
    let limit = timeline::clamp_limit(limit);

    let sessions = db
        .list_session_records_for_repo_prefix(repo_prefix, limit)
        .map_err(|e| e.to_string())?;
    Ok(timeline::build_repo_timeline(sessions, repo_prefix, limit))
}

fn tool_get_resume_command(args: &Value, db: &Db) -> Result<String, String> {
    let session_id = args
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or("missing required parameter: session_id")?;

    let full = db.resolve_session(session_id).map_err(|e| e.to_string())?;
    let (command, cwd) = resume_plan(&full.session).map_err(|e| e.to_string())?;

    let cmd_str = shlex::try_join(command.iter().map(String::as_str)).map_err(|e| e.to_string())?;
    match cwd {
        Some(cwd) => {
            let quoted = shlex::try_quote(&cwd).map_err(|e| e.to_string())?;
            Ok(format!("cd {quoted} && {cmd_str}"))
        }
        None => Ok(cmd_str),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notifications_and_responses_get_no_reply() {
        for line in [
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/roots/list_changed"}"#,
            r#"{"jsonrpc":"2.0","id":7,"result":{}}"#,
        ] {
            assert!(matches!(parse_incoming(line), Incoming::Ignore), "{line}");
        }
    }

    #[test]
    fn malformed_messages_get_json_rpc_errors() {
        let Incoming::Reject(parse) = parse_incoming("not json") else {
            panic!("expected parse error");
        };
        assert_eq!(parse["error"]["code"], -32700);
        assert_eq!(parse["id"], Value::Null);

        let Incoming::Reject(invalid) = parse_incoming(r#"{"jsonrpc":"2.0","id":3}"#) else {
            panic!("expected invalid request");
        };
        assert_eq!(invalid["error"]["code"], -32600);
        assert_eq!(invalid["id"], 3);
    }

    #[test]
    fn short_transcripts_are_returned_whole() {
        assert_eq!(page_transcript("a\nb", 0, 0, None, 100), "a\nb\n");
    }

    #[test]
    fn long_transcripts_are_paged_with_continuation_offsets() {
        let transcript = (1..=10)
            .map(|i| format!("line {i:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        // Each line is 7 chars + newline; 20 chars fits two lines.
        let first = page_transcript(&transcript, 0, 0, None, 20);
        assert!(first.starts_with("line 01\nline 02\n\n[Transcript lines 1-2 of 10"));
        assert!(first.contains("offset=2 to continue"));

        let last = page_transcript(&transcript, 8, 0, None, 1000);
        assert!(last.starts_with("line 09\nline 10\n"));
        assert!(last.ends_with("[Transcript lines 9-10 of 10]"));

        assert!(page_transcript(&transcript, 10, 0, None, 1000).contains("past the end"));

        let limited = page_transcript(&transcript, 0, 0, Some(3), 1000);
        assert!(limited.contains("lines 1-3 of 10") && limited.contains("offset=3"));
    }

    #[test]
    fn oversized_lines_are_cut_by_characters() {
        let page = page_transcript("ééééé\nnext", 0, 0, None, 3);
        assert!(page.starts_with("ééé\n\n[Transcript lines 1-1 of 2; line 1 continues"));
        assert!(page.contains("offset=0 and char_offset=3"));
        let rest = page_transcript("ééééé\nnext", 0, 3, None, 3);
        assert!(
            rest.starts_with("éé\n\n[Transcript lines 1-1 of 2. Call get_session with offset=1")
        );
        // The budget counts characters, not bytes: 5 two-byte chars fit in 6.
        assert_eq!(page_transcript("ééééé", 0, 0, None, 6), "ééééé\n");
    }

    #[test]
    fn long_lines_can_be_read_completely_across_pages() {
        let line: String = (0..50).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
        let transcript = format!("{line}\nnext");

        let first = page_transcript(&transcript, 0, 0, None, 20);
        assert!(
            first.starts_with(&format!("{}\n\n", &line[..20])),
            "{first}"
        );
        assert!(first.contains("offset=0 and char_offset=20"), "{first}");

        let second = page_transcript(&transcript, 0, 20, None, 20);
        assert!(
            second.starts_with(&format!("{}\n\n", &line[20..40])),
            "{second}"
        );
        assert!(second.contains("offset=0 and char_offset=40"), "{second}");

        let last = page_transcript(&transcript, 0, 40, None, 20);
        assert!(
            last.starts_with(&format!("{}\nnext\n", &line[40..])),
            "{last}"
        );
        assert!(last.ends_with("[Transcript lines 1-2 of 2]"), "{last}");
    }

    #[test]
    fn zero_max_lines_means_no_line_limit() {
        assert_eq!(page_transcript("a\nb", 0, 0, Some(0), 100), "a\nb\n");
    }

    #[test]
    fn batches_and_non_objects_are_rejected() {
        for line in [r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#, "42"] {
            let Incoming::Reject(error) = parse_incoming(line) else {
                panic!("expected rejection for {line}");
            };
            assert_eq!(error["error"]["code"], -32600);
        }
    }

    #[test]
    fn tools_list_includes_timeline_for_repo() {
        let response = handle_tools_list(None);
        let tools = response["result"]["tools"].as_array().expect("tools array");
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        assert!(names.contains(&"timeline_for_repo"));
    }
}
