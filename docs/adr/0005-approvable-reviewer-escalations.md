# 0005. Approvable reviewer escalations

- Status: Accepted
- Date: 2026-10-18T00:00:00Z

## Context

This decision is recorded retrospectively on 2026-10-18T00:00:00Z to resolve deadlocks caused by reviewer escalations. When a reviewer returned an escalate verdict, early versions of the orchestrator moved the task to needs-human without capturing the reviewer's reasoning. Human operators had no clear context on why the task had stopped without reading raw review transcripts. Furthermore, the CLI lacked an explicit approval mechanism to clear reviewer escalations and unblock the task. Tasks were left stranded in needs-human with no recovery path.

## Decision

When a reviewer emits `VERDICT: ESCALATE`, the orchestrator extracts the first feedback bullet and stores it as the task reason formatted as `escalation: reviewer: <bullet>`. The `overseer approve` command validates that the task is in `needs-human` with an escalation reason, marks the task approved, and transitions it back to `review` status with an `approved: ...` reason prefix so landing can proceed.

## Consequences

- Human operators immediately see why a reviewer escalated directly in task status listings.
- Operators can explicitly override reviewer objections using `overseer approve`.
- Approved tasks retain an audit trail of the original escalation reason and the subsequent approval.
