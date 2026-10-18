# Implementation Plan: Overseer Task Orchestrator

**Branch**: `001-overseer-orchestrator` | **Date**: 2026-10-18 | **Spec**: [spec.md](spec.md)

**Input**: Feature specification from `/specs/001-overseer-orchestrator/spec.md`

## Summary

Overseer is a single-user, local-first task orchestrator that coordinates coding tasks across local model endpoints (llama.cpp) and remote CLI harnesses (`agy`, `claude`), maintaining an authoritative SQLite queue. It enforces complete isolation via dedicated git worktrees, implements resilient fallback routing with credits-low throttling, runs multi-agent code reviews with bounded rework loops, guards sensitive paths through hard escalation, drains tasks continuously via a locked worker with crash recovery, proposes retrospective skill improvements without unsupervised prompt drift, and synchronizes specification tasks directly from `specs/*/tasks.md`.

## Technical Context

**Language/Version**: Rust 2024 edition (1.85+)

**Primary Dependencies**:
- `rusqlite` (bundled SQLite 3, synchronous single-writer access)
- `tokio` (asynchronous runtime for process execution, timeouts, and signal handling)
- `reqwest` (HTTP client for OpenAI-compatible completions and health checks)
- `clap` (v4 derive-based CLI parsing)
- `notify` (cross-platform filesystem watcher for task specification directories)
- `globset` (compiled glob matching for hard escalation paths)
- `serde`, `serde_json`, `toml` (configuration, migration, and tool serialization)
- `tracing`, `tracing-subscriber` (structured diagnostics and stderr logging)
- `thiserror`, `anyhow` (typed library errors and CLI contextual error propagation)

**Storage**:
- SQLite database (`overseer.db` in state directory) managing tasks, runs, reviews, lessons, and settings.
- Plain files on disk: run transcripts (`<state-dir>/runs/<run-id>.md`), round review reports (`<state-dir>/reviews/task-<id>/round-<n>/<profile>.md`), proposed lesson files and diffs (`<state-dir>/lessons/<id>.md`, `<id>.diff`), and versioned skills (`<state-dir>/skills/<name>/v<N>.md`, `<state-dir>/skills/<name>/current`).

**Testing**:
- Unit and integration tests in `crates/*/tests/` and inline test modules.
- Mock HTTP endpoints via `wiremock` for OpenAI backend and tool loop tests.
- Temporary directories (`tempfile`) and isolated mock git repositories for worktree, landing, and sync tests.

**Target Platform**: Linux (primary), macOS.

**Project Type**: Multi-crate Rust workspace CLI and library system.

**Performance Goals**:
- Worker idle loop overhead < 10ms per poll cycle.
- Atomic task claiming and locking in SQLite in < 5ms.
- Asynchronous non-blocking file watcher debouncing with 2-second quiet period.

**Constraints**:
- Strict single-user operation; no multi-tenant or server daemon overhead.
- Single-writer SQLite architecture executed behind `spawn_blocking` or dedicated store calls.
- Process execution confined to task worktrees (`worktrees/task-<id>`); tool loop strictly rejects path escapes and `.git` access.
- The repository working tree is never checked out or modified during active task runs.

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

- [x] **I. Single-User Simplicity & Local-First**: Overseer avoids multi-tenant or corporate abstractions. Local llama.cpp is prioritized.
- [x] **II. Single Source of Truth**: All tasks, runs, reviews, and lessons are stored in SQLite. No distributed JSON state files.
- [x] **III. Worktree & Branch Isolation**: Tasks run exclusively in isolated git worktrees on dedicated branches (`overseer/task-<id>`).
- [x] **IV. Safety Gates & Bounded Autonomy**: Rework loops are strictly bounded by `max_reworks`; path escalation halts to `needs-human`.
- [x] **V. Human-in-the-Loop Learning**: Lessons propose skill diffs for human review; no autonomous prompt mutation.
- [x] **Architectural & Quality Standards**: Backends are stateless execution protocols; secrets pass via env vars only; TDD enforced with passing tests.

## Project Structure

### Documentation (this feature)

