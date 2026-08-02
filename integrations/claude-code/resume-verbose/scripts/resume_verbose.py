#!/usr/bin/env python3
"""Verbose session listing for Claude Code — see PLAN at ~/plans/claude/.

Answers "which session was I in?" without resuming each candidate: role-split
timestamps, the gap between them, branch, the last prompt, and a recap.

Reads the sessiongrep index when present (fast path) and falls back to globbing
~/.claude/projects. Everything is read-only.
"""
from __future__ import annotations

import argparse, glob, json, os, re, shutil, sqlite3, sys, textwrap
from datetime import datetime, timezone

HOME = os.path.expanduser("~")
SG_DB = os.path.join(HOME, ".local/share/sessiongrep/index.db")
PROJECTS = os.path.join(HOME, ".claude/projects")
HISTORY = os.path.join(HOME, ".claude/history.jsonl")
SESSIONS_MD = os.path.join(HOME, "plans/SESSIONS.md")
PLANS = os.path.join(HOME, "plans")
STATE = os.path.join(HOME, ".claude/resume-verbose-last.json")
TAIL = 256 * 1024

UUID_RE = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")

# Two rules for discarding a prompt as the "last message sent":
#   1. Any slash command with no arguments — the name alone says nothing about the work.
#   2. A navigation/housekeeping command, arguments or not. `/resume <uuid>` and
#      `/compact` are things you did *to* the session, not work you were doing in it.
# Everything else with arguments is kept: `/wrap save the full session ID` and
# `/summary-load ~/plans/…` carry real intent and are good recall anchors.
SLASH_RE = re.compile(r"^/([a-z0-9:_-]+)(\s+(?P<args>.*))?$", re.I | re.S)
NAV_COMMANDS = frozenset("""
    resume clear compact rewind export context usage cost status doctor help
    plugin reload-plugins marketplace mcp ide agents model config login logout
    exit quit upgrade release-notes terminal-setup vim statusline output-style
    permissions add-dir hooks memory privacy-settings fast effort todos bug
    resume-verbose
""".split())


def substantive_prompt(text):
    """False for prompts that tell you nothing about what the session was doing."""
    t = (text or "").strip()
    if not t:
        return False
    m = SLASH_RE.match(t)
    if not m:
        return True
    name = m.group(1).split(":")[-1].lower()      # plugin commands: /plugin:cmd
    if name in NAV_COMMANDS:
        return False
    return bool((m.group("args") or "").strip())


# ---------------------------------------------------------------- time helpers

def parse_iso(s):
    if not s:
        return None
    try:
        return datetime.fromisoformat(str(s).replace("Z", "+00:00"))
    except ValueError:
        return None


def rel(ts, now):
    if not ts:
        return "—"
    s = (now - ts).total_seconds()
    if s < 90:
        return "just now"
    if s < 3600:
        return f"{int(s // 60)}m ago"
    if s < 86400:
        return f"{int(s // 3600)}h ago"
    return f"{int(s // 86400)}d ago"


def absol(ts, now):
    """Time only for today; date + time otherwise. Calendar day, not a 24h window."""
    if not ts:
        return "—"
    lt = ts.astimezone()
    return lt.strftime("%H:%M") if lt.date() == now.astimezone().date() else lt.strftime("%b %d %H:%M")


def gap(a, b):
    if not a or not b:
        return "—"
    s = (b - a).total_seconds()
    if s < 0:
        return "—"
    if s < 60:
        return f"{int(s)}s"
    if s < 3600:
        return f"{int(s // 60)}m"
    if s < 86400:
        return f"{int(s // 3600)}h{int((s % 3600) // 60):02d}m"
    return f"{int(s // 86400)}d"


def clip(s, n):
    s = " ".join((s or "").split())
    return s if len(s) <= n else s[: max(1, n - 1)] + "…"


# ------------------------------------------------------------------- transcript

def tail_records(path, nbytes=TAIL):
    try:
        size = os.path.getsize(path)
        with open(path, "rb") as fh:
            if size > nbytes:
                fh.seek(size - nbytes)
                fh.readline()          # discard the partial first line
            raw = fh.read()
    except OSError:
        return []
    out = []
    for line in raw.split(b"\n"):
        if not line.strip():
            continue
        try:
            out.append(json.loads(line))
        except (ValueError, UnicodeDecodeError):
            continue                   # truncated final line on a live session
    return out


