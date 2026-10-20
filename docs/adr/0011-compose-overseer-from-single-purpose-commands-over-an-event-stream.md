# 0011. Compose overseer from single-purpose commands over an event stream

- Status: Accepted
- Date: 2026-10-20T06:30:00Z

## Context

Overseer is one binary that runs a fixed pipeline: sync, claim, worktree, route, build, escalate, review, rework, split, retro and land. The order is hard-coded in `runner.rs` `execute()`, `review.rs` `run_review_loop_with_backends`, `worker.rs` and again in the CLI's `run_one`, so adding, removing or swapping a stage means editing Rust. Six modules write task status through an unguarded `UPDATE`, so the state machine exists only as caller convention. There is no event log: history is mutable rows plus markdown transcripts, a backend returns only its final output, and command backends report fixed token counts from config. Overseer also cannot see agents it did not launch, while the user runs Claude Code, opencode and agy on several machines and needs one view of what each agent is doing, what it costs and when it needs them.

## Decision

Overseer is rebuilt as one `ovs` binary whose subcommands each do one job and communicate through newline-delimited JSON events with a single versioned schema. The work is split into three sub-projects, built in order:

1. An agent monitor: per-source collectors (`ovs collect claude|opencode|agy`), `ovs ship` to a central `ovs serve` on ali-desktop, `ovs tail`, `ovs alert` and `ovs notify`. It reads what each agent tool already writes and is read-only in v1. Design: `docs/superpowers/specs/2026-10-20-agent-monitor-design.md`.
2. Overseer's own backends emit the same events, with real usage and tool calls.
3. The factory pipeline is split into subcommands around a task queue that is the only writer of task state.

Storage sits behind traits with SQLite and PostgreSQL implementations, starting with `EventStore` for the monitor.

## Consequences

- Each stage can be run, tested and replaced alone, for example `cat events.ndjson | ovs alert`.
- Agents launched by overseer appear in the monitor as one more source, with no special path.
- The event schema becomes the contract between components; changing it needs a version bump and updates to every collector.
- The existing crates stay in place until sub-project 3 replaces them, so the workspace carries both shapes for a while.
- Supporting two databases means a shared conformance suite and an ephemeral Postgres in the dev shell.