```text
specs/001-overseer-orchestrator/
├── spec.md              # Feature specification (scenarios, requirements, success criteria)
├── plan.md              # This architecture and implementation plan
└── tasks.md             # Sequenced task checklist (T001..T###)
```

### Source Code (repository root)

```text
crates/
├── overseer-core/            # Foundational types, traits, and configuration
│   └── src/
│       ├── config.rs         # overseer.toml schema, profiles, fallback, escalation, review, landing
│       ├── lib.rs            # Top-level exports and error types
│       ├── source.rs         # TaskSource, SourcedTask, SourceError trait definitions
│       └── types.rs          # Task, Run, TaskStatus, Permissions, Verdict, FailureKind
├── overseer-store/           # SQLite persistence layer and migrations
│   ├── migrations/           # 0001_init.sql .. 0007_sources.sql
│   └── src/
│       ├── lib.rs            # Store initialization and error types
│       └── store.rs          # Store CRUD, atomic state transitions, source queries
├── overseer-backends/        # Model invocation protocols
│   └── src/
│       ├── command.rs        # Subprocess runner for CLI harnesses (agy, claude, shell)
│       ├── health.rs         # Doctor shallow & deep health probes
│       ├── lib.rs            # Backend trait, RunRequest, RunOutcome
│       └── openai.rs         # OpenAI HTTP backend and agent tool loop (read/write/run)
├── overseer-orchestrator/    # Core execution and coordination engine
│   └── src/
│       ├── doctor.rs         # Plan analysis and health reporting
│       ├── escalation.rs     # Hard path escalation checker via globset
│       ├── landing.rs        # Git rebase, test command, gh PR, fast-forward merge
│       ├── learning.rs       # Retrospective runner and lesson generator
│       ├── lock.rs           # Single-worker PID file lock (worker.lock)
│       ├── review.rs         # Parallel reviewers, VERDICT parsing, rework loop
│       ├── router.rs         # Fallback chains, credits-low mode, failure classification
│       ├── runner.rs         # Worktree setup, skill injection, attempt execution
│       ├── skills.rs         # Disk-based versioned skills management
│       ├── sources/          # SpecKitSource implementation
│       ├── sync.rs           # External source synchronizer and task state reconciler
│       ├── watch.rs          # Asynchronous filesystem watcher with debouncing
│       ├── worker.rs         # Continuous queue drainer, crash recovery, signal handling
│       └── worktree.rs       # Git worktree lifecycle and branch diffing
└── overseer-cli/             # Command-line interface
    ├── src/
    │   ├── cli.rs            # Clap derive subcommands and flag parsing
    │   ├── commands.rs       # Command handlers for all overseer operations
    │   ├── main.rs           # Entrypoint, tracing init, exit code mapping
    │   └── state.rs          # State directory resolution and configuration loading
    └── tests/                # CLI integration test suite
```

## Architecture & Crates

1. **`overseer-core`**:
   - Zero-dependency core contract defining domain models and configuration schemas.
   - Houses the `Backend` trait (`run(req: &RunRequest) -> Result<RunOutcome, BackendError>`) and `TaskSource` trait (`scan() -> Result<Vec<SourcedTask>, SourceError>`).
   - Parses `overseer.toml`, enforcing validations such as cycle-free fallback chains, known profile references, and valid escalation globs.

2. **`overseer-store`**:
   - Owns SQLite database access using `rusqlite` with transactional execution.
   - Runs embedded migrations on initialization.
   - Enforces atomic state transitions: `pick_next_ready_task()` (priority + dependency order), `claim_task()`, `set_status()`, `record_run()`, `record_review()`, and `progress_by_source()`.

3. **`overseer-backends`**:
   - Implements model execution protocols without business logic.
   - `OpenAiBackend`: sends `/v1/chat/completions` requests. When `agent_tools = true`, provides worktree-confined tools (`list_dir`, `read_file`, `write_file`, `replace_in_file`, `run_command`) in a multi-turn agent loop.
   - `CommandBackend`: spawns child processes for CLI harnesses (`agy`, `claude`), templating `{workdir}` in arguments, passing clean environments, and classifying failures by parsing stderr output heuristics.
   - `Health`: checks profile availability via shallow model list pings or `--deep` test completions.

