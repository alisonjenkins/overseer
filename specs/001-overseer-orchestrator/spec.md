# Feature Specification: Overseer Task Orchestrator

**Feature Branch**: `001-overseer-orchestrator`

**Created**: 2026-10-18

**Status**: Implemented / In-Progress

**Input**: User description: "Write the speckit spec set for overseer, so that overseer can later feed its own work from specs/*/tasks.md through its speckit task source."

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Local-First Task Execution & Model Fallback (Priority: P1)

As an individual software developer, I want to queue programming tasks and have them executed by a local model (such as llama.cpp) or harness CLIs (such as `agy` or `claude`), automatically falling back across an ordered chain of alternative profiles when a model is rate-limited, unauthenticated, or unreachable, so that my coding workflow continues uninterrupted without manual babysitting or costly cloud locks.

**Why this priority**: Model invocation in an isolated workspace with resilient fallback is the foundational core of Overseer. Without task execution and routing, no automated development can take place.

**Independent Test**: Can be fully tested by creating a task with `overseer task add --ready`, executing `overseer run <id>` or `overseer worker --once` against a simulated failing primary profile (e.g., unreachable server or 429 quota exhaustion), verifying that Overseer routes to the configured fallback profile, executes in a dedicated git worktree on branch `overseer/task-<id>`, commits the resulting changes, writes execution transcripts, and updates SQLite task and run records.

**Acceptance Scenarios**:

1. **Given** a registered task in `ready` status and an `openai` profile pointing to a local model server, **When** `overseer run <id>` is executed, **Then** an isolated git worktree and branch `overseer/task-<id>` are created from HEAD, the task prompt and role prompt are submitted, changes in the worktree are committed on success, and the task transitions to `review`.
2. **Given** an active profile configured with `fallback = ["secondary"]`, **When** the primary profile fails with a routable failure (`quota-exhausted`, `auth-required`, or `unreachable`), **Then** Overseer cleans the worktree with `git checkout -- .` and `git clean -fd`, runs the secondary profile attempt, and records both attempts in the `runs` table.
3. **Given** `credits_low` mode is enabled or triggered by a metered profile quota failure, **When** a task attempt plan is formed, **Then** all profiles configured with `metered = true` are excluded from the execution plan.
4. **Given** a profile configured with `agent_tools = true`, **When** the model makes tool calls (`list_dir`, `read_file`, `write_file`, `replace_in_file`, `run_command`), **Then** tool invocations are executed strictly inside the worktree, path escapes outside the worktree or into `.git` are rejected with errors returned to the model, and tokens are aggregated across steps until final completion or `max_steps`.

---

### User Story 2 - Automated Review & Bounded Rework Loop (Priority: P2)

As a developer, I want completed task runs to undergo automated code review by one or more reviewer models that inspect full file diffs and return structured verdicts (`PASS`, `CHANGES`, `ESCALATE`), automatically triggering bounded rework cycles when changes are requested, so that routine defects are resolved before I inspect the result.

**Why this priority**: Eliminates manual round-trips for simple syntax or implementation bugs, enforcing automated quality gates while preventing infinite loops through configurable rework budgets.

**Independent Test**: Can be fully tested by configuring `[review]` with reviewer profiles and a `max_reworks` limit. When a build completes or `overseer review <id>` is called, reviewers execute concurrently. Returning `CHANGES` resumes the builder on the existing worktree with combined review comments up to `max_reworks`. If all pass, the task is marked approved (`approved = true`); if reworks are exhausted or any reviewer returns `ESCALATE`, the task halts in `needs-human`.

**Acceptance Scenarios**:

1. **Given** a task in `review` with changes committed on `overseer/task-<id>`, **When** reviewers run concurrently, **Then** each reviewer receives the task body, full diff against merge base, and instructions, writing round artifacts to `<state-dir>/reviews/task-<id>/round-<n>/<profile>.md`.
2. **Given** reviewer output where at least one reviewer returns `VERDICT: CHANGES` and the current round is below `max_reworks`, **When** the round barrier completes, **Then** Overseer constructs a rework prompt from the feedback, increments `round`, resumes the builder on the existing worktree and branch, and commits new changes.
3. **Given** reviewer output where all reviewers return `VERDICT: PASS`, **When** the round barrier completes, **Then** the task remains in `review`, is marked `approved = true`, and is ready for landing or merging.
4. **Given** a reviewer returning `VERDICT: ESCALATE` or rework budget exhausted (`round >= max_reworks`), **When** the round barrier completes, **Then** the task transitions to `needs-human` and records an approvable escalation reason in SQLite.

