---
phase: 1
title: "Grok collector"
status: pending
priority: P1
effort: "4h"
dependencies: []
---

# Phase 1: Grok collector

## Overview

Read live Grok Build sessions from the local session store and map them onto `AgentSession`.

## Requirements

- Functional: discover `grok` PIDs from shared `ps` data; bind each PID to one session directory; fill tokens, context, status, current tool, subagents, and children.
- Non-functional: no network, no prompt text, defensive `serde` defaults, tests on fixtures.

## Architecture

1. PID whose command basename is `grok`.
2. Session directory from an open file under `.../sessions/<group>/<id>/` (Linux `/proc/<pid>/fd`). Fallback: newest `active_sessions.json` row for that PID, group name = percent-encoded cwd (unreserved bytes left as-is, hex uppercase). If the encoded name is longer than 255 bytes, scan one directory level for `<id>`.
3. Skip `session_kind = subagent`.
4. Read `summary.json`, `usage.json`, `signals.json`. Tail the last 256KB of `events.jsonl` and `updates.jsonl`.
5. Status: file age > 30s → Waiting; `permission_prompt` → Waiting; `tool_execution` → Executing; streaming or `waiting_for_model` → Thinking.

## Related Code Files

- Create: `src/collector/grok.rs`
- Modify: `src/collector/mod.rs`
- Delete: none

## Implementation Steps

1. Add pure parsers and fixture tests.
2. Add `GrokCollector` and register it when `grok` is not hidden.
3. Update collector-count tests from 3 agents to 4.

## Success Criteria

- [x] Encoding of `/home/tuannh/.grok/worktrees/repos-abtop/grok-support` matches the on-disk group name
- [x] Newest `opened_at` wins when one PID has several rows
- [x] Usage input 100 + cache 40 + output 10 reports `total_tokens() == 110`
- [x] A `tool_execution` tail sets Executing and keeps a file path, not a tool body
- [x] `session_kind = subagent` is omitted from the top-level list

## Risk Assessment

Grok's session schema is undocumented and already split (`signals.json` missing on a live session, `usage.json` written on turn end). Missing files must yield an empty metric, not a dropped session. If `inputTokens` stops including cache, the total test fails and the subtract step must be removed.