4. **`overseer-orchestrator`**:
   - The central coordination brain uniting store, backends, and git.
   - `runner`: creates temporary worktree `<state-dir>/worktrees/task-<id>`, branches `overseer/task-<id>` from HEAD, injects role prompts and versioned skills, and iterates through the router's attempt plan.
   - `router`: computes candidate profiles, prunes metered profiles when `credits_low` is active, and classifies errors into retryable (`quota-exhausted`, `auth-required`, `unreachable`) versus unretryable (`other`).
   - `review`: coordinates parallel reviewer profiles at permission ceiling `read`, parses verdicts from the first non-empty line, executes rework cycles up to `max_reworks` on the existing worktree, or marks approved / needs-human.
   - `escalation`: runs git diff against the merge base after build or rework; matches changed paths against `[escalation].paths` via `globset`, halting to `needs-human` before running reviewers.
   - `learning`: triggers retrospectives on reworked or failed tasks, prompting the learning model to propose skill revisions as diffs against the active version.
   - `landing`: rebases reviewed branches onto remote base, executes test commands, pushes branches, opens PRs via `gh`, and executes fast-forward merges when configured.
   - `sources` & `sync`: parses SpecKit task markdown files, syncs task records into SQLite, detects added/updated/cancelled/revived tasks, and monitors filesystem changes via `notify`.

5. **`overseer-cli`**:
   - Front-end user interface providing complete operator control over the queue, worker, doctor diagnostics, diffs, landing, skills, and configuration.

## Data Model

### Task Statuses

```text
  [backlog] ──(ready)──> [ready] ──(claim)──> [running]
                                                 │
                                     ┌───────────┴───────────┐
                                (success)                 (failure)
                                     │                       │
                             [escalation check]              v
                              │              │          [blocked]
                          (matches)      (no match)
                              │              │
                              v              v
                        [needs-human]    [review] ──(all PASS)──> [approved] ──(merge/land/reconcile)──> [done]
                              ^              │
                              │         (any CHANGES)
                              │              │
                              │        (round < max) ──> [running] (rework)
                              │              │
                              └────────(round >= max / ESCALATE)
```

- `backlog`: Created or synced, but not yet ready to run.
- `ready`: Eligible for worker claiming (all dependencies are `done`).
- `running`: Actively being executed by an attempt runner.
- `review`: Build succeeded; awaiting reviewer execution or review approval.
- `blocked`: Execution failed (unroutable error or all fallback attempts failed).
- `needs-human`: Halted due to path escalation, reviewer `ESCALATE`, or rework limit exhaustion.
- `done`: Merged into main, landed via PR, or reconciled as already on HEAD.
- `cancelled`: Synced task removed from specification before work began.

### Database Schema (Tables)

- **`tasks`**:
  - `id`: INTEGER PRIMARY KEY AUTOINCREMENT
  - `external_id`: TEXT UNIQUE (e.g. `001-overseer-orchestrator/T001`)
  - `title`: TEXT NOT NULL
  - `body`: TEXT NOT NULL
  - `status`: TEXT NOT NULL (`backlog`, `ready`, `running`, `review`, `blocked`, `needs-human`, `done`, `cancelled`)
  - `priority`: INTEGER NOT NULL DEFAULT 10
  - `profile`: TEXT (requested profile name)
  - `round`: INTEGER NOT NULL DEFAULT 0
  - `approved`: BOOLEAN NOT NULL DEFAULT 0
  - `reason`: TEXT (escalation or refusal reason)
  - `source`: TEXT (e.g. `speckit`)
  - `created_at`: TEXT NOT NULL (ISO8601 UTC)
  - `updated_at`: TEXT NOT NULL (ISO8601 UTC)

