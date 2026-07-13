use anyhow::Result;

use crate::config::Config;
use crate::db::{Db, SourceScanCommit};
use crate::models::Provider;
use crate::providers::{
    antigravity::AntigravityAdapter,
    claude::ClaudeAdapter,
    codex::CodexAdapter,
    codex_logs::{CodexLogsAdapter, DISCOVERY_SOURCE},
    cursor::CursorAdapter,
    pi::PiAdapter,
};
use crate::util::normalize_path;

/// Incrementally (or fully) reindex all enabled providers into `db`.
///
/// Returns `(files_seen, sessions_updated)`. When `full` is true the database is
/// cleared first and every discovered file is re-parsed. Otherwise each file is
/// skipped when its `(mtime_ns, size_bytes)` already matches what's recorded in
/// `files_seen`, making repeated calls cheap.
///
/// When `progress` is provided it's invoked with `(index, total, updated)` after
/// each updated file so callers can render progress; the CLI uses this and the
/// MCP server passes `None`.
pub fn reindex(
    config: &Config,
    db: &Db,
    full: bool,
    progress: Option<&mut dyn FnMut(usize, usize, usize)>,
) -> Result<(usize, usize)> {
    if full {
        db.clear_all()?;
    }

    let claude = ClaudeAdapter::new(config.claude_paths());
    let codex = CodexAdapter::new(config.codex_paths(), config.codex_home());
    let cursor = CursorAdapter::new(config.cursor_paths());
    let antigravity = AntigravityAdapter::new(config.antigravity_paths());
    let pi = PiAdapter::new(config.pi_paths());

    let mut sources = Vec::new();
    if config.providers.claude.enabled {
        sources.extend(claude.discover());
    }
    if config.providers.codex.enabled {
        sources.extend(codex.discover());
    }
    if config.providers.cursor.enabled {
        sources.extend(cursor.discover());
    }
    if config.providers.antigravity.enabled {
        sources.extend(antigravity.discover());
    }
    if config.providers.pi.enabled {
        sources.extend(pi.discover());
    }

    let total = sources.len();
    let mut updated = 0usize;
    let mut progress = progress;
    for (i, source) in sources.iter().enumerate() {
        let source_path = normalize_path(&source.path);
        if !full
            && db.is_file_current(
                source.provider,
                &source_path,
                source.mtime_ns,
                source.size_bytes,
            )?
        {
            continue;
        }
        let parsed = match source.provider {
            Provider::Claude => claude.parse(source),
            Provider::Codex => codex.parse(source),
            Provider::Cursor => cursor.parse(source),
            Provider::Antigravity => antigravity.parse(source),
            Provider::Pi => pi.parse(source),
        };
        db.upsert_session(&parsed, source.mtime_ns, source.size_bytes)?;
        updated += 1;
        if let Some(cb) = progress.as_deref_mut() {
            cb(i + 1, total, updated);
        }
    }

    if config.providers.codex.enabled {
        let logs = CodexLogsAdapter::new(&config.codex_home());
        let source_path = logs.source_path();
        let durable_ids = codex.durable_ids();
        let stale_diagnostic_ids: std::collections::HashSet<_> = db
            .discovered_provider_ids(Provider::Codex, DISCOVERY_SOURCE)?
            .intersection(&durable_ids)
            .cloned()
            .collect();
        let checkpoint = (!full)
            .then(|| db.source_checkpoint(Provider::Codex, &source_path))
            .transpose()?
            .flatten();
        match logs.recover(&durable_ids, checkpoint.as_deref()) {
            Ok(recovery) => {
                if !recovery.unchanged || !stale_diagnostic_ids.is_empty() {
                    updated += recovery.sessions.len();
                    db.commit_source_scan(SourceScanCommit {
                        provider: Provider::Codex,
                        checkpoint_path: &source_path,
                        discovery_source: DISCOVERY_SOURCE,
                        checkpoint: recovery.checkpoint.as_deref(),
                        replace_all: recovery.replace_all,
                        affected_provider_ids: &recovery.affected_ids,
                        durable_provider_ids: &stale_diagnostic_ids,
                        sessions: &recovery.sessions,
                    })?;
                }
            }
            Err(err) => {
                eprintln!("sessiongrep: optional Codex logs source unavailable: {err:#}")
            }
        }
    }

    Ok((total + usize::from(config.providers.codex.enabled), updated))
}