def text_of(message):
    c = message.get("content")
    if isinstance(c, str):
        return c
    if isinstance(c, list):
        return " ".join(b.get("text", "") for b in c
                        if isinstance(b, dict) and b.get("type") == "text")
    return ""


def scan_transcript(path, fields=None):
    """Fill in whatever the index couldn't. `fields` limits the work when the index
    already carries the identity columns; the tail read is the same either way."""
    d = {"recv_ts": None, "recv_text": "", "branch": None, "version": None,
         "sent_ts": None, "sent_text": ""}
    for r in tail_records(path):
        if r.get("gitBranch"):
            d["branch"] = r["gitBranch"]
        if r.get("version"):
            d["version"] = r["version"]
        if r.get("isSidechain"):
            continue                   # subagent output is not what *you* were told
        rtype, ts = r.get("type"), parse_iso(r.get("timestamp"))
        if rtype == "assistant" and ts:
            body = text_of(r.get("message", {})).strip()
            if body:
                d["recv_ts"], d["recv_text"] = ts, body
        elif rtype == "user" and ts and not r.get("isMeta"):
            body = text_of(r.get("message", {})).strip()
            if body and not body.startswith("<") and substantive_prompt(body):
                d["sent_ts"], d["sent_text"] = ts, body
        elif rtype == "last-prompt" and not d["sent_text"]:
            if substantive_prompt(r.get("lastPrompt")):
                d["sent_text"] = r["lastPrompt"]
    return {k: v for k, v in d.items() if fields is None or k in fields}


# ------------------------------------------------------------------ index reads

def history_index():
    """sessionId -> (prompt, ts). Complete where a tail scan is not: long sessions
    can hold no user record in their last 256 KB."""
    best = {}
    try:
        fh = open(HISTORY, encoding="utf-8", errors="replace")
    except OSError:
        return {}
    with fh:
        for line in fh:
            try:
                o = json.loads(line)
            except ValueError:
                continue
            sid, ts, disp = o.get("sessionId"), o.get("timestamp"), (o.get("display") or "")
            if not sid or not ts or not substantive_prompt(disp):
                continue               # /clear, /resume <uuid> … tell you nothing
            if sid not in best or ts > best[sid][1]:
                best[sid] = (disp, ts)
    return {k: (t, datetime.fromtimestamp(ms / 1000, timezone.utc)) for k, (t, ms) in best.items()}


def wrapped_index():
    """session id -> /wrap recap from ~/plans/SESSIONS.md."""
    out = {}
    try:
        md = open(SESSIONS_MD, encoding="utf-8", errors="replace").read()
    except OSError:
        return out
    for line in md.splitlines():
        if not line.startswith("|"):
            continue
        m = UUID_RE.search(line)
        if m:
            cells = [c.strip() for c in line.split("|")]
            out[m.group(0)] = cells[-2] if len(cells) > 2 else ""
    return out


def summary_index():
    """session id (full or 8-char) -> path of a ~/plans session-summary file."""
    out = {}
    for pat in ("session-summary*.md", "SUMMARY-*.md"):
        for path in glob.glob(os.path.join(PLANS, "*", pat)):
            try:
                head = open(path, encoding="utf-8", errors="replace").read(2000)
            except OSError:
                continue
            m = UUID_RE.search(head)
            if m:
                out[m.group(0)] = path
                continue
            m = re.search(r"session[s]?[ `]+([0-9a-f]{8})\b", head, re.I)
            if m:
                out.setdefault(m.group(1), path)
    return out


def sessions_from_sessiongrep(project, limit, all_projects):
    """Read the index. Prefers the session-identity columns added in sessiongrep #28;
    on an older index those are absent and the transcript scan supplies them instead."""
    try:
        con = sqlite3.connect(f"file:{SG_DB}?mode=ro", uri=True)
        have = {r[1] for r in con.execute("pragma table_info(sessions)")}
    except sqlite3.Error:
        return None
    extra = [c for c in ("git_branch", "last_user_message_at",
                         "last_assistant_message_at", "last_assistant_text")
             if c in have]
    cols = ["provider_session_id", "title", "cwd", "updated_at", "message_count",
            "source_path", "created_at"] + extra
    q = f"select {', '.join(cols)} from sessions where provider='claude'"
    args = []
    if not all_projects and project:
        q += " and (cwd = ? or cwd like ?)"        # not LIKE 'x%' — that matches x2, x3
        args += [project, project.rstrip("/") + "/%"]
    q += " order by updated_at desc limit ?"
    args.append(limit)
    try:
        rows = con.execute(q, args).fetchall()
        total = con.execute("select count(*) from sessions where provider='claude'").fetchone()[0]
    except sqlite3.Error:
        return None
    out = []
    for r in rows:
        rec = dict(sid=r[0], title=r[1], cwd=r[2], msgs=r[4], path=r[5],
                   created=parse_iso(r[6]))
        indexed = dict(zip(extra, r[7:]))
        rec["branch"] = indexed.get("git_branch")
        rec["sent_ts"] = parse_iso(indexed.get("last_user_message_at"))
        rec["recv_ts"] = parse_iso(indexed.get("last_assistant_message_at"))
        rec["recv_text"] = indexed.get("last_assistant_text") or ""
        out.append(rec)
    return out, total