- **`runs`**:
  - `id`: INTEGER PRIMARY KEY AUTOINCREMENT
  - `task_id`: INTEGER NOT NULL REFERENCES `tasks(id)`
  - `profile`: TEXT NOT NULL
  - `status`: TEXT NOT NULL (`running`, `completed`, `failed`)
  - `input_tokens`: INTEGER NOT NULL DEFAULT 0
  - `output_tokens`: INTEGER NOT NULL DEFAULT 0
  - `failure_kind`: TEXT (`quota-exhausted`, `auth-required`, `unreachable`, `other`)
  - `failure_text`: TEXT
  - `transcript_path`: TEXT
  - `skills`: TEXT (comma-separated `name:version` active during attempt)
  - `created_at`: TEXT NOT NULL
  - `finished_at`: TEXT

- **`reviews`**:
  - `id`: INTEGER PRIMARY KEY AUTOINCREMENT
  - `task_id`: INTEGER NOT NULL REFERENCES `tasks(id)`
  - `round`: INTEGER NOT NULL
  - `profile`: TEXT NOT NULL
  - `verdict`: TEXT NOT NULL (`pass`, `changes`, `escalate`)
  - `path`: TEXT NOT NULL
  - `created_at`: TEXT NOT NULL

- **`lessons`**:
  - `id`: INTEGER PRIMARY KEY AUTOINCREMENT
  - `task_id`: INTEGER NOT NULL REFERENCES `tasks(id)`
  - `skill`: TEXT NOT NULL
  - `base_version`: INTEGER NOT NULL
  - `status`: TEXT NOT NULL (`proposed`, `applied`, `rejected`)
  - `path`: TEXT NOT NULL
  - `diff_path`: TEXT NOT NULL
  - `created_at`: TEXT NOT NULL

- **`skills`**:
  - `id`: INTEGER PRIMARY KEY AUTOINCREMENT
  - `name`: TEXT NOT NULL
  - `version`: INTEGER NOT NULL
  - `content_hash`: TEXT NOT NULL
  - `created_at`: TEXT NOT NULL

- **`settings`**:
  - `key`: TEXT PRIMARY KEY
  - `value`: TEXT NOT NULL

## Key Flows

### 1. Worker Queue Drainer Flow
1. Acquire exclusive lock on `<state-dir>/worker.lock` writing process PID; exit code 3 if already locked.
2. Run startup crash recovery: find tasks in `running` status, transition to `blocked`, mark latest run failed with `interrupted`.
3. Check for external source updates via `sync_sources()` if triggered by watcher or rescan interval.
4. Select highest priority task in `ready` status whose dependencies are all `done`.
5. Claim task and invoke runner with attempt plan (primary profile + fallback chain).
6. If attempt plan succeeds with uncommitted changes, commit to `overseer/task-<id>`.
7. Evaluate hard escalation; if clean, trigger automated review loop; otherwise halt to `needs-human`.
8. Sleep poll duration (`poll_secs`) if no tasks are ready; handle `SIGINT`/`SIGTERM` gracefully.

### 2. Review and Rework Loop Flow
1. Collect reviewer profiles from `[review].profiles`. If empty, task is approved immediately.
2. For current round `n`, launch all reviewer profiles concurrently with permission ceiling `read`.
3. Pass task body, instructions, and full unified diff (truncated to 60 KB threshold) to reviewers.
4. Parse first non-empty line of each reviewer response for exact verdict (`VERDICT: PASS|CHANGES|ESCALATE`).
5. Retry unparseable responses once; if retry fails, record as `ESCALATE`.
6. Record review rows and write reports to `<state-dir>/reviews/task-<id>/round-<n>/<profile>.md`.
7. Aggregate verdicts at round barrier:
   - Any `ESCALATE` -> transition to `needs-human`.
   - Any `CHANGES`:
     - If `round < max_reworks` -> assemble rework prompt with reviewer comments, increment `round`, resume builder on existing worktree and branch, commit new changes, repeat review.
     - If `round >= max_reworks` -> transition to `needs-human` (rework budget exhausted).
   - All `PASS` -> mark `approved = true` in status `review`.

