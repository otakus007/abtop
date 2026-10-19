---
title: Grok Build collector in abtop
date: 2026-10-18
summary: Local session files feed a grok row; quota stays Claude and Codex.
---

# Grok Build collector in abtop

## What happened
Added `src/collector/grok.rs` and registered it beside Claude, Codex, and OpenCode. Live `grok` PIDs resolve through open session fds, with the newest `active_sessions.json` row as fallback. `usage.json` cache reads are subtracted so session totals are not double-counted. `signals.json` supplies context %. `generated_title` is stored directly so abtop does not call `claude --print`.

## Decision
Do not shell out to `grok usage`, and do not add a quota row. Subagent sessions stay off the top-level list and show from `subagents/*/meta.json`.

## Next steps
Changes are on this worktree and are not merged. `cargo test` passed (232) and `cargo clippy --all-targets -- -D warnings` is clean. A live collect saw both running grok processes. This worktree's session had no `usage.json` or `signals.json` yet, so tokens and context stayed at 0 until the turn finishes.

> Historical work record — not durable authority. Prefer docs/specs/ADRs for current decisions.
