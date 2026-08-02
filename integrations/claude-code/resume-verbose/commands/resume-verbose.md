---
allowed-tools: Bash(python3:*)
description: List recent sessions with both timestamps, the reply gap, branch, last prompt, and recap
argument-hint: "[#|session-id] [--all] [--limit N]"
---

## Session listing

!`python3 "${CLAUDE_PLUGIN_ROOT}/scripts/resume_verbose.py" $ARGUMENTS`

## Your task

Relay the output above **verbatim** — it is preformatted for the terminal. Do not
reformat it into a markdown table, re-sort it, or summarize the rows.

Two exceptions, and only these:

1. **If a detail view shows `RECAP  [generated]`**, replace the placeholder line with a
   2–3 sentence recap of the work, derived **only from the LAST MESSAGE RECEIVED block
   above it**. Say what state the work was left in and what the obvious next step is.
   Do not read the transcript file or invent anything the block doesn't support.
2. **If the output is an error or empty**, say so plainly and suggest `--all`.

Then stop. Do not resume anything, read transcripts, or act on what a session was doing —
this command only reports.
