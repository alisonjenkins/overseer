# Research: Paperclip and the Software Factory

Generated: 2026-10-18T07:39:45Z. Read via WebFetch on 2026-10-18. **(uncertain)** marks items not confirmed from source.

Sources:
- P1: https://github.com/paperclipai/paperclip (README via raw master; adapter-plugin.md)
- P2: https://github.com/zazencodes/zazencodes-season-3/tree/main/src/software-factory-claude-code (dir listing, SKILL.md, README.md, agents/approver.md, agents/builder.md)

Not read: Paperclip `server/`, `packages/`, `ui/`, docs.paperclip.ing. Paperclip claims come from its README and one adapter doc.

## 1. Paperclip

### Purpose
Self-hosted, MIT, Node.js server + React UI for running teams of AI agents. README: "If OpenClaw is an _employee_, Paperclip is the _company_." Assigns goals, org charts, tracks work and cost. Not an agent framework, prompt manager, or chatbot (its words).

### Entities and relationships
- **Company**: multi-tenant; own tasks, agents, permissions, activity.
- **Goal → Project → Task (issue)**: tasks link to projects, goals, parent tasks.
- **Agent**: role, reporting line, permissions, budget. Humans and agents share an org chart.
- **Task**: single assignee; blocker dependencies, comments, documents, attachments.
- **Routine**: recurring trigger; each run creates an owned, tracked task.
- **Skill**: shared, loaded at runtime; Skill Studio with saved test inputs and version restore.
- **Connection/secret**: GitHub, Notion, Railway, MCP; secrets injected into authorised runs.

### Agent/model invocation
- "Works with": OpenClaw, Claude Code, Codex, Cursor (+Cloud), Gemini CLI, OpenCode, Pi, Hermes (+Gateway), Grok Build, Kimi Code; also custom processes, HTTP/webhook endpoints, external adapter packages.
- Registry (adapter-plugin.md, phase-1 status doc): `registerServerAdapter`, `unregisterServerAdapter`, `requireServerAdapter` in `server/src/adapters/registry.ts`; `assertKnownAdapterType` gates agent routes; UI has `registerUIAdapter`, `findUIAdapter`, `listUIAdapters`.
- **(uncertain)** adapter method signatures, run invocation, usage/cost/session return shape.

### Assignment, tracking, queue
- Atomic checkout and execution locks prevent two runs claiming one task.
- Heartbeat execution (README): DB-backed wakeup queue with coalescing; per run: budget check, workspace resolution, secret injection, skill load, adapter call. Writes logs, usage records, session state.
- Wakes on assigned work, follow-ups, or optional timer/schedules. A mention alone does not wake (FAQ).
- **(uncertain)** queue ordering and retry policy ("bounded recovery" only).

### Review / feedback / self-improvement
- Review and approval stages gate completion; connection-action approvals gate tool calls.
- "Agent evals", saved test runs, quality metrics, performance reviews listed. "Active learning loops" mentioned, no mechanism.
- Roadmap: evals & feedback done; Memory/Knowledge in progress; Organizational Learning and Self-Organization not started.
- **(uncertain)** whether feedback changes agent behaviour beyond skill versioning and review records.