### 3. Hard Escalation Flow
1. Following any build or rework run with changes, inspect changed paths (`git diff --name-only <base>...<branch>`).
2. Match changed paths against `[escalation].paths` using compiled `globset`.
3. If any path matches, set task status to `needs-human` with reason `escalation: <path>`.
4. Skip reviewer execution; worker leaves task halted without retrying.
5. Human operator inspects changes via `overseer diff <id>` and `overseer task show <id>`.
6. Operator runs `overseer approve <id>`, clearing the escalation halt, setting `approved = true`, and moving task to `review`.

### 4. Retrospective Learning Loop Flow
1. Upon reaching a terminal state (`review`, `blocked`, `needs-human`), check if `[learning].profile` is set.
2. Filter by `min_signal`: skip tasks that succeeded on first attempt with round 0 and no failures.
3. Form retrospective prompt containing task description, final diff (truncated to 40 KB), attempt failures, and active skill content.
4. Execute learning profile with `read` permission.
5. If response is `NO CHANGE`, complete without action.
6. If revised skill content is proposed, record row in `lessons` (status `proposed`), save `<id>.md`, generate unified diff against current version (`<id>.diff`).
7. Operator inspects diff via `overseer lessons show <id>`.
8. Operator runs `overseer lessons apply <id>`: Overseer verifies current skill version matches `base_version`, writes `v<N+1>.md`, updates `current` pointer, and marks lesson `applied`.

### 5. Landing Flow
1. Invoked via `overseer land <id>`.
2. Verify task is in status `review` and marked approved (`approved = true`).
3. Verify task branch has no fixup or squash commits (`fixup!`, `squash!`).
4. Rebase task branch onto latest remote tracking branch (`<remote>/<base>`).
5. If `test_command` is configured, execute via `sh -c` in the repository; abort on non-zero exit.
6. Push task branch to remote repository.
7. Open or update pull request via `gh pr create` or `gh pr edit`.
8. If `auto_merge = true`, execute fast-forward merge into base branch, remove task worktree, delete task branch, and set task status to `done`.

### 6. SpecKit Source Synchronization Flow
1. `SpecKitSource` scans configured path (e.g. `specs/`) for subdirectories containing `tasks.md`.
2. Parse phase headings (`## Phase N: ...`) and task lines (`- [ ] T### ...`, `- [x] T### ...`).
3. Extract `[P]` parallel marker, `[US#]` user story tag, task ID, and title/continuation lines.
4. Assign external ID `<spec-dir>/<task-id>` (e.g. `001-overseer-orchestrator/T001`).
5. `sync_sources()` compares scanned tasks against SQLite records:
   - New external ID: insert task with source `speckit`.
   - Existing unstarted task with modified description: update title and body.
   - Unstarted task removed from markdown: transition status to `cancelled`.
   - Previously cancelled task re-appearing: revive to `backlog`.
   - Ticked line (`- [x]`): mark backlog task `done`.
6. Asynchronous `SourceWatcher` monitors specification paths using `notify`, debouncing filesystem events with a 2-second quiet period and triggering source rescan.
7. `overseer status` queries `progress_by_source("speckit")` to display done/total tasks and completion percentage per specification directory.

## Complexity Tracking

| Decision | Why Needed | Simpler Alternative Rejected Because |
|----------|------------|-------------------------------------|
| Multi-crate workspace (5 crates) | Clear separation of pure types (`core`), persistence (`store`), backend protocols (`backends`), orchestrator workflows (`orchestrator`), and CLI (`cli`). | Monolithic crate leads to circular dependencies, slow test builds, and tight coupling of CLI and backends. |
| Synchronous `rusqlite` behind single writer | Deterministic local persistence without async connection pool overhead. | `sqlx` async connection pools introduce connection contention and unnecessary async complexity for a single-user local tool. |
| Git worktree isolation per task | Ensures model file changes and tests never touch the developer's working directory. | In-place execution risks overwriting uncommitted developer work and breaking active editor state. |
| Strict First-Line `VERDICT:` Protocol | Deterministic parsing of model evaluations without fragile JSON parsing or hallucinated keys. | Free-form markdown parsing or complex schemas frequently fail across smaller local models (e.g., 8B/14B Qwen/Llama). |