def sessions_from_disk(project, limit, all_projects):
    """Fallback when sessiongrep isn't installed."""
    files = glob.glob(os.path.join(PROJECTS, "*", "*.jsonl"))
    total = len(files)
    files.sort(key=lambda p: os.path.getmtime(p), reverse=True)
    out = []
    for path in files:
        if len(out) >= limit:
            break
        sid = os.path.basename(path)[:-6]
        recs = tail_records(path, 32 * 1024)
        cwd = next((r.get("cwd") for r in reversed(recs) if r.get("cwd")), None)
        if not all_projects and project and cwd:
            if cwd != project and not cwd.startswith(project.rstrip("/") + "/"):
                continue
        title = next((r.get("lastPrompt") for r in reversed(recs) if r.get("type") == "last-prompt"), None)
        out.append(dict(sid=sid, title=title or "(session)", cwd=cwd, msgs=None,
                        path=path, created=None))
    return out, total


# ---------------------------------------------------------------------- render

def build(project, limit, all_projects):
    # Over-fetch: the index orders by its own updated_at, but the real ordering key is
    # max(last sent, last recv), which is only known after enrichment. Sort, then trim.
    fetch = max(limit * 3, limit + 10)
    got = sessions_from_sessiongrep(project, fetch, all_projects)
    rows, total = got if got else sessions_from_disk(project, fetch, all_projects)
    hist, wrapped, summaries = history_index(), wrapped_index(), summary_index()
    for r in rows:
        # Scan the transcript only for what the index couldn't supply. With sessiongrep #28
        # that's just the prompt text and the CLI version; on an older index, everything.
        if all(r.get(k) for k in ("branch", "sent_ts", "recv_ts", "recv_text")):
            scanned = scan_transcript(r["path"], fields=("sent_text", "version"))
            r.setdefault("version", scanned.get("version"))
            r["sent_text"] = scanned.get("sent_text", "")
        else:
            for key, value in scan_transcript(r["path"]).items():
                if not r.get(key):
                    r[key] = value
        if r["sid"] in hist:                        # history beats the tail
            r["sent_text"], r["sent_ts"] = hist[r["sid"]]
        r["wrapped"] = r["sid"] in wrapped or r["sid"] in summaries or r["sid"][:8] in summaries
        r["recap"] = wrapped.get(r["sid"], "")
        r["summary_path"] = summaries.get(r["sid"]) or summaries.get(r["sid"][:8])
    epoch = datetime.fromtimestamp(0, timezone.utc)
    rows.sort(key=lambda r: max(r["sent_ts"] or epoch, r["recv_ts"] or epoch), reverse=True)
    return rows[:limit], total


def render_list(rows, total, scope, now):
    width = shutil.get_terminal_size((120, 24)).columns
    # No title column: sessiongrep's title *is* the last prompt, which the ↳ line
    # already shows in full width. Branch takes the space instead — it's the field
    # that actually separates 81 sessions sharing one cwd.
    br_w = max(20, width - 68)
    print(f"\nRecent sessions — {scope}        ({len(rows)} of {total})\n")
    print(f" {'#':>2}  {'last sent':<21} {'last recv':<21} {'Δ':<6} {'msgs':>5}   branch")
    for i, r in enumerate(rows, 1):
        stale = r["sent_ts"] and (not r["recv_ts"] or r["recv_ts"] < r["sent_ts"])
        mark = "✓" if r["wrapped"] else ("⚠" if stale else " ")
        sent = f"{rel(r['sent_ts'], now):<9} {absol(r['sent_ts'], now):<12}"
        recv = f"{rel(r['recv_ts'], now):<9} {absol(r['recv_ts'], now):<12}"
        br = r["branch"] or "(none)"
        br = ("…" + br[-(br_w - 1):]) if len(br) > br_w else br
        print(f" {i:>2}  {sent:<21} {recv:<21} {gap(r['sent_ts'], r['recv_ts']):<6} "
              f"{r['msgs'] if r['msgs'] is not None else '?':>5} {mark}  {br}")
        print(f"     ↳ {clip(r['sent_text'], max(40, width - 8))}")
    print("\n  Δ = gap between your last message and the agent's reply.  ✓ wrapped   ⚠ ended mid-turn")
    print("  Drill in: /resume-verbose <#>      Resume: claude --resume <id>\n")


