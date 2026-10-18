# 0001. Record decisions as ADRs

- Status: Accepted
- Date: 2026-10-18T00:00:00Z

## Context

This decision is recorded retrospectively on 2026-10-18T00:00:00Z to establish the record-keeping convention for the repository. Architecture choices in overseer were originally captured across initial specifications, commit bodies, and code comments. As the orchestrator grew, tracking why specific designs were chosen became difficult without reading multiple documents. Future maintainers need a single source of truth for architectural rationale and trade-offs.

## Decision

Decisions that shape the architecture of overseer are recorded as lightweight Architecture Decision Records in `docs/adr/`. Each record captures the context, decision, and consequences in plain Markdown following the format established in nix-config.

## Consequences

- Architecture rationale is kept close to the codebase in version control.
- Significant changes that reverse or modify an existing decision require a new ADR.
- Specifications can describe initial intent while ADRs document active architectural choices.
