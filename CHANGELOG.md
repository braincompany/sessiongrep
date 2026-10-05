# Changelog

## 0.1.0-rc.1 - 2026-10-05

Release candidate for the first tagged release.

Install this RC with the installer below, or `cargo install sessiongrep --version 0.1.0-rc.1 --locked`. Please report install or indexing problems in GitHub issues.

- Indexes Claude Code, Codex CLI, Cursor, Antigravity, and Pi sessions into a local SQLite + FTS5 index that updates incrementally before every command.
- `sessiongrep`: search, list, show, export, and resume sessions from the CLI or an interactive TUI.
- `sessiongrep-mcp`: MCP server that lets agents search and read past sessions (`search_sessions`, `get_session` with paging, `list_sessions`, `timeline_for_repo`, `get_resume_command`).
- Indexes the conversation only: tool output, injected context, and sub-agent transcripts are left out.
- Prebuilt binaries for macOS (Apple Silicon, Intel) and Linux (x86_64, ARM64), plus a shell installer.