---

### User Story 3 - Safety Gates, Hard Escalation & Landing (Priority: P3)

As a repository owner, I want changes affecting sensitive paths (CI pipelines, migrations, dependencies, security credentials) to halt immediately for human approval, and I want an automated landing pipeline that verifies test suites, rebases cleanly, opens pull requests, and merges fast-forward without squash commits.

**Why this priority**: Protects critical project infrastructure from unauthorized or uninspected automated changes, ensuring all integrated code satisfies project hygiene and passing test gates.

**Independent Test**: Can be fully tested by generating a diff touching a path in `[escalation].paths` (e.g. `Cargo.toml`). Verify that Overseer transitions the task immediately to `needs-human` with reason `escalation: <path>` before running any reviewers. Verify that `overseer approve <id>` transitions it to `review`. Verify that `overseer land <id>` checks for review PASS, rejects fixup commits, runs `test_command`, pushes to remote, opens a PR via `gh`, and fast-forward merges when `auto_merge = true`.

**Acceptance Scenarios**:

1. **Given** a build or rework run that produces changes matching `[escalation].paths` globs, **When** the run finishes, **Then** Overseer halts the task in `needs-human` with a recorded escalation reason, skips all automated reviewers, and worker does not retry.
2. **Given** a task halted in `needs-human` with an escalation reason, **When** `overseer approve <id>` is run by a human operator, **Then** the task is marked `approved = true` and moved to `review`.
3. **Given** a task in `review` marked approved, **When** `overseer land <id>` is invoked, **Then** Overseer verifies the branch contains no fixup/squash commits, rebases onto the remote base branch, executes `test_command`, pushes the branch, creates or updates a PR via `gh`, and executes a fast-forward merge if `auto_merge = true`.
4. **Given** a task in `review` or `needs-human` whose branch commits are already reachable on HEAD, **When** `overseer status` or `overseer worker` runs, **Then** reconcile automatically detects the branch is on HEAD and transitions the task to `done`.

---

### User Story 4 - Continuous Draining, SpecKit Sync & Retrospective Learning (Priority: P4)

As a software engineer managing a multi-task specification, I want Overseer to run as a continuous worker daemon that ingests task definitions directly from `specs/*/tasks.md`, respects prerequisites, debounces file changes, displays specification progress, and runs retrospectives on failed or reworked tasks to propose skill improvements.

**Why this priority**: Enables complete autonomy in task ingestion and continuous execution, allowing specifications written with SpecKit to be parsed, executed, monitored, and learned from systematically.

**Independent Test**: Can be fully tested by placing a `tasks.md` file in `specs/001-feature/`, configuring `[[sources]] kind = "speckit"`, running `overseer sync`, verifying task rows with external ID `001-feature/T###`, running `overseer worker`, verifying that `worker.lock` enforces single-instance execution and crash recovery recovers dead tasks, and running `overseer retro <id>` to verify that proposed skill diffs are generated without being automatically applied.

**Acceptance Scenarios**:

1. **Given** a markdown file `specs/<spec>/tasks.md` containing formatted task lines (`- [ ] T### [P?] [US#?] Description`), **When** `overseer sync` or the background watcher runs, **Then** tasks are inserted or updated with external ID `<spec>/<T###>`, unstarted tasks removed from `tasks.md` are marked `cancelled`, and ticked `- [x]` lines are marked `done`.
2. **Given** a running worker holding `<state-dir>/worker.lock`, **When** a second worker process is started, **Then** the second worker immediately exits with code 3 and reports the existing PID.
3. **Given** an interrupted worker leaving a task in `running`, **When** a new worker starts, **Then** crash recovery moves the task to `blocked` and marks the interrupted run failed with `interrupted`.
4. **Given** a completed task that required rework, failed runs, or escalation, **When** retrospective analysis runs (`overseer retro <id>` or post-worker hook), **Then** a proposed skill diff is generated in `<state-dir>/lessons/<id>.diff` without mutating active skills until `overseer lessons apply <id>` is explicitly run.
5. **Given** synced SpecKit tasks, **When** `overseer status` is run, **Then** a SPECS progress summary displays done/total counts and percentage per specification directory.

---

### Edge Cases

