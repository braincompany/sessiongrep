# resume-verbose

`claude --resume` shows a title and a relative mtime. That distinguishes two sessions from
this morning. It does not distinguish ten sessions across four worktrees from the last two
weeks — and on this machine **81 sessions share a single `cwd`**.

`/resume-verbose` adds the three fields that actually identify a session: **when you last
spoke to the agent, when it last answered, and the gap between them** — plus the branch,
your last prompt in full, and a recap.

```
Recent sessions — /Users/you/Documents/GitHub/bowhead        (6 of 156)

  #  last sent             last recv             Δ       msgs   branch
  1  9h ago    12:00        8h ago    12:05        5m       132    fix/ins-2943-prevent-cross-tower-overlaps
     ↳ - use limit_id - limit-selection, need more info - where does the QS inheritance live now? …
  5  2d ago    Jul 30 01:28 2d ago    Jul 30 15:41 14h12m   208    fix/ins-2943-prevent-cross-tower-overlaps
     ↳ Fixed - the EL and WC policies are the same policy. I deduped, check again.

  Δ = gap between your last message and the agent's reply.  ✓ wrapped   ⚠ ended mid-turn
```

**Δ is the point.** A large or missing gap means the session was interrupted — usually
exactly the one you're looking for. Row 5's `14h12m` above is a session that was left
mid-thought two days ago.

## Usage

```
/resume-verbose                 # 10 most recent sessions in this project
/resume-verbose --all           # every project
/resume-verbose --limit 25
/resume-verbose 5               # drill into row 5 of the last listing
/resume-verbose d382243f        # …or by session id prefix
```

The detail view shows the full last prompt, the last assistant reply, and a recap — taken
from your `/wrap` notes in `~/plans` when one exists, otherwise summarized from the last
reply and labelled `[generated]` so the two are never confused.

## Install

From a checkout of [sessiongrep](https://github.com/braincompany/sessiongrep):

```
/plugin marketplace add <path-to-sessiongrep>/integrations/claude-code
/plugin install resume-verbose
```

## How it reads your sessions

Everything is **read-only**; no transcript or index is ever modified.

| Source | Used for |
|---|---|
| `~/.local/share/sessiongrep/index.db` | session list, cwd, branch, both timestamps, last reply, message count |
| `~/.claude/history.jsonl` | last prompt + its timestamp |
| `~/.claude/projects/*/*.jsonl` (last 256 KB) | CLI version; anything the index lacks |
| `~/plans/SESSIONS.md`, `~/plans/*/session-summary*.md` | `/wrap` recaps, ✓ wrapped marker |

The index is the fast path, not a requirement. Three tiers, degrading cleanly:

1. **Index with the session-identity columns** — branch, `last_user_message_at`,
   `last_assistant_message_at`, and `last_assistant_text` come straight from SQLite.
2. **Older index** — those columns are absent; the script derives them from the transcript
   tail instead. Detected via `pragma table_info`, not assumed.
3. **No sessiongrep at all** — globs `~/.claude/projects` directly. Everything works except
   `msgs` and `started`.

The prompt always comes from `history.jsonl` rather than a tail read: long sessions can hold
no user record in their last 256 KB, so tailing reports `—` for sessions that plainly have a
prompt. Subagent output (`isSidechain`) is excluded — otherwise a session ending in a
delegated task reports the subagent's words as the agent's reply.

Navigation commands are filtered from "last message sent": `/resume`, `/clear`, `/compact`
and friends are things you did *to* the session, not work you were doing in it. Commands
that carry arguments and intent — `/wrap save the full session ID` — are kept.

## Known limits

- `Δ` is meaningless for a session that is still running; it reads as a long gap.
- Sessions predating `history.jsonl`, or created by forking, fall back to the transcript's
  `last-prompt` record.
- Only Claude Code sessions. sessiongrep indexes Codex and Cursor too; this doesn't read them yet.

Design notes and the upstream sessiongrep changes this anticipates:
`~/plans/claude/resume-verbose-plugin.md`.
