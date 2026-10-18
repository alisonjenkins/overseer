# 0003. Model-agnostic backends with fallback chains

- Status: Accepted
- Date: 2026-10-18T00:00:00Z

## Context

This decision is recorded retrospectively on 2026-10-18T00:00:00Z to document backend abstraction and failure routing. Overseer needs to run against multiple AI providers, including local models via llama.cpp and hosted models via external CLIs or APIs. Hosted services regularly exhaust usage quotas, suffer network outages, or run out of paid credits. Relying on a single provider causes orchestration pipelines to stall whenever that provider becomes unavailable. File editing also requires either harness CLIs or a worktree-confined tool execution loop.

## Decision

Model backends implement a unified `Backend` trait supporting OpenAI-compatible HTTP endpoints and external command-line harnesses, accompanied by health check functions for reachability and authentication. Chat-only backends do not edit files; file-modifying tasks must use harness CLIs or enable the worktree-confined tool loop in the OpenAI backend. Each profile defines an ordered `fallback` chain of profile names. When a run fails with quota exhaustion, authentication failure, or unreachable status, the runner falls back to the next profile, automatically enabling `credits_low` mode on metered quota exhaustion to bypass credit-consuming routes. Other failures, such as a timeout or an empty output, abort the fallback chain and block the task.

## Consequences

- Execution continues smoothly across local models, Gemini, and Claude as quotas shift.
- Metered API credits are protected by automatically falling back to unmetered routes.
- File modifications are strictly contained within the task worktree.
