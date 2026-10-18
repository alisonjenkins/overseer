# 0009. Sync task sources only while the worker is idle

- Status: Accepted
- Date: 2026-10-18T00:00:00Z

## Context

This decision is recorded retrospectively on 2026-10-18T00:00:00Z to stabilize task synchronization during worker execution. Ingesting tasks from filesystem watchers during active runs creates race conditions, such as modifying dependencies or cancelling tasks currently in progress. File notifications from inotify can also be dropped or missed during burst writes. The worker requires a synchronization schedule that guarantees stability while remaining responsive to specification changes.

## Decision

Task source synchronization triggers on three events: once at worker startup, after file modifications detected by notify following a 2-second quiet period, and on a periodic 10-minute rescan timer. All sync executions are restricted to the worker's idle state when no ready task is currently running. If the worker is busy processing a task, sync waits until the run finishes and the worker becomes idle.

## Consequences

- Running tasks are never modified or cancelled out from under the orchestrator.
- Inotify event bursts are debounced into a single synchronized update.
- Missed filesystem events are reliably caught by the 10-minute periodic rescan backstop.
