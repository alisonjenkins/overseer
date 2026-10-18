# Overseer Constitution

## Core Principles

### I. Single-User Simplicity & Local-First
Overseer is built for a single developer. No multi-tenant abstractions, org hierarchies, or cloud dependencies. Prioritize offline, local model execution (llama.cpp) before falling back to external CLI harnesses (agy, claude).

### II. Single Source of Truth
SQLite holds all mutable task, run, review, and lesson state behind a single writer. No distributed JSON state files. Transcripts and diffs live as inspectable plain files referenced by path.

### III. Worktree & Branch Isolation
Every task execution and retry runs in an isolated git worktree on a dedicated branch (`overseer/task-<id>`). The repository working tree is never modified mid-run. Retries clean uncommitted worktree changes.

### IV. Safety Gates & Bounded Autonomy
Automated review loops enforce bounded rework cycles (default max 2). Hard escalation rules immediately halt tasks modifying sensitive paths (CI, migrations, auth, dependencies) to `needs-human` before review.

### V. Human-in-the-Loop Learning
Retrospectives analyze completed tasks and propose skill updates as reviewable diffs. Overseer never updates skills or prompts autonomously; explicit human approval via the CLI is mandatory.

## Architectural Constraints

Backends implement stateless execution protocols (`openai`, `command`). Secrets are injected strictly via environment variables, never into prompt bodies or persistent logs. Tasks enforce strict permission ceilings (`read`, `write-workdir`, `write-shell`).

## Development & Quality Standards

Every crate adheres to strict TDD: unit and integration tests must verify features before release. Commits are atomic and granular; reverting a commit must leave the workspace compilable. All operations must be idempotent.

## Governance

This constitution supersedes ad-hoc implementation practices. Any amendment requires documented justification, updated specifications, and verified tests. All specifications in `specs/` must align with these principles.

**Version**: 1.0.0 | **Ratified**: 2026-10-18 | **Last Amended**: 2026-10-18
