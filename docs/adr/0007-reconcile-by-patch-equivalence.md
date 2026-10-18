# 0007. Reconcile by patch equivalence

- Status: Accepted
- Date: 2026-10-18T00:00:00Z

## Context

This decision is recorded retrospectively on 2026-10-18T00:00:00Z to address stale task statuses after external merges. Tasks in review or human intervention states are often merged out of band through rebasing, squashing, or cherry-picking. Standard git ancestry checks fail to detect these merges because commit SHAs change during rebase. As a result, overseer continued tracking already merged tasks as pending review or needs-human.

## Decision

The orchestrator reconciles task states against repository HEAD using patch equivalence via `git cherry HEAD <branch>`. If `git cherry` returns no lines starting with `+`, all changes on the task branch exist on HEAD, and the task is automatically marked `done`. Empty output is also treated as merged, covering fast-forward merges where commits are reachable from HEAD.

## Consequences

- Tasks merged externally via rebase or cherry-pick are cleanly recognized as done.
- Overseer avoids redundant re-runs or rework loops for work already present on the main branch.
- Reconciliation runs automatically during `overseer status` and `overseer worker` startup without manual intervention.
- Tasks marked done this way keep their worktree and branch on disk.
