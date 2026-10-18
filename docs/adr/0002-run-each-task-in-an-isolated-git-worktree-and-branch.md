# 0002. Run each task in an isolated git worktree and branch

- Status: Accepted
- Date: 2026-10-18T00:00:00Z

## Context

This decision is recorded retrospectively on 2026-10-18T00:00:00Z to document task execution isolation. Coding agents execute tasks by reading and modifying files across the repository. Running agent tasks directly in the primary working tree risks dirtying uncommitted files and makes concurrent or isolated runs impossible. Tasks also need distinct branches so their changes can be reviewed, diffed, merged, or discarded cleanly. Discarding a failed or rejected task must clean up the workspace and return the task to the backlog.

## Decision

Each task executes in its own dedicated git worktree located at `worktrees/task-<id>` on a branch named `overseer/task-<id>`. The orchestrator manages worktree creation from HEAD and cleans up worktrees upon merge or discard. When `overseer discard` is invoked on a review or blocked task, the orchestrator removes the worktree, deletes the task branch, and resets the task status to backlog.

## Consequences

- Tasks run in isolated environments without interfering with each other or the primary working tree.
- Inspection and diffing are straightforward using standard git commands on the task branch.
- Discarding a task removes temporary work cleanly and safely restores it to the backlog.