- **Worker crash or SIGKILL mid-run**: The SQLite state holds the last recorded state; the next worker start detects orphaned `running` tasks, transitions them to `blocked`, and marks the run failed.
- **Graceful shutdown signals**: A single `SIGINT` or `SIGTERM` signals the worker to finish the current running task before exiting code 0. A second signal immediately aborts with code 130.
- **Malformed reviewer output**: If a reviewer does not return `VERDICT: PASS|CHANGES|ESCALATE` on its first non-empty line, Overseer retries the reviewer once. If it fails again, it records `ESCALATE` with an explanation.
- **Merge conflict on land or merge**: `overseer merge` and `overseer land` evaluate `git merge-tree --write-tree` before altering repository state; if conflicts exist, the operation aborts with exit code 2 and leaves the repository untouched.
- **SpecKit task deletion mid-flight**: If an unstarted task (`backlog` or `ready`) is deleted from `tasks.md`, sync marks it `cancelled`. If an in-progress, reviewed, or done task is deleted from `tasks.md`, sync preserves the task and emits a warning.
- **SpecKit ticked line sync**: If a line is checked (`- [x]`) in `tasks.md`, sync marks a backlog task `done`; if the task is already in `review`, it remains in `review` to prevent bypassing verification.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: System MUST maintain all task, run, review, and lesson state in a single local SQLite database managed by a single writer.
- **FR-002**: System MUST provide CLI commands to initialize projects (`overseer init`), manage tasks (`task add`, `list`, `show`, `ready`, `discard`), and view overall status (`overseer status`).
- **FR-003**: System MUST execute task runs in dedicated git worktrees and branches (`overseer/task-<id>`) branched from the repository HEAD.
- **FR-004**: System MUST support the `openai` backend for OpenAI-compatible HTTP servers (e.g., llama.cpp), supporting both single-request completions and an interactive tool loop (`agent_tools = true`) with strict worktree path confinement.
- **FR-005**: System MUST support the `command` backend for external CLI harnesses (`agy`, `claude -p`, custom scripts) with argv `{workdir}` templating, permission arguments, and clean environment variable injection.
- **FR-006**: System MUST route task attempts through profile-specific fallback chains upon routable failures (`quota-exhausted`, `auth-required`, `unreachable`), resetting uncommitted worktree state before each retry attempt.
- **FR-007**: System MUST support `credits_low` mode (toggleable via CLI or triggered by metered profile exhaustion), automatically skipping all profiles designated as `metered = true`.
- **FR-008**: System MUST provide an `overseer doctor` command with shallow connectivity checks and an optional deep execution test (`--deep`) reporting attempt plan usability.
- **FR-009**: System MUST provide an automated review loop running configured reviewer profiles concurrently, parsing first-line verdicts (`VERDICT: PASS|CHANGES|ESCALATE`).
- **FR-010**: System MUST aggregate review verdicts: all `PASS` marks the task approved; any `CHANGES` initiates a rework cycle up to `max_reworks`; any `ESCALATE` or exhausted rework budget moves the task to `needs-human`.
- **FR-011**: System MUST pass full unified diffs (truncated safely to byte thresholds) and task descriptions to reviewers.
- **FR-012**: System MUST match git diff changes against `[escalation].paths` globs after any build or rework run, halting matching tasks to `needs-human` before running reviewers.
- **FR-013**: System MUST allow an operator to clear escalations via `overseer approve <id>`, moving the task from `needs-human` to `review`.
- **FR-014**: System MUST provide `overseer diff <id> [--stat]` to inspect changes on task branches without invoking pagers.
- **FR-015**: System MUST provide `overseer merge <id>` to verify tree cleanliness and merge safety (`git merge-tree`), execute `--no-ff` merge, clean up worktree and branch, and mark the task `done`.
- **FR-016**: System MUST provide `overseer land <id>` to gate on review approval, verify absence of fixup commits, rebase onto remote base, execute `test_command`, push to remote, open/update a PR via `gh`, and fast-forward merge if `auto_merge = true`.
- **FR-017**: System MUST provide a continuous queue drainer `overseer worker` that selects eligible tasks by priority and dependency satisfaction, enforcing single-instance execution via `worker.lock`.
- **FR-018**: System MUST perform startup crash recovery in the worker, moving abandoned `running` tasks to `blocked` and recording interrupted run failures.
- **FR-019**: System MUST reconcile tasks in `review` or `needs-human` whose branch commits are already present on HEAD, automatically marking them `done`.
- **FR-020**: System MUST inject versioned skill files (`<state-dir>/skills/<name>/v<N>.md`) into profile system prompts and record active skill versions on run rows.
- **FR-021**: System MUST support retrospective learning on reworked, blocked, or escalated tasks (`overseer retro <id>`), generating reviewable markdown lessons and unified diffs.
- **FR-022**: System MUST allow operators to list, inspect, apply, or reject lessons (`overseer lessons list|show|apply|reject`), verifying base skill version alignment before updating skills.
- **FR-023**: System MUST provide a SpecKit task source (`[[sources]] kind = "speckit"`) that scans `specs/*/tasks.md`, parsing sequential `T###` tasks, phase headings, user stories, and completion status.
- **FR-024**: System MUST synchronize SpecKit tasks into SQLite (`overseer sync`), assigning external ID `<spec>/<T###>`, updating changed descriptions on unstarted tasks, cancelling removed unstarted tasks, and reviving re-added lines.
- **FR-025**: System MUST monitor SpecKit specification paths using an asynchronous file watcher with quiet-period debouncing, triggering source synchronization during worker idle periods.
- **FR-026**: System MUST display per-specification completion progress (done/total tasks and percentage) in `overseer status`.
- **FR-027**: System MUST provide starter configuration inspection via `overseer config starter [--repo PATH]` and config path resolution via `overseer config path`.
- **FR-028**: System MUST allow discarding task worktrees and branches via `overseer discard <id>`, resetting status to `backlog`.

