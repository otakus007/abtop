---
title: "Cursor Agent CLI support"
description: "Show live cursor-agent sessions in abtop from local chat/transcript files. No quota; tokens/context deferred when unavailable."
status: completed
priority: P1
effort: "1d"
tags: [cursor-agent, collector]
blockedBy: []
blocks: []
created: 2026-10-19
---

# Cursor Agent CLI support

## Contract

- **Outcome:** A running `cursor-agent` process appears in the abtop sessions list as agent `cursor-agent`, with project, status, title/prompt summary, current tool/task, children, and ports. `hidden_agents = ["cursor-agent"]` hides it. Implementation happens only on a dedicated git worktree.
- **Constraints:** Read-only local filesystem + existing shared `ps`/`lsof`/`/proc` snapshot. No Cursor API. No prompt or tool-body content in the TUI (tool name + first arg path only). Match `AgentCollector` + OpenCode/Grok collector shape. Quota panel stays Claude + Codex only. Match binary `cursor-agent` only — never bare `agent` (can be an unrelated CLI).
- **Non-goals:** Cursor IDE Composer / ACP desktop sessions; account rate-limit gauges; inventing token/context % from encrypted `store.db` blobs; cost estimation; upstream merge unless requested later.
- **Acceptance:** Live `cursor-agent` PID maps to its chat dir + transcript; fixture tests cover discovery, PID/`worker-server` exclusion, meta/transcript parsing, and status/task labels; `cargo test` + `cargo clippy -- -D warnings` pass on the feature worktree; tokens/context may be zero/`—` when no reliable local source exists.

## Decision

Add `CursorCollector` (`agent_cli = "cursor-agent"`).

**Chosen:** FD-first discovery (Codex-like): live PID → open `~/.cursor/chats/{hash}/{sessionId}/store.db` → sibling `meta.json` + `~/.cursor/projects/{encoded-cwd}/agent-transcripts/{sessionId}/{sessionId}.jsonl`.

**Fallback (secondary):** when FD mapping misses, scan recent `~/.cursor/chats/*/meta.json` and match `cwd` to the process cwd — only for unmatched PIDs; never assign one chat to multiple PIDs.

**Rejected:** parsing encrypted `store.db` blobs for token parity (unstable, high blast radius). Rejected: treating bare `agent` as Cursor (collides with other tools). Rejected: quota row without a local account-level source.

## Evidence

Confirmed on a live Linux `cursor-agent` (2026-10-18):

| Source | Path / signal | Use |
|--------|---------------|-----|
| Process | `/usr/bin/cursor-agent … /opt/cursor-agent/index.js` | Discovery; exclude `worker-server` |
| Chat DB | `~/.cursor/chats/{hash}/{sid}/store.db` (+ WAL) | Ownership via open FD; do not parse blobs |
| Meta | `…/meta.json` | `cwd`, `title`, `createdAtMs`, `updatedAtMs` |
| Store meta | SQLite `meta` key/value | `agentId`, `lastUsedModel`, `mode` (optional) |
| Transcript | `~/.cursor/projects/{encoded-cwd}/agent-transcripts/{sid}/{sid}.jsonl` | Roles, `tool_use` name/args; **no usage tokens observed** |
| Config root | `~/.cursor` | `config_root` display |

Path encoding for projects: `/mnt/sk/repos/abtop` → `mnt-sk-repos-abtop` (strip leading `/`, replace `/` with `-`).

Sibling reference (completed, other worktree): `plans/261018-1650-grok-build-support` / `GrokCollector` — same “collector + UI label + no quota” pattern.

## Scope challenge (HOLD)

1. **Exists:** `AgentCollector`, `MultiCollector::with_hidden_*`, Codex FD mapping (`scan_proc_fds`), OpenCode/Grok collector templates, sessions label match, `hidden_agents`.
2. **Asked:** Cursor Agent CLI support; plan before code; implement on a new worktree.
3. **Complexity:** ~6–8 files; one new collector module; two phases. No new services.

Unrequested additions rejected: IDE Composer support, encrypted-blob reverse engineering, quota hacking.

## Delivery gate (before cook)

1. Create an isolated worktree (`ak-worktree` / `feat/cursor-agent-support` or equivalent).
2. Implement **only** inside that worktree.
3. Do not start coding on `main` checkout.

## Phases

| # | Phase | Status |
|---|-------|--------|
| 1 | [Collector](./phase-01-start.md) | Completed |
| 2 | [Wire UI, docs, tests](./phase-02-wire-ui-docs-tests.md) | Completed |

## Architecture

```text
SharedProcessData (ps + ports)
        │
        ▼
 CursorCollector::collect
        │
        ├─ find cursor-agent PIDs (exclude worker-server)
        ├─ map PID → store.db via /proc fd (Linux) or lsof (else)
        ├─ read meta.json (+ optional store meta for model)
        ├─ resolve transcript path from meta.cwd + sessionId
        ├─ tail/parse JSONL → tools, status, title, chat tail
        └─ attach children/ports from SharedProcessData
        │
        ▼
 AgentSession { agent_cli: "cursor-agent", … }
        │
 MultiCollector → App / UI (label @CA) / snapshot
```

## Success Criteria

- [x] Live `cursor-agent` sessions show in the sessions panel with a dedicated label (e.g. `@CA`)
- [x] Correct project/cwd from `meta.json`; session id matches chat dir
- [x] Status + current task derived from transcript mtime / last `tool_use`
- [x] `worker-server` never appears as its own session row
- [x] No quota row for Cursor; tokens/context honest zeros when unknown
- [x] `hidden_agents = ["cursor-agent"]` disables the collector
- [x] `cargo test` and `cargo clippy -- -D warnings` pass on the feature worktree
- [x] All implementation commits happen on the feature worktree only

## Open Questions

None for v1. Revisit tokens/context only if Cursor starts writing usage into a readable local file.

## Mode / gates

Planning mode: **fast** (brainstorm already locked discovery + non-goals). Red-team/validate not auto-run; optional before cook if desired.

<!-- slug: cursor-agent-support -->
