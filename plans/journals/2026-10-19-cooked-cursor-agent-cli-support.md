---
title: Cooked Cursor Agent CLI support
date: 2026-10-19
summary: Implemented CursorCollector on feat/cursor-support worktree; live smoke + tests green.
---

# Cooked Cursor Agent CLI support

Worktree: /mnt/sk/repos/worktrees/abtop-cursor-support (feat/cursor-support).

Added CursorCollector (FD→store.db, meta.json, agent-transcripts JSONL), MultiCollector wiring, @CA UI label, README/AGENTS docs. Fixed fallback claim race. cargo test 232 pass, clippy clean. Live --json shows cursor-agent session.

> Historical work record — not durable authority. Prefer docs/specs/ADRs for current decisions.
