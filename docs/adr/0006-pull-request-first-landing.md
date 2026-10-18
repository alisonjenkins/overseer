# 0006. Pull-request first landing

- Status: Accepted
- Date: 2026-10-18T00:00:00Z

## Context

This decision is recorded retrospectively on 2026-10-18T00:00:00Z to enforce commit integrity and repository merge standards. Repository policy requires clean, atomic git history with cryptographic commit signatures preserved. Using server-side GitHub merge buttons or the `gh pr merge` command creates unsigned merge commits or squashes changes into monolithic commits. Landing must also guarantee that temporary commits are cleaned up and that test suites pass after rebasing onto the latest target branch.

## Decision

The `overseer land` command enforces landing gates: the task must be in review status (`PASS` verdict or approved), contain no `fixup!`, `squash!`, or `amend!` commits, rebase cleanly onto the remote base branch, and pass the repository's configured `test_command`. It pushes the task branch to the remote and opens or reuses a GitHub pull request. When `auto_merge` is enabled (defaulting to `false`), it merges by fast-forward push of HEAD to the remote base branch, never invoking `gh pr merge`.

## Consequences

- Commit signatures and granular atomic commit histories are preserved intact.
- Branches with uncombined fixups or failing tests are blocked from merging.
- Defaulting `auto_merge` to false ensures human control over merges unless explicitly automated.
- With `auto_merge` set, the task is marked done and the worktree and branch are kept on disk. Removing them is not yet implemented.
