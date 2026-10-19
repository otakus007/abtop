---
phase: 2
title: "Wire UI and docs"
status: pending
priority: P1
effort: "2h"
dependencies: [1]
---

# Phase 2: Wire UI and docs

## Overview

Surface Grok sessions in the existing TUI and document the support boundary.

## Requirements

- Functional: sessions table badge, kill allow-list, title without `claude --print`, README and AGENTS.md.
- Non-functional: no quota-panel row.

## Architecture

`ui/sessions.rs` matches `agent_cli`. `app.rs` treats `grok` as a killable agent and stores `generated_title` directly so summary jobs do not call Claude. Quota rendering is unchanged because `GrokCollector::live_rate_limit` stays `None`.

## Related Code Files

- Create: none
- Modify: `src/ui/sessions.rs`, `src/app.rs`, `src/snapshot.rs`, `README.md`, `AGENTS.md`
- Delete: none

## Implementation Steps

1. Add the `GK` badge color.
2. Accept `grok` in `is_supported_agent_command`.
3. Skip `claude --print` when `agent_cli` is `grok` and a title is already present.
4. Document discovery, the token rule, and the quota non-goal.

## Success Criteria

- [x] A Grok row renders as `GK`
- [x] `x` can target a `grok` process and still refuses unrelated commands
- [x] Summary drain does not spawn `claude` for a Grok title
- [x] README supported-agents table includes Grok Build and leaves rate limit unsupported

## Risk Assessment

`AgentSession` literals in tests must still compile. No new required fields.