### Persistence, UI, API
- Embedded PostgreSQL for local runs, or external Postgres. Local or S3-compatible files. Encrypted credentials; durable audit records. Export omits secret values, but plain env values and local paths can remain (README warns).
- UI + API `http://localhost:3100`; `pnpm dev:mobile` on 3101.
- `npx paperclipai@latest onboard --yes`, `--bind lan|tailnet`, `npx paperclipai configure`, `npx paperclipai test-drive` (env `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `OPENROUTER_API_KEY`; `--harness codex|opencode`, `--model`). Manual: `pnpm install && pnpm dev`. Node 24.11+, pnpm 9.15+.

### Heartbeats, scheduling, budget
- Routines: cron, webhook, API triggers; concurrency and catch-up policies.
- Spend by company, agent, project, goal, issue, provider, model. Budgets per company/agent/project with warning thresholds and hard stops that pause work. Enforcement uses recorded spend, so reporting lag can delay a stop (README).

### Company-themed vs useful
- Company-themed: org charts, CEO framing, hiring and hire approvals, board governance, "Ready-Made Teams", "Multi-Organization".
- Useful: adapter registry; per-run pipeline (budget → secrets → skills → adapter → audit); atomic checkout and coalescing wakeup queue; threshold budgets by model; versioned skills with test inputs; routines that create normal tracked tasks.

## 2. Software Factory for Claude Code

### Purpose
`/factory <feature>` runs spec → build on own branch → four parallel reviews → approver → merge to `main` or `needs-human`. Folder mirrors a repo root for copying. Examples ran on zazencodes.com (Vue + Firebase).

### Files
- `.claude/skills/factory/SKILL.md`: orchestrator; sole writer of `job.json` (per README).
- `.claude/agents/`: seven agent definitions.
- `factory/backlog.md` (for `/factory next`), `factory/dashboard.html`, `factory/jobs/` (two examples), `BUILD_PROMPT.md` (historical; describes a replaced `board.json`).

### Entities
- Job: `factory/jobs/<job-id>/job.json`. Fields: `id`, `feature`, `branch`, `worktree`, `stage`, `round`, `reviews`.
- Stages: `spec`, `build`, `review`, `approve`, `merged`, `needs-human`.
- Review values: `pending`, `PASS`, `CHANGES`; `reviews` covers current round only.

### Agents and models
| Agent | Model | Output |
|---|---|---|
| spec-writer | opus | `spec.md` |
| builder | opus | commits on branch; `build.md` (Read, Grep, Glob, Bash, Edit, Write) |
| reviewer-security | opus | `round-N/review-security.md` |
| reviewer-ux / -ui / -code | sonnet | `round-N/review-*.md` |
| approver | opus | `decision.md` only (Read, Grep, Glob, Bash, Write) |

Spawned as Claude Code subagents by the orchestrator; reviewers spawned in one message; builder resumed via `SendMessage`. Context read from files. **(uncertain)** exact subagent call syntax.

### Commands and parallelism
`/factory <feature>`, `/factory next`, `/factory approve <id>`, `/factory rework <id> <note>`. Parallel jobs = separate sessions, each with own worktree, branch, `job.json`, ports (builder `5100+10n`, reviewer-ux `5101+10n`, reviewer-ui `5102+10n`). No scheduler; jobs start manually.

### Review / feedback / skills loop
- **Trigger**: orchestrator sets `stage: "review"`, creates `round-<N>/`, spawns four reviewers together. Worktree at `../<repo>-factory/<job-id>/` on branch `factory/<job-id>`.
- **Verdicts**: first line `VERDICT: PASS` or `VERDICT: CHANGES`; written to `job.json` as each lands. Builder never resumed mid-round. Build gate: `build.md` first line `STATUS: READY` or `BLOCKED` (BLOCKED → `needs-human`).
- **CHANGES, resumes left**: builder resumed via `SendMessage`. `MAX_RESUMES = 2` after round 1, so at most 3 rounds.
- **CHANGES, no resumes left**: orchestrator writes `decision.md` starting `ESCALATE`.
- **All PASS**: approver writes `decision.md`. `APPROVE` → merge; `ESCALATE` → `needs-human`.
- **APPROVE requires**: every criterion met with evidence, one spot-checked; final reviews all PASS; `git merge-tree --write-tree main <branch>` clean.
- **ESCALATE triggers**: payments/auth/permissions/security rules; deploy, CI, hosting, build scripts; schema migration, email, production writes, new dependency; business-decision assumptions. Example 001 escalated for a prerender-script edit.
- **Merge**: clean `main` checkout (ignoring `factory/backlog.md`); `git merge --no-ff`, `git worktree remove`, `git branch -d`; stage `merged`. Busy checkout → needs-human. "Never push."
- **Rework**: needs-human only; note saved as `rework-<k>.md`; stage → build; fresh builder with fresh resume budget.
- **Artifacts**: `spec.md`, `build.md`, `round-<N>/review-*.md`, `decision.md`, `rework-<k>.md`, `job.json`.
- **Self-improvement**: none found; checklists are generic and tailored by hand. **(uncertain)** no hidden automation.
- **Inconsistency**: SKILL.md says orchestrator writes only `job.json`, yet it writes `decision.md` (exhausted rounds) and `rework-<k>.md`. Writer of `decision.md` on BLOCKED is unspecified.

### Persistence, UI, budget
- Markdown + JSON on disk, git branches, worktrees. `factory/jobs/` should be gitignored.
- `dashboard.html`: stage cards, review chips, needs-human highlight; `python3 -m http.server` in `factory/` → port 8000; polls every 2 s; no API.
- No budget or cost tracking described.

### Company-themed vs useful
- Company-themed: none; it is a software workflow.
- Useful: verdict-prefix convention; parallel reviewers with round barrier; bounded rework; hard escalation list; per-job worktree + branch + state file; single-writer state. Builder rules: verify branch first, explicit `git add` paths, never mask failures to reach READY. Approver edits only `decision.md`.

## 3. Ideas worth keeping for a non-company, single-user, model-agnostic orchestrator

- Gate each run (budget, secrets, skills) before the model call; record usage after.
- One minimal adapter interface per harness (run in; transcript, usage, session id out). Paperclip's exact interface is uncertain, so design your own.
- Single-writer `job.json`-style state per job, with explicit stages.
- Worktree + branch per job; only the orchestrator merges to main.
- Parallel reviewers with a round barrier (wait for all verdict files).
- Machine-readable first-line verdicts (`VERDICT:`, `APPROVE|ESCALATE`, `STATUS:`).
- Bounded automated rework (cap at 2), then `needs-human`.
- Hard-coded escalation classes (auth, deploy/CI, migrations, dependencies) regardless of verdicts.
- Approver read-only except its decision file; run `git merge-tree` before approving.
- Budgets as warn-then-stop thresholds per model and per job.
- Skills as versioned markdown with saved test inputs; human edits them unless an eval says otherwise.
- Plain files and git as state, inspectable without a UI; skip org-chart, hiring, board, and multi-tenant layers.
