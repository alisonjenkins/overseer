# 0004. Concurrent reviews with changed file context

- Status: Accepted
- Date: 2026-10-18T00:00:00Z

## Context

This decision is recorded retrospectively on 2026-10-18T00:00:00Z to capture the design of the review aggregation system. Automated code reviews must be reliable and fast, evaluating candidate task branches before merging. Reviewers provided only with diffs frequently hallucinated issues or escalated valid changes, such as correct doc comments that could not be verified without seeing surrounding definitions. Running multiple reviewers sequentially also introduced unacceptable latency. The orchestrator required an aggregation policy that prioritizes safety while allowing automated rework.

## Decision

The orchestrator executes all configured reviewer profiles concurrently using asynchronous tasks. Reviewer prompts include both the unified git diff (truncated to 60 KB if large) and the full content of changed files up to 400 lines or 60 KB total. Verdicts are aggregated strictly: any `ESCALATE` yields `ESCALATE`, else any `CHANGES` yields `CHANGES`, else `PASS`. If changes are requested, the orchestrator triggers rework runs up to `max_reworks` before escalating to human attention.

## Consequences

- Reviewers make fewer false-positive escalations because they have full file context.
- Concurrent execution keeps review round latency bounded by the slowest single reviewer.
- Any safety concern or escalation immediately stops automated rework and requests human review.
