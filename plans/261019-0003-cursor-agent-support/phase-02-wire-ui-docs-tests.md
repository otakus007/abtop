---
phase: 2
title: "Wire UI, docs, tests"
status: pending
priority: P1
effort: "3h"
dependencies: [1]
---

# Phase 2: Wire UI, docs, tests

## Overview

Surface `cursor-agent` sessions in the TUI and docs, keep quota Claude/Codex-only, finish integration tests, and verify on the feature worktree with a live process when available.

## Requirements

- Functional: Sessions table shows a dedicated label/color for `cursor-agent` (recommend `@CA`); snapshot/`--once` includes the agent; README/AGENTS fork docs mention Cursor Agent CLI support and the no-quota / tokens-may-be-empty caveat.
- Functional: MEM line and Claude-only UI paths stay gated on `agent_cli == "claude"` (no Cursor memory panel).
- Non-functional: English-only docs; privacy rules unchanged; `hidden_agents` documented with `"cursor-agent"`.
- Non-goals: Quota panel row; inventing token rates for sparklines when totals are zero.

## Architecture

| Area | Change |
|------|--------|
| UI label | `src/ui/sessions.rs` match arm: `"cursor-agent" => ("@CA", <color>)` — pick a color distinct from CC/CD/OC (e.g. soft violet or teal; avoid purple-glow SaaS cliché if a calmer ink exists in theme) |
| Locale (optional) | `src/locale.rs` `agent.cursor-agent` keys if other agents are localized the same way; sessions.rs may keep hardcoded labels like OC today |
| Snapshot docs | `src/snapshot.rs` comment listing owning CLIs |
| Model comments | `src/model/session.rs` config_root / LaunchSurface docs mention Cursor CLI |
| Docs | `README.md` support matrix + discovery blurb; `AGENTS.md` Data Sources section + adjust Non-Goals for this fork |
| Demo (only if required for compile) | `src/demo.rs` if agent lists are exhaustive |

Quota UI (`src/ui/quota.rs` `SOURCES`) stays `["claude", "codex"]` — do not add Cursor.

## Related Code Files

- Modify: `src/ui/sessions.rs`
- Modify: `src/snapshot.rs` (comments / any exhaustive docs)
- Modify: `src/model/session.rs` (comments)
- Modify: `README.md`, `AGENTS.md` (fork-facing)
- Modify (if needed): `src/locale.rs`, `src/demo.rs`
- Verify: `src/collector/mod.rs` registration from phase 1; `src/ui/quota.rs` untouched for sources

## Implementation Steps

1. Add sessions label/color for `cursor-agent`.
2. Update README feature matrix and discovery notes (local chats + transcripts; no quota; tokens may be empty).
3. Update `AGENTS.md` architecture/data-sources for the fork; document binary match caveat (`cursor-agent` vs `agent`).
4. Fix any doc comments that claim only claude/codex/opencode.
5. Integration-style tests: MultiCollector with cursor not hidden increases collector count; hidden drops it; fixture-driven session fields if not fully covered in phase 1.
6. Run `cargo test`, `cargo clippy -- -D warnings`, optional `cargo run -- --once` against a live `cursor-agent`.
7. Manual check: Enter-jump still uses existing jump adapters (PID-based) — no new jumper required unless smoke fails.

## Todo

- [x] UI label `@CA` (or agreed equivalent) for `cursor-agent`
- [x] README + AGENTS updated for fork support + limitations
- [x] Quota sources unchanged
- [x] Hidden-agent + fixture tests green
- [x] clippy + test suite green on worktree
- [x] Live smoke: session appears when `cursor-agent` is running

## Success Criteria

- [x] TUI lists Cursor Agent sessions with correct project and task line
- [x] Docs state no Cursor quota and possible empty token/context fields
- [x] `hidden_agents = ["cursor-agent"]` works end-to-end
- [x] No regression in Claude/Codex/OpenCode collector counts when Cursor is hidden
- [x] Implementation remains only on the feature worktree

## Risk Assessment

| Risk | Signal | Response |
|------|--------|----------|
| Label collision / unreadability | Users confuse with Codex | Keep `@CA` distinct glyph/color; document in README table |
| Docs drift vs upstream Non-Goals | AGENTS still says Cursor non-goal | Explicit fork note: this fork adds Cursor Agent CLI; upstream non-goal unchanged for merge intent |
| Empty tokens look like a bug | Users report “always 0” | Document limitation next to OpenCode sqlite caveat style |
| Jump fails for agent tty | Enter no-ops | Reuse existing PID jump; file follow-up only if reproducible |

## Test / validation gate

```bash
cargo test
cargo clippy -- -D warnings
cargo run -- --once   # with a live cursor-agent when available
```
