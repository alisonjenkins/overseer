# 0004. Concurrent reviews with changed file context

- Status: Accepted
- Date: 2026-10-18T00:00:00Z

## Context

This decision is recorded retrospectively on 2026-10-18T00:00:00Z to capture the design of the review aggregation system. Automated code reviews must be reliable and fast, evaluating candidate task branches before merging. On 2026-10-18, a reviewer given only the diff returned ESCALATE for a correct doc comment on Store::list_tasks (overseer dogfood task 4), because it could not check the comment against the method body. The orchestrator required an aggregation policy that prioritizes safety while allowing automated rework.

## Decision

The orchestrator executes all configured reviewer profiles concurrently using asynchronous tasks. Reviewer prompts include both the unified git diff (truncated to 60 KB if large) and the full content of changed files up to 400 lines or 60 KB total. Verdicts are aggregated strictly: any `ESCALATE` yields `ESCALATE`, else any `CHANGES` yields `CHANGES`, else `PASS`. If changes are requested, the orchestrator triggers rework runs up to `max_reworks` before escalating to human attention.

## Consequences

- Reviewers make fewer false-positive escalations because they have full file context.
- Concurrent execution keeps review round latency bounded by the slowest single reviewer.
- When changes are requested, the orchestrator runs rework rounds up to `max_reworks`. If the budget runs out, the task moves to `needs-human` with an escalation reason, so `overseer approve` can clear it.
