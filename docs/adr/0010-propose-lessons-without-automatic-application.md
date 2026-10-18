# 0010. Propose lessons without automatic application

- Status: Accepted
- Date: 2026-10-18T00:00:00Z

## Context

This decision is recorded retrospectively on 2026-10-18T00:00:00Z to govern automated agent learning. When tasks require rework or fail, retrospective analysis can extract valuable lessons to improve future agent prompts and skill files. However, automatically mutating skill files without human oversight risks corrupting prompts, introducing regressions, or drifting from project conventions. The original specification explicitly scoped autonomous learning out of the MVP in favor of manual updates.

## Decision

Retrospectives analyze completed runs that exhibited learning signals (such as reworks or failures) and propose improvements as structured lesson files in `lessons/<id>.md` and `lessons/<id>.diff`. Lessons are stored in SQLite with status `Proposed`. Operators review proposed lessons and decide whether to apply or reject them via `overseer lessons apply` or `overseer lessons reject`. Skills are versioned per profile, and each run row records the exact skill versions active during execution.

## Consequences

- Agent skill guidelines improve incrementally without risk of silent prompt corruption.
- Human review is required before any prompt or skill change becomes active.
- Exact skill versions are recorded on every run for debugging and reproducibility.
