---
title: Cursor Agent support shipped to fork
date: 2026-10-19
summary: Journaled cook outcome; committing feat/cursor-support and merging into fork main.
---

# Cursor Agent support shipped to fork

## What happened
Implemented Cursor Agent CLI support on worktree `/mnt/sk/repos/worktrees/abtop-cursor-support` (`feat/cursor-support`).

- Added `src/collector/cursor.rs` (FD→store.db, meta.json, agent-transcripts; worker-server excluded; no bare `agent` match)
- Wired MultiCollector + `@CA` UI label; README/AGENTS docs
- Fixed fallback chat claim race
- Verified: 232 tests, clippy clean, live `--json` shows cursor-agent session

## Decision
FD-first discovery; no Cursor quota/tokens until a local source exists. Implement only on the feature worktree.

## Next steps
1. Commit + push `feat/cursor-support` to fork `otakus007/abtop`
2. Merge into fork `main`

> Historical work record — not durable authority. Prefer docs/specs/ADRs for current decisions.
