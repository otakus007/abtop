---
title: Cursor Agent CLI support plan
date: 2026-10-19
summary: Planned FD-first CursorCollector; worktree-before-code; no quota; tokens deferred.
---

# Cursor Agent CLI support plan

Locked brainstorm into a two-phase plan under plans/261019-0003-cursor-agent-support.

Decision: CursorCollector with agent_cli=cursor-agent, FD map to ~/.cursor/chats/.../store.db, enrich from meta.json + agent-transcripts JSONL. Fallback cwd meta scan only for unmatched PIDs. No store.db blob parsing. No quota. Tokens/context may be empty.

Delivery: create feature worktree before cook; implement only there.

Phases: (1) collector (2) UI/docs/tests.

> Historical work record — not durable authority. Prefer docs/specs/ADRs for current decisions.
