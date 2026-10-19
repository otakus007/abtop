---
phase: 1
title: "Collector"
status: pending
priority: P1
effort: "4h"
dependencies: []
---

# Phase 1: Collector

## Overview

Create `CursorCollector` on the feature worktree: discover live `cursor-agent` PIDs, map them to chat dirs, and build `AgentSession` rows from `meta.json` + agent transcript JSONL.

## Requirements

- Functional: FD-first PID → `store.db` → chat dir; parse `meta.json`; locate and parse transcript; exclude `worker-server`; set `agent_cli = "cursor-agent"`.
- Functional: Fallback cwd/meta scan only for PIDs with no FD hit; one chat per PID.
- Non-functional: No network; defensive `serde(default)` parsing; do not decrypt/parse `store.db` blobs; privacy — tool name + first path arg only.
- Delivery: Worktree must exist before any source edits (see plan delivery gate).

## Architecture

Mirror Codex open-file mapping + OpenCode/Grok session assembly:

1. `find_cursor_agent_pids(process_info)` — `cmd_has_binary(..., "cursor-agent")`, skip commands containing `worker-server` or `grep`.
2. `map_pid_to_chat_dir(pids)` — Linux: `process::scan_proc_fds`; match path under `~/.cursor/chats/` ending in `/store.db` (or `-wal`). Parent of `store.db` is the chat dir (`…/{hash}/{sid}/`).
3. Non-Linux: `lsof`-style open-file scan when available; else fallback meta scan.
4. Read `meta.json`: `cwd`, `title`, `createdAtMs` / `updatedAtMs`.
5. Optional: read SQLite `meta` row for `lastUsedModel` / `agentId` (text/hex JSON only — never blob table).
6. Transcript path: `{cursor_home}/projects/{encode(cwd)}/agent-transcripts/{sid}/{sid}.jsonl` where `encode` strips leading `/` and replaces `/` with `-`.
7. Parse JSONL (full first pass or offset-tail later): last user text → `initial_prompt` (truncated/sanitized); last `tool_use` → `current_tasks` / `tool_calls`; status from transcript mtime + pending tool heuristics (same 30s freshness idea as other collectors).
8. Tokens/context: leave `0` / `context_percent = 0` / `context_window` from `context_window_for_model` only if a real model id is known; otherwise model string from `lastUsedModel` or `"default"`.
9. Children/ports from `SharedProcessData` tree walk (copy OpenCode cycle-guard pattern).
10. `config_root`: abbreviated `~/.cursor`.

## Related Code Files

- Create: `src/collector/cursor.rs`
- Modify: `src/collector/mod.rs` (`mod cursor`, `pub use`, register in `with_hidden_and_claude_config_dirs`, update `with_hidden_*` test counts to include the new collector)
- Create (fixtures): e.g. `src/collector/fixtures/cursor/` or inline tempdirs in unit tests — `meta.json`, minimal JSONL, fake fd path helpers where possible

## Implementation Steps

1. Create feature worktree; all following steps run there.
2. Add `cursor.rs` skeleton implementing `AgentCollector`.
3. Implement PID filter + FD → chat-dir mapping (+ platform cfg stubs).
4. Implement `meta.json` + transcript path resolution + JSONL parse helpers.
5. Assemble `AgentSession` (status, tasks, children, empty tokens).
6. Register module in `mod.rs`; extend hidden-agent unit tests (`collectors.len()` expectations).
7. Unit-test: path encoding, worker-server exclusion, meta→session fields, tool label extraction, fallback does not double-assign PIDs.

## Todo

- [x] Feature worktree created and used as cwd for implementation
- [x] `CursorCollector` discovers live sessions via FD map
- [x] `worker-server` excluded
- [x] Transcript/tools/status populated without secrets
- [x] Tokens/context left honest when unknown
- [x] Collector registered behind `hidden_agents`
- [x] Unit tests for discovery + parse helpers green

## Success Criteria

- [x] `cargo test` covers cursor collector helpers without requiring a live agent
- [x] With a live `cursor-agent`, `--once` / TUI path (after phase 2 wiring) can list the session — collector alone returns non-empty `Vec<AgentSession>` in a manual smoke if wired early
- [x] No reads of encrypted blob payloads; no network

## Risk Assessment

| Risk | Signal | Response |
|------|--------|----------|
| Cursor stops holding `store.db` open | FD map empty for live PID | Use meta cwd fallback; log once if both miss |
| Transcript path encoding changes | File not found despite live chat | Prefer `meta.cwd` + encoded path; add alternate encodings only with evidence |
| Bare `agent` false positive | Wrong CLI listed | Keep binary match on `cursor-agent` only |
| Schema churn in JSONL | Parse yields empty tools | `serde(default)` + tolerate missing fields; degrade to Waiting + title from meta |

## Test / validation gate

`cargo test cursor` (or full `cargo test`) on the worktree after this phase.
