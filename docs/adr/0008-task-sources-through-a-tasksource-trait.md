# 0008. Task sources through a TaskSource trait

- Status: Accepted
- Date: 2026-10-18T00:00:00Z

## Context

This decision is recorded retrospectively on 2026-10-18T00:00:00Z to support flexible external task ingestion. Overseer needs to discover tasks defined in external specifications without hard-coding a single markdown parser into the core engine. The initial source is Speckit, but future sources could include issue trackers or alternate task formats. Synchronization must maintain uniqueness across sources, cancel tasks removed before execution, and honor tasks checked off by hand.

## Decision

Task ingestion is abstracted behind a `TaskSource` trait defining `name()`, `watch_paths()`, and `scan()`. The Speckit source scans specification directories and constructs external IDs formatted as `<spec-dir>/<T###>`, enforced by a unique index on `(source, external_id)` in SQLite. When syncing, unstarted tasks missing from the source are cancelled, while ticked tasks in backlog or ready states are marked done. Tasks already in progress, review, or blocked states are left untouched by sync.

## Consequences

- New task sources can be introduced by implementing the `TaskSource` trait.
- Hand-ticking a task in `tasks.md` prevents overseer from executing finished work.
- In-flight tasks are protected from state overwrites during external file synchronization.