### Key Entities

- **Task**: The primary unit of work in SQLite. Contains `id`, `external_id` (e.g. `001-feature/T001`), `title`, `body`, `status` (`backlog`, `ready`, `running`, `review`, `blocked`, `needs-human`, `done`, `cancelled`), `priority`, `profile`, `round`, `approved`, `reason`, `source`, `created_at`, `updated_at`.
- **Run**: A single execution attempt of a task. Contains `id`, `task_id`, `profile`, `status` (`running`, `completed`, `failed`), `input_tokens`, `output_tokens`, `failure_kind`, `failure_text`, `transcript_path`, `skills`, `created_at`, `finished_at`.
- **Profile**: Configuration in `overseer.toml` defining how to invoke a model. Attributes: `name`, `backend` (`openai`, `command`), `model`, `base_url`, `api_key_env`, `command`, `env`, `prompt_flag`, `prompt_stdin`, `role_prompt`, `skills`, `fallback`, `permissions` (`read`, `write-workdir`, `write-shell`), `permission_args`, `metered`, `timeout_secs`, `agent_tools`, `max_steps`.
- **Review**: A recorded review evaluation. Contains `id`, `task_id`, `round`, `profile`, `verdict` (`pass`, `changes`, `escalate`), `path`, `created_at`.
- **Lesson**: A proposed retrospective improvement to a skill. Contains `id`, `task_id`, `skill`, `base_version`, `status` (`proposed`, `applied`, `rejected`), `path`, `diff_path`, `created_at`.
- **Skill**: Versioned markdown file stored at `<state-dir>/skills/<name>/v<N>.md` with active pointer file `<state-dir>/skills/<name>/current`.
- **TaskSource**: Trait for external task providers (such as `SpecKitSource`) implementing `name()`, `watch_paths()`, and `scan() -> Result<Vec<SourcedTask>, SourceError>`.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: 100% of task worktree modifications, retries, and reviewer assessments occur within isolated git worktrees without mutating the developer's main working tree or checking out branches on HEAD.
- **SC-002**: Automatic fallback transitions between distinct backend profiles (e.g., local server to CLI harness) complete cleanly with accurate run row tracking and zero orphan worktree state.
- **SC-003**: 100% of file modifications matching configured escalation globs halt automatically to `needs-human`, preventing unreviewed integration of critical files.
- **SC-004**: Automated review and rework loop resolves reworkable defects within configured `max_reworks` cycles, terminating in approved `review` or `needs-human` with zero hung processes.
- **SC-005**: SpecKit task parser accurately synchronizes markdown specifications (`specs/*/tasks.md`) with the SQLite task store, maintaining consistent external IDs and tracking completion metrics.

## Assumptions

- Overseer is deployed on a Linux or macOS workstation with `git` and modern shell environments available in `PATH`.
- The user operates as a single developer with exclusive access to their local overseer state directory.
- Model backends (local OpenAI-compatible server like llama.cpp, or CLI tools like `agy` and `claude`) are installed and authenticated by the user.
- Multi-tenancy, web GUIs, automated unsupervised prompt mutation, and direct GPU VRAM/quantization management are strictly out of scope.
