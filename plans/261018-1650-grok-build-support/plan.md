---
title: "Grok Build CLI support"
description: "Show live Grok Build sessions in abtop from local session files, with tokens, context, status, and subagents. No quota row and no merge."
status: completed
priority: P1
effort: "1d"
tags: [grok, collector]
created: 2026-10-18
---

# Grok Build CLI support

## Contract

- **Outcome:** A running `grok` process appears in the abtop sessions list as agent `grok`, with project, model, title, token totals, context %, status, current tool, child processes, and subagents. `hidden_agents = ["grok"]` hides it. Work stays on this worktree.
- **Constraints:** Read-only local files plus the existing `ps` snapshot. No Grok API, no `grok usage` subprocess, no prompt or tool-body text in the TUI. Match the OpenCode collector shape. Quota stays Claude and Codex only.
- **Non-goals:** Account rate-limit gauges, memory-file counts, `claude --print` titles, merging to the upstream repo, Gemini/Cursor.
- **Acceptance:** Fixture tests cover path encoding, PID disambiguation, token math, status, and tool labels. `cargo test` passes. A live `grok` PID resolves to its `summary.json` directory.

## Decision

Add `GrokCollector` (`agent_cli = "grok"`).

Rejected: shelling out to `grok usage` each tick (slow, couples to the leader socket). Rejected: `active_sessions.json` alone (no tokens, context, or current tool).

## Evidence

Live store is `$GROK_HOME` or `~/.grok`:

- `active_sessions.json`: `{session_id, pid, cwd, opened_at}`. One PID can have many rows; the newest `opened_at` is the fallback.
- Process PWD is not session cwd. Worktree sessions keep PWD on the source repo. Prefer an open fd under `sessions/<group>/<id>/`.
- `summary.json`: `current_model_id`, `generated_title`, `reasoning_effort`, `head_branch`, `created_at`, `session_kind`.
- `usage.json` `session`: `inputTokens` includes cached reads (`total = input + output`). Subtract cache before storing so `AgentSession::total_tokens()` does not double-count.
- `signals.json`: `contextTokensUsed`, `contextWindowTokens`, `turnCount`, `compactionCount`. Absent mid-turn; show zeros until it appears.
- `events.jsonl` phases: `waiting_for_model`, `streaming_reasoning`, `streaming_text`, `tool_execution`, `permission_prompt`.
- `updates.jsonl` tool updates: tool label plus `locations[].path`. Do not read `rawInput`.
- `subagents/<id>/meta.json`: `subagent_type`, `status`. Ignore `prompt`.
- Context windows are not a single hardcoded size (`grok-4.7` is 256k or 500k). Use `signals.json`.

## Phases

| # | Phase | Status |
|---|-------|--------|
| 1 | [Collector](./phase-01-start.md) | Completed |
| 2 | [Wire UI and docs](./phase-02-wire-ui-docs.md) | Completed |

## Success Criteria

- [x] Live Grok sessions show in the sessions panel with label `GK`
- [x] Token total matches `usage.json` session total
- [x] Context % comes from `signals.json` when present
- [x] Subagent sessions are not separate top-level rows
- [x] No quota row and no `claude --print` for Grok titles
- [x] `cargo test` passes
- [x] Changes stay on this worktree

<!-- slug: grok-build-support -->
