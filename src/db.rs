use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use rusqlite::{params, Connection, OptionalExtension};

use crate::models::{
    ParsedSession, Provider, SearchFilters, SearchHit, SessionRecord, SessionWithTranscript,
};
use crate::util::{snippet_from_match, truncate_for_display};

pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let conn = Connection::open(path)?;
        let db = Self { conn };
        db.init()?;
        Ok(db)
    }

    fn init(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            pragma journal_mode = wal;
            pragma foreign_keys = on;
            create table if not exists sessions (
                id text primary key,
                provider text not null,
                provider_session_id text not null,
                title text,
                summary text,
                cwd text,
                repo_root text,
                created_at text,
                updated_at text,
                last_message_at text,
                preview_text text not null,
                source_path text not null,
                message_count integer,
                parse_version text not null,
                raw_metadata_json text,
                parse_warning text,
                discovery_source text not null
            );
            create table if not exists transcripts (
                session_id text primary key references sessions(id) on delete cascade,
                transcript_text text not null
            );
            create table if not exists files_seen (
                provider text not null,
                source_path text not null,
                mtime_ns integer not null,
                size_bytes integer not null,
                last_indexed_at text not null,
                content_hash text,
                primary key(provider, source_path)
            );
            create index if not exists idx_sessions_provider on sessions(provider);
            create index if not exists idx_sessions_updated_at on sessions(updated_at desc);
            create index if not exists idx_sessions_provider_id on sessions(provider_session_id);
            ",
        )?;
        // Migrate: drop old contentless FTS table if present, then create regular FTS table
        let fts_sql: Option<String> = self
            .conn
            .query_row(
                "select sql from sqlite_master where type='table' and name='sessions_fts'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if fts_sql.as_ref().is_some_and(|sql| sql.contains("content=")) {
            self.conn.execute_batch("drop table sessions_fts")?;
        }
        self.conn.execute_batch(
            "create virtual table if not exists sessions_fts using fts5(
                title, summary, preview_text, transcript_text
            )",
        )?;
        // Auto-populate FTS if sessions exist but FTS is empty (e.g. after schema upgrade)
        let sessions_count: i64 =
            self.conn
                .query_row("select count(*) from sessions", [], |row| row.get(0))?;
        let fts_count: i64 =
            self.conn
                .query_row("select count(*) from sessions_fts", [], |row| row.get(0))?;
        if sessions_count > 0 && fts_count == 0 {
            self.conn.execute(
                "insert into sessions_fts (rowid, title, summary, preview_text, transcript_text)
                 select s.rowid, s.title, s.summary, s.preview_text, coalesce(t.transcript_text, '')
                 from sessions s
                 left join transcripts t on t.session_id = s.id",
                [],
            )?;
        }
        // Parser changes don't alter source files' mtime/size, so forget what we've
        // seen to re-parse everything still on disk. Sessions whose source files
        // are gone keep their indexed copy.
        let version: i64 = self
            .conn
            .query_row("pragma user_version", [], |row| row.get(0))?;
        if version < INDEX_FORMAT_VERSION {
            self.conn.execute_batch(&format!(
                "delete from files_seen; pragma user_version = {INDEX_FORMAT_VERSION};"
            ))?;
        }
        Ok(())
    }

    pub fn clear_all(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            delete from sessions_fts;
            delete from transcripts;
            delete from sessions;
            delete from files_seen;
            ",
        )?;
        Ok(())
    }

    pub fn is_file_current(
        &self,
        provider: Provider,
        path: &str,
        mtime_ns: i64,
        size: i64,
        content_hash: Option<&str>,
    ) -> Result<bool> {
        let result = self
            .conn
            .query_row(
                "select mtime_ns, size_bytes, content_hash from files_seen where provider = ?1 and source_path = ?2",
                params![provider.as_str(), path],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?;
        Ok(matches!(
            result,
            Some((stored_mtime, stored_size, stored_hash))
                if stored_mtime == mtime_ns
                    && stored_size == size
                    && stored_hash.as_deref() == content_hash
        ))
    }

    pub fn upsert_session(
        &self,
        parsed: &ParsedSession,
        mtime_ns: i64,
        size_bytes: i64,
        content_hash: Option<&str>,
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let session = &parsed.session;
        if session.parse_warning.is_some() {
            // Adapters only warn when a file couldn't be read (e.g. briefly
            // unreadable). Keep whatever was indexed from it before, and leave the
            // file unrecorded so the next run tries again.
            if !source_has_session(&tx, session.provider, &session.source_path)? {
                write_session(&tx, parsed)?;
            }
            tx.commit()?;
            return Ok(());
        }
        // A source file holds one session. If the ID parsed from it changed, drop
        // the record indexed under the old ID so it doesn't linger as a duplicate.
        delete_sessions_from_source(
            &tx,
            session.provider,
            &session.source_path,
            Some(&session.id),
        )?;
        write_session(&tx, parsed)?;
        record_file_seen(
            &tx,
            session.provider,
            &session.source_path,
            mtime_ns,
            size_bytes,
            content_hash,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Record `source_path` as seen without indexing a session for it, removing
    /// any session previously indexed from that file.
    pub fn exclude_source(
        &self,
        provider: Provider,
        source_path: &str,
        mtime_ns: i64,
        size_bytes: i64,
        content_hash: Option<&str>,
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        delete_sessions_from_source(&tx, provider, source_path, None)?;
        record_file_seen(
            &tx,
            provider,
            source_path,
            mtime_ns,
            size_bytes,
            content_hash,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Most recently updated sessions matching `filters`, metadata only.
    pub fn list_recent(&self, filters: &SearchFilters) -> Result<Vec<SessionRecord>> {
        let (filter_sql, filter_params) = filter_clause(filters);
        let sql = format!(
            "select {SESSION_COLUMNS} from sessions s where 1 = 1 {filter_sql}
             order by s.updated_at desc, s.id asc limit {}",
            sql_limit(filters.limit)
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(filter_params.iter()),
            row_to_session_record,
        )?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Load session metadata only (no transcript join) for a repo path prefix.
    /// Prefix matching is ASCII case-insensitive on `cwd` and `repo_root`.
    /// LIKE metacharacters in the prefix are escaped so `_`/`%` are literal.
    pub fn list_session_records_for_repo_prefix(
        &self,
        repo_prefix: &str,
        limit: usize,
    ) -> Result<Vec<SessionRecord>> {
        let pattern = format!("{}%", escape_like_prefix(repo_prefix));
        let sql = "
            select
                s.id, s.provider, s.provider_session_id, s.title, s.summary, s.cwd, s.repo_root,
                s.created_at, s.updated_at, s.last_message_at, s.preview_text, s.source_path,
                s.message_count, s.parse_version, s.raw_metadata_json, s.parse_warning, s.discovery_source
            from sessions s
            where lower(coalesce(s.cwd, '')) like ? escape '\\'
               or lower(coalesce(s.repo_root, '')) like ? escape '\\'
            order by coalesce(s.updated_at, s.created_at, '') desc, s.id asc
            limit ?
        ";
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(
            params![pattern, pattern, limit as i64],
            row_to_session_record,
        )?;
        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    pub fn search(
        &self,
        query: &str,
        filters: &SearchFilters,
        current_repo: Option<&str>,
    ) -> Result<Vec<SearchHit>> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }

        let query_lower = query.to_ascii_lowercase();
        let tokens: Vec<&str> = query_lower.split_whitespace().collect();
        let pool = filters.limit.saturating_mul(5).max(50);

        // Keyword candidates come from FTS with the filters applied in SQL, so a
        // narrow filter (e.g. one provider) can't be starved by matches elsewhere.
        // Each candidate is paired with whether FTS matched it.
        let mut candidates: Vec<(SessionWithTranscript, bool)> = match fts_query(query) {
            Some(fts) => self
                .fts_candidates(&fts, filters, pool)?
                .into_iter()
                .map(|candidate| (candidate, true))
                .collect(),
            None => Vec::new(),
        };
        // FTS doesn't index paths, so sessions in a matching directory join the
        // keyword hits. With no keyword hits at all, fall back to words inside
        // longer words (which FTS can't see, e.g. "grep" in "ripgrep") and
        // typo-tolerant matches. These candidates must earn relevance below.
        let extra = if candidates.is_empty() {
            self.fallback_candidates(filters, &tokens)?
        } else {
            self.path_candidates(filters, &tokens, pool)?
        };
        let mut seen: HashSet<String> = candidates
            .iter()
            .map(|(candidate, _)| candidate.session.id.clone())
            .collect();
        for candidate in extra {
            if seen.insert(candidate.session.id.clone()) {
                candidates.push((candidate, false));
            }
        }

        let matcher = SkimMatcherV2::default().smart_case();
        let min_fuzzy_score = FUZZY_SCORE_PER_CHAR * query.chars().count() as i64;
        let mut hits = Vec::new();

        for (record, fts_match) in candidates {
            let title = record.session.title.as_deref().unwrap_or_default();
            let summary = record.session.summary.as_deref().unwrap_or_default();
            let cwd = record.session.cwd.as_deref().unwrap_or_default();
            let repo_root = record.session.repo_root.as_deref().unwrap_or_default();
            let preview = record.session.preview_text.as_str();
            let transcript = record.transcript_text.as_str();
            let haystacks = [
                ("title", title),
                ("summary", summary),
                ("cwd", cwd),
                ("repo", repo_root),
                ("preview", preview),
                ("transcript", transcript),
            ];

            let mut score = 0i64;
            let mut best_source = "fuzzy".to_string();
            let mut best_source_score = i64::MIN;
            let mut best_snippet = snippet_from_match(preview, query, 160);

            let mut total_tokens_matched = 0usize;
            let mut literal_hit = false;
            let mut best_fuzzy = 0i64;
            for (source, value) in haystacks {
                let lowered = value.to_ascii_lowercase();
                let mut source_score = 0i64;
                if lowered.contains(&query_lower) {
                    literal_hit = true;
                    source_score += match source {
                        "title" => 600,
                        "summary" => 450,
                        "cwd" | "repo" => 350,
                        "preview" => 250,
                        _ => 100,
                    };
                }
                let mut tokens_hit = 0usize;
                for token in &tokens {
                    if !token.is_empty() && lowered.contains(token) {
                        source_score += 40;
                        tokens_hit += 1;
                    }
                }
                literal_hit |= tokens_hit > 0;
                total_tokens_matched = total_tokens_matched.max(tokens_hit);
                if matches!(source, "title" | "cwd" | "repo" | "preview") {
                    let fuzzy = matcher.fuzzy_match(value, query).unwrap_or_default().max(0);
                    best_fuzzy = best_fuzzy.max(fuzzy);
                    source_score += fuzzy;
                }

                score += source_score;
                if source_score > best_source_score {
                    best_source_score = source_score;
                    best_source = source.to_string();
                    best_snippet = snippet_from_match(value, query, 160);
                }
            }
            // Recency and current-repo bonuses below only rank relevant sessions;
            // they must never turn an unrelated session into a hit.
            if !fts_match && !literal_hit && best_fuzzy < min_fuzzy_score {
                continue;
            }
            // Bonus when all query tokens matched somewhere
            if tokens.len() > 1 && total_tokens_matched == tokens.len() {
                score += 150;
            }

            if let Some(updated_at) = record.session.updated_at {
                let age_days = (Utc::now() - updated_at).num_days().clamp(0, 90);
                score += (90 - age_days) * 2;
            }
            if let (Some(current_repo), Some(repo_root)) =
                (current_repo, record.session.repo_root.as_deref())
            {
                if current_repo == repo_root {
                    score += 200;
                    if best_source == "fuzzy" {
                        best_source = "repo".to_string();
                        best_snippet = snippet_from_match(repo_root, query, 160);
                    }
                }
            }
            hits.push(SearchHit {
                session: record.session,
                score,
                match_source: best_source,
                match_snippet: best_snippet,
            });
        }

        hits.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| b.session.updated_at.cmp(&a.session.updated_at))
        });
        hits.truncate(filters.limit);
        Ok(hits)
    }

    /// Sessions whose FTS index matches `fts_query`, best BM25 rank first.
    fn fts_candidates(
        &self,
        fts_query: &str,
        filters: &SearchFilters,
        limit: usize,
    ) -> Result<Vec<SessionWithTranscript>> {
        let (filter_sql, filter_params) = filter_clause(filters);
        let sql = format!(
            "select {SESSION_COLUMNS}, coalesce(t.transcript_text, '')
             from sessions_fts f
             join sessions s on s.rowid = f.rowid
             left join transcripts t on t.session_id = s.id
             where sessions_fts match ? {filter_sql}
             order by rank
             limit {}",
            sql_limit(limit)
        );
        let mut params_vec = vec![fts_query.to_string()];
        params_vec.extend(filter_params);
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(params_vec.iter()),
            row_to_session_with_transcript,
        )?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Sessions whose cwd or repo root contains a literal query word, newest first.
    fn path_candidates(
        &self,
        filters: &SearchFilters,
        tokens: &[&str],
        limit: usize,
    ) -> Result<Vec<SessionWithTranscript>> {
        let words: Vec<String> = tokens
            .iter()
            .filter(|token| token.chars().count() >= 2)
            .take(MAX_LITERAL_TOKENS)
            .map(|token| token.to_string())
            .collect();
        if words.is_empty() {
            return Ok(Vec::new());
        }
        let in_cwd = contains_any("lower(coalesce(s.cwd, ''))", words.len());
        let in_repo = contains_any("lower(coalesce(s.repo_root, ''))", words.len());
        let (filter_sql, filter_params) = filter_clause(filters);
        let sql = format!(
            "select {SESSION_COLUMNS}, coalesce(t.transcript_text, '')
             from sessions s
             left join transcripts t on t.session_id = s.id
             where ({in_cwd} or {in_repo}) {filter_sql}
             order by s.updated_at desc
             limit {}",
            sql_limit(limit)
        );
        let mut params_vec = words.clone();
        params_vec.extend(words);
        params_vec.extend(filter_params);
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(params_vec.iter()),
            row_to_session_with_transcript,
        )?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// All sessions matching `filters`, for the fallback when FTS finds nothing.
    /// Transcripts are only loaded (and only searched) where they contain one
    /// of the query words, so the fallback stays cheap on large histories.
    fn fallback_candidates(
        &self,
        filters: &SearchFilters,
        tokens: &[&str],
    ) -> Result<Vec<SessionWithTranscript>> {
        let words: Vec<String> = tokens
            .iter()
            .take(MAX_LITERAL_TOKENS)
            .map(|token| token.to_string())
            .collect();
        let in_transcript = contains_any("lower(coalesce(t.transcript_text, ''))", words.len());
        let (filter_sql, filter_params) = filter_clause(filters);
        let sql = format!(
            "select {SESSION_COLUMNS},
                 case when {in_transcript} then t.transcript_text else '' end
             from sessions s
             left join transcripts t on t.session_id = s.id
             where 1 = 1 {filter_sql}"
        );
        let mut params_vec = words;
        params_vec.extend(filter_params);
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(params_vec.iter()),
            row_to_session_with_transcript,
        )?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn resolve_session(&self, value: &str) -> Result<SessionWithTranscript> {
        let mut stmt = self.conn.prepare(
            "
            select
                s.id, s.provider, s.provider_session_id, s.title, s.summary, s.cwd, s.repo_root,
                s.created_at, s.updated_at, s.last_message_at, s.preview_text, s.source_path,
                s.message_count, s.parse_version, s.raw_metadata_json, s.parse_warning, s.discovery_source,
                coalesce(t.transcript_text, '')
            from sessions s
            left join transcripts t on t.session_id = s.id
            where s.id = ?1 or s.provider_session_id = ?1
               or s.id like ?2 escape '\\' or s.provider_session_id like ?2 escape '\\'
            ",
        )?;

        let pattern = format!("{}%", escape_like_prefix(value));
        let rows = stmt.query_map(params![value, pattern], row_to_session_with_transcript)?;
        let mut matches = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        // An exact ID wins even if it is also a prefix of some other ID.
        if let Some(index) = matches
            .iter()
            .position(|m| m.session.id == value || m.session.provider_session_id == value)
        {
            return Ok(matches.swap_remove(index));
        }
        match matches.len() {
            0 => Err(anyhow!("no session matches '{value}'")),
            1 => Ok(matches.remove(0)),
            count => {
                let examples = matches
                    .iter()
                    .take(5)
                    .map(|m| {
                        let title = m.session.title.as_deref().unwrap_or("(untitled)");
                        format!("  {}  {}", m.session.id, truncate_for_display(title, 60))
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Err(anyhow!(
                    "session prefix '{value}' is ambiguous ({count} matches); use more characters:\n{examples}"
                ))
            }
        }
    }

    pub fn count_parse_warnings(&self) -> Result<i64> {
        self.conn
            .query_row(
                "select count(*) from sessions where parse_warning is not null and parse_warning != ''",
                [],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub fn counts_by_provider(&self) -> Result<HashMap<String, i64>> {
        let mut stmt = self
            .conn
            .prepare("select provider, count(*) from sessions group by provider")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut out = HashMap::new();
        for row in rows {
            let (provider, count) = row?;
            out.insert(provider, count);
        }
        Ok(out)
    }
}

/// Insert or update a session row, its transcript, and its FTS entry.
fn write_session(conn: &Connection, parsed: &ParsedSession) -> Result<()> {
    let session = &parsed.session;
    conn.execute(
        "
        insert into sessions (
            id, provider, provider_session_id, title, summary, cwd, repo_root, created_at,
            updated_at, last_message_at, preview_text, source_path, message_count, parse_version,
            raw_metadata_json, parse_warning, discovery_source
        ) values (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
            ?9, ?10, ?11, ?12, ?13, ?14,
            ?15, ?16, ?17
        )
        on conflict(id) do update set
            provider = excluded.provider,
            provider_session_id = excluded.provider_session_id,
            title = excluded.title,
            summary = excluded.summary,
            cwd = excluded.cwd,
            repo_root = excluded.repo_root,
            created_at = excluded.created_at,
            updated_at = excluded.updated_at,
            last_message_at = excluded.last_message_at,
            preview_text = excluded.preview_text,
            source_path = excluded.source_path,
            message_count = excluded.message_count,
            parse_version = excluded.parse_version,
            raw_metadata_json = excluded.raw_metadata_json,
            parse_warning = excluded.parse_warning,
            discovery_source = excluded.discovery_source
        ",
        params![
            session.id,
            session.provider.as_str(),
            session.provider_session_id,
            session.title,
            session.summary,
            session.cwd,
            session.repo_root,
            session.created_at.map(|value| value.to_rfc3339()),
            session.updated_at.map(|value| value.to_rfc3339()),
            session.last_message_at.map(|value| value.to_rfc3339()),
            session.preview_text,
            session.source_path,
            session.message_count,
            session.parse_version,
            session.raw_metadata_json,
            session.parse_warning,
            session.discovery_source,
        ],
    )?;
    conn.execute(
        "
        insert into transcripts (session_id, transcript_text)
        values (?1, ?2)
        on conflict(session_id) do update set transcript_text = excluded.transcript_text
        ",
        params![session.id, parsed.transcript_text],
    )?;
    // Update FTS index: delete old entry then insert new one
    conn.execute(
        "insert or replace into sessions_fts (rowid, title, summary, preview_text, transcript_text)
         values (
             (select rowid from sessions where id = ?1),
             ?2, ?3, ?4, ?5
         )",
        params![
            session.id,
            session.title,
            session.summary,
            session.preview_text,
            parsed.transcript_text,
        ],
    )?;
    Ok(())
}

/// Whether any session is indexed from `source_path`.
fn source_has_session(conn: &Connection, provider: Provider, source_path: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "select 1 from sessions where provider = ?1 and source_path = ?2 limit 1",
            params![provider.as_str(), source_path],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Delete sessions indexed from `source_path`, except the one with `keep_id`.
fn delete_sessions_from_source(
    conn: &Connection,
    provider: Provider,
    source_path: &str,
    keep_id: Option<&str>,
) -> Result<()> {
    let matching = "provider = ?1 and source_path = ?2 and (?3 is null or id != ?3)";
    let args = params![provider.as_str(), source_path, keep_id];
    conn.execute(
        &format!(
            "delete from sessions_fts where rowid in (select rowid from sessions where {matching})"
        ),
        args,
    )?;
    conn.execute(
        &format!("delete from transcripts where session_id in (select id from sessions where {matching})"),
        args,
    )?;
    conn.execute(&format!("delete from sessions where {matching}"), args)?;
    Ok(())
}

fn record_file_seen(
    conn: &Connection,
    provider: Provider,
    source_path: &str,
    mtime_ns: i64,
    size_bytes: i64,
    content_hash: Option<&str>,
) -> Result<()> {
    conn.execute(
        "
        insert into files_seen (provider, source_path, mtime_ns, size_bytes, last_indexed_at, content_hash)
        values (?1, ?2, ?3, ?4, ?5, ?6)
        on conflict(provider, source_path) do update set
            mtime_ns = excluded.mtime_ns,
            size_bytes = excluded.size_bytes,
            last_indexed_at = excluded.last_indexed_at,
            content_hash = excluded.content_hash
        ",
        params![
            provider.as_str(),
            source_path,
            mtime_ns,
            size_bytes,
            Utc::now().to_rfc3339(),
            content_hash,
        ],
    )?;
    Ok(())
}

fn row_to_session_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRecord> {
    let provider: String = row.get(1)?;
    Ok(SessionRecord {
        id: row.get(0)?,
        provider: provider
            .parse()
            .map_err(|_| rusqlite::Error::InvalidQuery)?,
        provider_session_id: row.get(2)?,
        title: row.get(3)?,
        summary: row.get(4)?,
        cwd: row.get(5)?,
        repo_root: row.get(6)?,
        created_at: row
            .get::<_, Option<String>>(7)?
            .as_deref()
            .and_then(crate::util::parse_datetime),
        updated_at: row
            .get::<_, Option<String>>(8)?
            .as_deref()
            .and_then(crate::util::parse_datetime),
        last_message_at: row
            .get::<_, Option<String>>(9)?
            .as_deref()
            .and_then(crate::util::parse_datetime),
        preview_text: row.get(10)?,
        source_path: row.get(11)?,
        message_count: row.get(12)?,
        parse_version: row.get(13)?,
        raw_metadata_json: row.get(14)?,
        parse_warning: row.get(15)?,
        discovery_source: row.get(16)?,
    })
}

fn row_to_session_with_transcript(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<SessionWithTranscript> {
    Ok(SessionWithTranscript {
        session: row_to_session_record(row)?,
        transcript_text: row.get(17)?,
    })
}

/// Bump whenever a provider parser changes what it extracts, so existing indexes
/// re-parse their source files on the next run (stored in `pragma user_version`).
const INDEX_FORMAT_VERSION: i64 = 4;

/// Columns read by [`row_to_session_record`], in order, from `sessions s`.
const SESSION_COLUMNS: &str = "s.id, s.provider, s.provider_session_id, s.title, s.summary, \
     s.cwd, s.repo_root, s.created_at, s.updated_at, s.last_message_at, s.preview_text, \
     s.source_path, s.message_count, s.parse_version, s.raw_metadata_json, s.parse_warning, \
     s.discovery_source";

/// Minimum average fuzzy score per query character for a fuzzy-only match.
/// Typos and abbreviations ("sesiongrep", "platfrm") score ~16-19 per char;
/// scattered subsequence matches in unrelated text score near zero.
const FUZZY_SCORE_PER_CHAR: i64 = 12;

/// Build `and ...` SQL conditions (over `sessions s`) and their parameters for
/// `filters`. The limit is not included.
fn filter_clause(filters: &SearchFilters) -> (String, Vec<String>) {
    let mut sql = String::new();
    let mut params = Vec::new();
    if let Some(provider) = filters.provider {
        sql.push_str(" and s.provider = ?");
        params.push(provider.as_str().to_string());
    }
    if let Some(path_prefix) = &filters.path_prefix {
        sql.push_str(
            " and (lower(coalesce(s.cwd, '')) like ? escape '\\' \
             or lower(coalesce(s.repo_root, '')) like ? escape '\\')",
        );
        let pattern = format!("{}%", escape_like_prefix(path_prefix));
        params.push(pattern.clone());
        params.push(pattern);
    }
    if let Some(since) = filters.since {
        sql.push_str(" and coalesce(s.updated_at, s.created_at, '') >= ?");
        params.push(since.to_rfc3339());
    }
    if filters.warnings_only {
        sql.push_str(" and s.parse_warning is not null and s.parse_warning != ''");
    }
    (sql, params)
}

/// Query words used for literal (non-FTS) matching; more are ignored.
const MAX_LITERAL_TOKENS: usize = 8;

/// SQL that is true when `expr` contains any of `count` (at least one) bound strings.
fn contains_any(expr: &str, count: usize) -> String {
    let checks = vec![format!("instr({expr}, ?) > 0"); count];
    format!("({})", checks.join(" or "))
}

/// A `LIMIT` value SQLite accepts (it rejects integers above `i64::MAX`).
fn sql_limit(limit: usize) -> i64 {
    i64::try_from(limit).unwrap_or(i64::MAX)
}

/// Turn free text into an FTS5 query: each whitespace-separated token becomes a
/// quoted prefix term (`"tok"*`), OR-ed together, so partial words match while
/// typing and FTS syntax characters in the input are treated literally.
/// Returns `None` when no token contains anything FTS would index.
fn fts_query(query: &str) -> Option<String> {
    let terms: Vec<String> = query
        .split_whitespace()
        .filter(|token| token.chars().any(char::is_alphanumeric))
        .map(|token| format!("\"{}\"*", token.replace('"', "\"\"")))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" OR "))
}

/// Lowercase `prefix` and escape `\`, `%`, and `_` for a SQL LIKE pattern with
/// ESCAPE '\'. Lowercasing here pairs with the `lower(column)` in the queries,
/// so prefixes match case-insensitively.
pub(crate) fn escape_like_prefix(prefix: &str) -> String {
    prefix
        .to_ascii_lowercase()
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    use super::{escape_like_prefix, fts_query, Db};
    use crate::models::{ParsedSession, Provider, SearchFilters, SessionRecord};
    use chrono::Utc;
    use rusqlite::params;
    use tempfile::{tempdir, TempDir};

    fn open_db() -> (TempDir, Db) {
        let temp = tempdir().unwrap();
        let db = Db::open(&temp.path().join("index.db")).unwrap();
        (temp, db)
    }

    fn insert(db: &Db, provider: Provider, id: &str, title: &str, cwd: &str, transcript: &str) {
        let parsed = ParsedSession {
            session: SessionRecord {
                id: format!("{provider}:{id}"),
                provider,
                provider_session_id: id.to_string(),
                title: Some(title.to_string()),
                summary: None,
                cwd: Some(cwd.to_string()),
                repo_root: Some(cwd.to_string()),
                created_at: Some(Utc::now()),
                updated_at: Some(Utc::now()),
                last_message_at: Some(Utc::now()),
                preview_text: title.to_string(),
                source_path: format!("/sessions/{id}.jsonl"),
                message_count: Some(1),
                parse_version: "test".to_string(),
                raw_metadata_json: None,
                parse_warning: None,
                discovery_source: "test".to_string(),
            },
            transcript_text: transcript.to_string(),
        };
        db.upsert_session(&parsed, 0, 0, None).unwrap();
    }

    fn filters(provider: Option<Provider>, limit: usize) -> SearchFilters {
        SearchFilters {
            provider,
            path_prefix: None,
            since: None,
            limit,
            warnings_only: false,
        }
    }

    fn hit_ids(db: &Db, query: &str, filters: &SearchFilters, repo: Option<&str>) -> Vec<String> {
        db.search(query, filters, repo)
            .unwrap()
            .into_iter()
            .map(|hit| hit.session.id)
            .collect()
    }

    #[test]
    fn unrelated_query_returns_nothing_even_for_recent_sessions_in_current_repo() {
        let (_temp, db) = open_db();
        insert(
            &db,
            Provider::Claude,
            "a",
            "Fix auth bug",
            "/src/app",
            "token refresh failed",
        );
        insert(
            &db,
            Provider::Codex,
            "b",
            "Add redis cache",
            "/src/app",
            "cache layer",
        );

        let hits = hit_ids(
            &db,
            "qzxjvkw-nonexistent",
            &filters(None, 25),
            Some("/src/app"),
        );
        assert!(hits.is_empty(), "unexpected hits: {hits:?}");
    }

    #[test]
    fn provider_filter_is_applied_before_the_candidate_limit() {
        let (_temp, db) = open_db();
        for i in 0..80 {
            insert(
                &db,
                Provider::Claude,
                &format!("c{i}"),
                "redis redis redis",
                "/src/a",
                "redis",
            );
        }
        insert(
            &db,
            Provider::Codex,
            "only-codex",
            "notes",
            "/src/b",
            "one mention of redis",
        );

        let hits = hit_ids(&db, "redis", &filters(Some(Provider::Codex), 1), None);
        assert_eq!(hits, vec!["codex:only-codex"]);
    }

    #[test]
    fn partial_words_match_by_prefix() {
        let (_temp, db) = open_db();
        insert(
            &db,
            Provider::Claude,
            "a",
            "Index tuning",
            "/src/app",
            "move to sqlite fts5",
        );
        insert(
            &db,
            Provider::Claude,
            "b",
            "Unrelated",
            "/src/app",
            "nothing here",
        );

        assert_eq!(
            hit_ids(&db, "sqli", &filters(None, 10), None),
            vec!["claude:a"]
        );
    }

    #[test]
    fn fallback_matches_typos_and_infixes_in_metadata_only() {
        let (_temp, db) = open_db();
        insert(
            &db,
            Provider::Claude,
            "a",
            "Tidy CLI",
            "/src/sessiongrep",
            "help text",
        );
        insert(
            &db,
            Provider::Claude,
            "b",
            "Deploy",
            "/src/platform",
            "helm upgrade",
        );

        assert_eq!(
            hit_ids(&db, "sesiongrep", &filters(None, 10), None),
            vec!["claude:a"]
        );
        // "grep" sits inside the token "sessiongrep", which FTS prefix queries can't see.
        assert_eq!(
            hit_ids(&db, "grep", &filters(None, 10), None),
            vec!["claude:a"]
        );
        // Same for words inside transcript tokens ("grade" in "upgrade").
        assert_eq!(
            hit_ids(&db, "grade", &filters(None, 10), None),
            vec!["claude:b"]
        );
    }

    #[test]
    fn keyword_hits_do_not_hide_sessions_in_a_matching_directory() {
        let (_temp, db) = open_db();
        insert(
            &db,
            Provider::Claude,
            "path",
            "Fix startup",
            "/src/platform",
            "repair launch sequence",
        );
        insert(
            &db,
            Provider::Claude,
            "prefix",
            "platformer game",
            "/src/game",
            "render sprites",
        );
        insert(
            &db,
            Provider::Claude,
            "other",
            "Unrelated",
            "/src/misc",
            "x",
        );

        let mut hits = hit_ids(&db, "platform", &filters(None, 25), None);
        hits.sort();
        assert_eq!(hits, vec!["claude:path", "claude:prefix"]);
    }

    #[test]
    fn keyword_hits_do_not_hide_full_path_or_directory_substring_matches() {
        for (query, path) in [
            ("/src/platform", "/src/platform"),
            ("platform", "/src/my-platform"),
            ("PLATFORM", "/src/My-Platform"),
        ] {
            for (cwd, repo_root) in [(Some(path), None), (None, Some(path))] {
                let (_temp, db) = open_db();
                insert(
                    &db,
                    Provider::Claude,
                    "path",
                    "Fix startup",
                    path,
                    "repair launch sequence",
                );
                db.conn
                    .execute(
                        "update sessions set cwd = ?1, repo_root = ?2 where id = 'claude:path'",
                        params![cwd, repo_root],
                    )
                    .unwrap();
                insert(
                    &db,
                    Provider::Claude,
                    "prefix",
                    "src platformer game",
                    "/games/demo",
                    "render sprites",
                );
                insert(
                    &db,
                    Provider::Claude,
                    "other",
                    "Unrelated",
                    "/src/misc",
                    "nothing here",
                );

                let mut hits = hit_ids(&db, query, &filters(None, 25), None);
                hits.sort();
                assert_eq!(
                    hits,
                    vec!["claude:path", "claude:prefix"],
                    "query={query:?}, cwd={cwd:?}, repo_root={repo_root:?}"
                );
            }
        }
    }

    #[test]
    fn several_words_inside_longer_words_match_transcripts() {
        let (_temp, db) = open_db();
        insert(
            &db,
            Provider::Claude,
            "infix",
            "Investigate tooling",
            "/src/app",
            "use ripgrep before helm upgrade",
        );
        assert_eq!(
            hit_ids(&db, "grep grade", &filters(None, 25), None),
            vec!["claude:infix"]
        );
    }

    #[test]
    fn fts_syntax_in_queries_is_treated_literally() {
        let (_temp, db) = open_db();
        insert(
            &db,
            Provider::Claude,
            "a",
            "C++ build",
            "/src/app",
            "c++ linker error NEAR main",
        );
        for query in [
            "\"unbalanced",
            "a OR",
            "NEAR(",
            "c++",
            "-",
            "*",
            "col:value",
            "(x",
        ] {
            db.search(query, &filters(None, 10), None)
                .unwrap_or_else(|err| panic!("query {query:?} failed: {err}"));
        }
        assert_eq!(fts_query("- * ()"), None);
        assert_eq!(
            fts_query("say \"hi\""),
            Some("\"say\"* OR \"\"\"hi\"\"\"*".to_string())
        );
    }

    #[test]
    fn list_recent_applies_filters_and_limit() {
        let (_temp, db) = open_db();
        insert(&db, Provider::Claude, "a", "one", "/src/my_app", "x");
        insert(&db, Provider::Claude, "b", "two", "/src/myxapp", "x");
        insert(&db, Provider::Codex, "c", "three", "/src/my_app", "x");

        let mut by_path = filters(None, 10);
        by_path.path_prefix = Some("/src/my_app".to_string());
        let mut ids: Vec<_> = db
            .list_recent(&by_path)
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect();
        ids.sort();
        // `_` is literal, so "/src/myxapp" must not match.
        assert_eq!(ids, vec!["claude:a", "codex:c"]);
        // Prefixes match regardless of case.
        by_path.path_prefix = Some("/SRC/My_App".to_string());
        assert_eq!(db.list_recent(&by_path).unwrap().len(), 2);
        assert_eq!(db.list_recent(&filters(None, 2)).unwrap().len(), 2);
        assert_eq!(db.list_recent(&filters(None, usize::MAX)).unwrap().len(), 3);
        db.search("one", &filters(None, usize::MAX), None).unwrap();
    }

    #[test]
    fn resolve_session_prefers_exact_ids_and_explains_ambiguity() {
        let (_temp, db) = open_db();
        insert(&db, Provider::Claude, "abc", "short", "/src/app", "x");
        insert(&db, Provider::Claude, "abcdef", "long", "/src/app", "x");

        assert_eq!(db.resolve_session("abc").unwrap().session.id, "claude:abc");
        let err = db.resolve_session("ab").unwrap_err().to_string();
        assert!(
            err.contains("ambiguous") && err.contains("claude:abcdef"),
            "{err}"
        );
        assert!(
            db.resolve_session("a_c").is_err(),
            "`_` must not act as a wildcard"
        );

        insert(
            &db,
            Provider::Pi,
            "2026_10_05_abc",
            "underscored",
            "/src/app",
            "x",
        );
        assert_eq!(
            db.resolve_session("2026_10").unwrap().session.id,
            "pi:2026_10_05_abc"
        );
    }

    #[test]
    fn escape_like_prefix_escapes_underscore_and_percent() {
        assert_eq!(escape_like_prefix("/home/me/my_app"), "/home/me/my\\_app");
        assert_eq!(escape_like_prefix("100%done"), "100\\%done");
        assert_eq!(escape_like_prefix(r"a\b"), r"a\\b");
    }

    #[test]
    fn format_version_bump_forces_reparse_but_keeps_sessions() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("index.db");
        let db = Db::open(&path).unwrap();
        insert(&db, Provider::Claude, "kept", "old", "/src/app", "x");
        db.conn.execute_batch("pragma user_version = 0").unwrap();
        assert!(db
            .is_file_current(Provider::Claude, "/sessions/kept.jsonl", 0, 0, None)
            .unwrap());
        drop(db);

        let db = Db::open(&path).unwrap();
        assert!(!db
            .is_file_current(Provider::Claude, "/sessions/kept.jsonl", 0, 0, None)
            .unwrap());
        assert!(db.resolve_session("claude:kept").is_ok());
        let version: i64 = db
            .conn
            .query_row("pragma user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, super::INDEX_FORMAT_VERSION);
    }

    #[test]
    fn exclude_source_drops_previous_session_and_marks_file_seen() {
        let (_temp, db) = open_db();
        insert(
            &db,
            Provider::Codex,
            "sub",
            "Guardian review",
            "/src/app",
            "parent history",
        );
        insert(
            &db,
            Provider::Codex,
            "main",
            "Real work",
            "/src/app",
            "parent history",
        );

        db.exclude_source(Provider::Codex, "/sessions/sub.jsonl", 7, 8, Some("h"))
            .unwrap();

        assert!(db.resolve_session("codex:sub").is_err());
        assert_eq!(
            hit_ids(&db, "parent history", &filters(None, 10), None),
            vec!["codex:main"]
        );
        assert!(db
            .is_file_current(Provider::Codex, "/sessions/sub.jsonl", 7, 8, Some("h"))
            .unwrap());
    }

    #[test]
    fn reparsing_a_file_under_a_new_id_replaces_the_old_record() {
        let (_temp, db) = open_db();
        insert(
            &db,
            Provider::Codex,
            "old-id",
            "Update models",
            "/src/app",
            "x",
        );
        let mut parsed = db.resolve_session("codex:old-id").unwrap();
        parsed.session.id = "codex:new-id".to_string();
        parsed.session.provider_session_id = "new-id".to_string();
        let reparsed = ParsedSession {
            session: parsed.session,
            transcript_text: "x".to_string(),
        };
        db.upsert_session(&reparsed, 0, 0, None).unwrap();

        assert!(db.resolve_session("codex:old-id").is_err());
        assert_eq!(
            hit_ids(&db, "update models", &filters(None, 10), None),
            vec!["codex:new-id"]
        );
    }

    fn failed_parse(id: &str, source_path: &str) -> ParsedSession {
        let mut failed = crate::util::minimal_record(
            Provider::Codex,
            std::path::Path::new(source_path),
            "Permission denied (os error 13)".to_string(),
        );
        failed.session.id = format!("codex:{id}");
        failed
    }

    #[test]
    fn failed_reads_keep_the_last_good_session_and_are_retried() {
        let (_temp, db) = open_db();
        insert(
            &db,
            Provider::Codex,
            "good",
            "Important conversation",
            "/src/app",
            "x",
        );

        // The file changed (new mtime/size) but couldn't be read this time.
        db.upsert_session(
            &failed_parse("rollout-good", "/sessions/good.jsonl"),
            5,
            6,
            None,
        )
        .unwrap();

        assert_eq!(
            db.resolve_session("codex:good")
                .unwrap()
                .session
                .title
                .as_deref(),
            Some("Important conversation")
        );
        assert!(db.resolve_session("codex:rollout-good").is_err());
        assert!(!db
            .is_file_current(Provider::Codex, "/sessions/good.jsonl", 5, 6, None)
            .unwrap());
    }

    #[test]
    fn failed_reads_of_new_files_are_recorded_with_a_warning_and_retried() {
        let (_temp, db) = open_db();
        db.upsert_session(&failed_parse("new", "/sessions/new.jsonl"), 1, 2, None)
            .unwrap();

        assert_eq!(db.count_parse_warnings().unwrap(), 1);
        assert!(!db
            .is_file_current(Provider::Codex, "/sessions/new.jsonl", 1, 2, None)
            .unwrap());
    }

    #[test]
    fn metadata_hash_participates_in_file_freshness() {
        let temp = tempdir().unwrap();
        let db = Db::open(&temp.path().join("index.db")).unwrap();
        db.conn
            .execute(
                "insert into files_seen (
                    provider, source_path, mtime_ns, size_bytes, last_indexed_at, content_hash
                 ) values (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    Provider::Codex.as_str(),
                    "/tmp/session.jsonl",
                    10_i64,
                    20_i64,
                    "2026-08-01T12:00:00Z",
                    "old-metadata",
                ],
            )
            .unwrap();

        assert!(db
            .is_file_current(
                Provider::Codex,
                "/tmp/session.jsonl",
                10,
                20,
                Some("old-metadata")
            )
            .unwrap());
        assert!(!db
            .is_file_current(
                Provider::Codex,
                "/tmp/session.jsonl",
                10,
                20,
                Some("new-metadata")
            )
            .unwrap());
    }
}