def render_detail(r, now):
    width = min(shutil.get_terminal_size((100, 24)).columns, 100)
    rule = lambda label: print(f"\n{label} " + "─" * max(4, width - len(label) - 1))
    print(f"\nSession {r['sid']}" + ("   ✓ wrapped" if r["wrapped"] else "   ⚠ not wrapped"))
    print(f"  project   {r['cwd'] or '(unknown)'}")
    print(f"  branch    {r['branch'] or '(none)'}")
    print(f"  version   {r['version'] or '?'}        messages  {r['msgs'] if r['msgs'] is not None else '?'}")
    if r["created"]:
        print(f"  started   {absol(r['created'], now)}")
    print(f"  last sent {absol(r['sent_ts'], now)} ({rel(r['sent_ts'], now)})"
          f"   last recv {absol(r['recv_ts'], now)} ({rel(r['recv_ts'], now)})"
          f"   Δ {gap(r['sent_ts'], r['recv_ts'])}")

    rule("LAST MESSAGE SENT")
    for ln in textwrap.wrap(" ".join((r["sent_text"] or "(none)").split()), width - 2)[:10]:
        print("  " + ln)
    rule("LAST MESSAGE RECEIVED")
    for ln in textwrap.wrap(" ".join((r["recv_text"] or "(none)").split()), width - 2)[:12]:
        print("  " + ln)

    if r["recap"]:
        src = "/wrap · SESSIONS.md"
    elif r["summary_path"]:
        src = os.path.relpath(r["summary_path"], HOME)
    else:
        src = "generated"
    rule(f"RECAP  [{src}]")
    body = r["recap"]
    if not body and r["summary_path"]:
        try:
            body = open(r["summary_path"], encoding="utf-8", errors="replace").read(1200)
        except OSError:
            body = ""
    if body:
        for ln in textwrap.wrap(" ".join(body.split()), width - 2)[:12]:
            print("  " + ln)
    else:
        print("  (no /wrap recap — summarize LAST MESSAGE RECEIVED above)")
    print(f"\n  ▸ claude --resume {r['sid']}\n")


def main():
    ap = argparse.ArgumentParser(prog="/resume-verbose", add_help=False)
    ap.add_argument("selector", nargs="?", help="row number from the last listing, or a session id")
    ap.add_argument("--limit", type=int, default=10)
    ap.add_argument("--all", action="store_true", help="every project, not just this one")
    ap.add_argument("--project", default=os.getcwd())
    ap.add_argument("-h", "--help", action="help")
    a = ap.parse_args()
    now = datetime.now(timezone.utc)

    # Drill-in by row number resolves against the previous listing.
    if a.selector and a.selector.isdigit():
        try:
            prev = json.load(open(STATE))
        except (OSError, ValueError):
            print("No previous listing — run /resume-verbose first.", file=sys.stderr)
            return 1
        ids = prev.get("ids", [])
        n = int(a.selector)
        if not 1 <= n <= len(ids):
            print(f"Row {n} out of range (1–{len(ids)}).", file=sys.stderr)
            return 1
        a.selector = ids[n - 1]

    if a.selector:                                   # detail view
        rows, _ = build(a.project, 500, all_projects=True)
        match = [r for r in rows if r["sid"].startswith(a.selector)]
        if not match:
            print(f"No session matching '{a.selector}'.", file=sys.stderr)
            return 1
        render_detail(match[0], now)
        return 0

    rows, total = build(a.project, a.limit, a.all)   # list view
    if not rows:
        print(f"\nNo sessions for {a.project}. Try --all.\n")
        return 0
    try:
        json.dump({"ids": [r["sid"] for r in rows]}, open(STATE, "w"))
    except OSError:
        pass                                         # row-number drill-in is a convenience
    render_list(rows, total, "all projects" if a.all else a.project, now)
    return 0


if __name__ == "__main__":
    sys.exit(main())
