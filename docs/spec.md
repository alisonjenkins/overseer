# Overseer: Single-User Agentic Orchestrator Specification

## 0. Decisions overriding the sections below

Reviewed 2026-10-18. Where this section and a later one disagree, this section wins.

1. **One source of truth**: SQLite holds all task, run, review and skill state. No `task.json`; run transcripts and review files are plain files referenced by path from the DB. Tasks are mutable rows; only the orchestrator writes them.
2. **Backends are protocols, not products**: `openai` (any OpenAI-compatible HTTP server, including llama.cpp and Ollama), `agy`, `claude`, `command`. There is no `ollama` backend.
3. **Routing is per profile**: each Profile has an ordered `fallback` list of profile names. A run that fails with `QuotaExhausted`, `AuthRequired` or `Unreachable` moves to the next profile in the list; any other failure fails the run. The hard-coded Primary/Secondary/Tertiary chain in section 4 is dropped. `credits_low` mode (CLI flag or `overseer mode credits-low`) skips every profile whose backend is `claude`, and is an input to the router, not a failure response.
4. **Hard escalation inspects the diff, not the task text**: after a build run, changed paths are matched against globs in `escalation.toml` (default: `.github/**`, `**/migrations/**`, `Cargo.toml`, `flake.nix`, `**/auth/**`, deploy scripts). A match sends the task to `needs-human` regardless of verdicts.
5. **Permissions** are `read`, `write-workdir`, `write-shell`, mapped per backend (for `agy`: no flag, `--mode accept-edits`, `--dangerously-skip-permissions`). `write-shell` is only allowed inside a task worktree.
6. **Secrets** come from environment variables only. No encrypted secrets file in the MVP.
7. **Async**: tokio runtime; `rusqlite` calls run in `spawn_blocking` behind a single store actor, which is the single writer.
8. **Profiles** also carry `capabilities` tags (`code`, `review`, `summarise`) and a `permissions` ceiling. Profiles live in `overseer.toml`, not the DB.
9. **MVP milestones are re-ordered** (see section 10): the goal is "queue a task, a local model does it, Gemini takes over when the local one is down" by M3.
10. **Chat-completions backends do not edit files**: a chat-completions backend returns text only and cannot edit files. File-editing tasks run through harness CLIs (agy, claude, opencode) via the `command`/`agy`/`claude` backends, which edit the worktree themselves. A built-in read/write/edit/run tool loop for the `openai` backend is milestone M2b (after M3).

## 1. Problem, Goals, and Non-Goals

### Problem
Individual developers and small teams rely on high-cost LLM APIs (e.g., Claude) for complex, multi-step software tasks. When credits run out, the workflow halts completely. Existing orchestration tools like Paperclip or the Software Factory are often over-engineered, multi-tenant, or strictly bound to specific models (e.g., only Opus/Sonnet). There is a need for a lightweight, local-first, model-agnostic system that allows a user to continue working with local models (llama.cpp), open-source endpoints, or alternative CLIs (agy, Gemini) when premium credits are exhausted, without managing complex organizational hierarchies or multi-tenant infrastructure.

### Goals
- **Continuity**: Ensure the user's workflow never stops due to API credit exhaustion.
- **Model Agnosticism**: Support any model backend via a unified interface: OpenAI-compatible HTTP servers, the `agy` CLI (Gemini), `claude -p` CLI, and generic shell commands.
- **Local-First**: Prioritize local execution (llama.cpp) and offline-capable storage.
- **Single-User Simplicity**: No org charts, hiring flows, or multi-tenant architecture. Designed for one developer.
- **Safety**: Enforce strict gates for budget, permissions, and "hard escalation" tasks (auth, CI, migrations) to prevent catastrophic local errors.
- **Auditability**: Provide a complete, inspectable history of every token usage, decision, and model output.

### Non-Goals
- **Multi-tenancy**: No support for multiple users, teams, or shared resources.
- **Web UI**: No graphical dashboard in the MVP; CLI-first, with a small JSON API for automation.
- **VRAM Management**: The orchestrator does not manage GPU memory or model quantization; it assumes the user has configured their local inference server correctly.
- **Auto-Learning**: The system will not automatically update skills or prompts based on feedback; all learning is manual and human-in-the-loop.
- **Real-time Collaboration**: No features for multiple developers working on the same task simultaneously.

## 2. Concepts

### Project
A `Project` represents a repository or workspace context. It contains the codebase, configuration, and the state machine for tasks.
- **Structure**: A directory containing the source code and an `overseer.json` manifest.
- **Isolation**: Tasks within a project run in isolated worktrees to prevent state leakage.

### Task
A `Task` is a single unit of work. It is immutable once created, with state transitions managed by the orchestrator.
- **Title/Body**: Human-readable description.
- **Status**: One of `backlog`, `ready`, `running`, `review`, `blocked`, `needs-human`, `done`.
- **Dependencies**: List of Task IDs that must be `done` before this task can transition to `ready`.
- **Priority**: Integer (lower is higher priority).
- **Profile**: Reference to a `Profile` ID defining the model and role.
- **Round Counter**: Tracks the number of automated rework cycles (max 2).
- **State File**: A single JSON file (e.g., `task.json`) holding the current state, ensuring single-writer consistency.

### Profile
A `Profile` defines the intelligence and constraints for a task.
- **Name**: Human-readable identifier (e.g., "Local Llama", "Claude Fallback").
- **Backend**: The execution engine (e.g., `ollama`, `agy`, `claude-cli`, `http`).
- **Model**: Specific model name or endpoint path.
- **Role Prompt**: System prompt defining the agent's persona.
- **Skills**: List of Skill IDs injected into the context.
- **Cost Class**: Budget tier (`low`, `medium`, `high`) to influence routing.

### Backend Trait
The core abstraction for model invocation.
- **Interface**:
  - `run(prompt: String, workdir: Path, permissions: Permissions) -> Transcript`
  - `Transcript` contains: `output` (text), `usage` (tokens in/out), `exit_status`.
- **Implementations**:
  - `OpenAICompatible`: HTTP client for local LLM servers.
  - `Agy`: Wrapper for the `agy` CLI (Gemini).
  - `ClaudeCLI`: Wrapper for `claude -p` or similar.
  - `GenericCommand`: Shell execution for non-LLM tools.

### Run
A `Run` is a single atomic execution attempt of a task.
- **State**: `queued`, `running`, `completed`, `failed`.
- **Artifacts**: Logs, transcripts, generated files in the worktree.
- **Lifecycle**: Claimed by orchestrator -> Executed by backend -> Results stored -> Status updated.

### Review
The mechanism for automated quality control before merging or escalating.
- **Verdict**: The first line of the review file must be `VERDICT: PASS`, `VERDICT: CHANGES`, or `VERDICT: ESCALATE`.
- **Process**:
  1. Task transitions to `review`.
  2. Reviewers (defined in Profile) generate reports.
  3. Orchestrator aggregates reports.
  4. If all `PASS` -> Approver checks merge safety.
  5. If `CHANGES` -> Builder resumes (if rounds < max).
  6. If `ESCALATE` -> Task moves to `blocked` or `needs-human`.

### Skill
Versioned Markdown files injected into prompts.
- **Versioning**: `skill_name.md` at `v1`, `v2`, etc.
- **Injection**: Content is appended to the system prompt or context window before execution.
- **Management**: Stored in `projects/<id>/skills/`.

### Lesson
A retrospective artifact generated after a task completes.
- **Format**: Markdown diff or suggestion list.
- **Storage**: Stored in `projects/<id>/lessons/`.
- **Application**: Human must explicitly apply lessons to update Skills; never auto-applied in MVP.

## 3. Execution Flow

### Atomic Claim
1. Orchestrator queries the task queue for tasks in `backlog` or `ready` status.
2. Selects the highest priority task satisfying dependency constraints.
3. Attempts an atomic lock (SQLite row lock or file lock) on the task's state file.
4. If locked, retries with exponential backoff or moves to next task.

### Worktree and Branch Setup
1. Create a temporary worktree at `projects/<id>/worktrees/<task-id>`.
2. Create a new git branch `orchestrator/<task-id>`.
3. Initialize permissions (read/write) and environment variables.
4. Inject Skills and Role Prompt into the task context.

### Gate (Budget/Quota/Health)
Before invoking the backend:
1. **Budget Check**: Verify remaining quota against the Profile's `Cost Class`.
2. **Health Check**: Verify backend connectivity (ping, token check).
3. **Quota Check**: If "credits low" mode is active, exclude high-cost models (e.g., Claude) from routing.
4. If any gate fails, transition task to `blocked` and log reason.

### Build Run
1. Invoke the Backend trait `run()` with the generated prompt.
2. Capture `exit_status`, `usage`, and `transcript`.
3. Write transcript to `run_<timestamp>.md` in the worktree.
4. Update task state to `running` -> `completed` (success) or `failed`.
5. If success and status is `done`, release lock and move to next.

### Parallel Reviewers
1. If task is `done` but requires review, transition to `review`.
2. Spawn multiple reviewer agents (defined in Profile) in parallel.
3. Each reviewer writes to `round-N/review-<role>.md`.
4. **Round Barrier**: Orchestrator waits until all reviewers have written their verdicts.
5. Parse first line of each verdict file.

### Approver and Merge
1. If all reviews are `PASS`:
   - Approver agent verifies `git merge-tree --write-tree` safety.
   - If clean, merge branch to `main`.
   - Transition task to `done`.
2. If not clean or not all `PASS`:
   - Proceed to decision logic.

### Rework Logic
1. If any verdict is `CHANGES`:
   - Check `round counter`.
   - If `round < 2`:
     - Resume Builder agent.
     - Increment `round counter`.
     - Transition to `running`.
   - Else:
     - Write `decision.md` with `VERDICT: ESCALATE`.
     - Transition to `needs-human`.

### Hard Escalation List
Regardless of review verdicts, tasks containing specific keywords or patterns (configured in `hard-escalation-rules.md`) immediately transition to `needs-human`:
- Authentication/Security changes.
- CI/CD pipeline modifications.
- Database migrations.
- New dependency installations (production).
- Production deployment scripts.

## 4. Routing and Fallback

### Ordered Chain
Routing is deterministic based on the Profile's `Cost Class` and available backends.
1. **Primary**: Local `ollama` or `llama.cpp` (Low cost, fast).
2. **Secondary**: `agy` CLI or OpenAI-compatible endpoint (Medium cost).
3. **Tertiary**: `claude` CLI (High cost, fallback only).

### Fallback Detection
The orchestrator monitors the `Transcript` for failure indicators:
- **Quota Exhaustion**: API returns 429 or 403 with "credits" error.
- **Auth Required**: API returns 401.
- **Unreachable**: Connection timeout or DNS failure.

### Fallback Action
1. Detect failure type.
2. If `credits low` mode is ON:
   - Route to next cheaper backend (e.g., Ollama).
   - If no cheaper backend exists, fail task with "No available model".
3. If `credits low` mode is OFF:
   - Route to next backend in chain.
   - If chain exhausted, fail task.

### VRAM Awareness
**Non-Goal**: The orchestrator does not query GPU memory or attempt to swap models. It assumes the user has configured their local server to handle the requested model size.

## 5. Learning Loop

### Retrospective Run
After a task completes (success or failure):
1. Generate a `Lesson` artifact.
2. Analyze the transcript and review logs for patterns (e.g., "Builder failed on file encoding").
3. Propose a Skill diff: `old_skill.md` vs `new_skill.md`.

### Human Acceptance
1. Write proposed lessons to `projects/<id>/lessons/proposed_<task-id>.md`.
2. User reviews via CLI: `overseer lessons review <project>`.
3. User accepts specific lessons via CLI: `overseer lessons apply <project> <lesson-id>`.
4. Accepted lessons update the `Skill` version in the file system.
5. **Constraint**: Skills are never auto-updated. Manual review is mandatory to prevent hallucinated optimizations.

## 6. Storage

### Database
- **Tool**: `rusqlite` (synchronous, simple, zero-config) preferred over `sqlx` for MVP simplicity.
- **Justification**: Single-user, local-first application does not require async DB drivers or complex connection pooling. `rusqlite` offers file-based persistence and ease of debugging.
- **Schema**:
  - `tasks`: id, project_id, title, body, status, profile_id, round, priority.
  - `profiles`: id, name, backend, model, cost_class.
  - `runs`: id, task_id, timestamp, usage, exit_status, transcript_path.
  - `skills`: id, name, version, content_hash.

### File Storage
- **State Dir**: `~/.overseer/state/<project-id>/`.
- **Task State**: `task.json` (single writer).
- **Worktrees**: Git worktrees under `projects/<project-id>/worktrees/`.
- **Artifacts**:
  - `runs/`: Raw transcripts.
  - `reviews/`: Round-based review files.
  - `lessons/`: Proposed and applied lessons.
  - `skills/`: Versioned skill markdown files.

### Consistency
- **Single-Writer Orchestrator**: The CLI/API acts as the sole writer.
- **File Locking**: Use `flock` or SQLite advisory locks to prevent race conditions during state updates.
- **Atomic Writes**: Write to temp file -> `rename` to final path.

## 7. Interfaces

### CLI (Primary)
- **Init**: `overseer init <project-dir>` (creates repo, state dir, default profiles).
- **Task**:
  - `add <project> <title> <body>`
  - `list <project>`
  - `show <project> <id>`
- **Run**: `run <project> <id>` (triggers atomic claim and execution).
- **Worker**: `worker <project>` (runs in background mode for continuous execution).
- **Review**: `review <project> <id>` (manual intervention trigger).
- **Skills**:
  - `list <project>`
  - `edit <project> <skill-name>` (opens editor).
  - `apply <project> <lesson-id>`
- **Status**: `status <project>` (human-readable summary).

### HTTP JSON API (Secondary)
- **Port**: `127.0.0.1:3000` (optional, disabled by default).
- **Endpoints**:
  - `POST /api/v1/tasks`
  - `GET /api/v1/tasks/:id`
  - `POST /api/v1/tasks/:id/run`
  - `GET /api/v1/projects/:id/status`
- **Purpose**: Automation integration (e.g., GitHub Actions, custom scripts).

### Web UI
- **Deferred**: Not included in MVP.

## 8. Rust Workspace

### Structure
```text
overseer/
├── core/          # Core logic, traits, state machine
├── store/         # Database (rusqlite), file I/O
├── backends/      # Implementations: ollama, agy, claude-cli, http
├── orchestrator/  # Main execution loop, routing, gates
├── cli/           # Command-line interface (clap)
├── tests/         # Integration tests with mock backends
└── Cargo.toml
```

### Dependencies
- **Libraries**: `thiserror` (error definitions), `tokio` (async runtime), `tracing` (logging).
- **CLI**: `anyhow` (error handling in CLI), `clap` (argument parsing).
- **DB**: `rusqlite` (sync), `sqlx` (optional for future).
- **Network**: `reqwest` (HTTP backends).
- **Process**: `tokio::process` (CLI wrappers).

### Tooling
- **Build**: `cargo build --release`.
- **Dev**: `nix flake` via `crate2nix` for reproducible toolchains.
- **Testing**: TDD approach with `mockall` or manual mocks for backends.
- **Shell**: `devshell` for consistent environment setup.

## 9. Security

### Secrets Management
- **No Secrets in Prompts**: API keys are injected only to the backend process, never into the LLM prompt context.
- **No Secrets in Logs**: `tracing` filters out sensitive headers and keys.
- **Storage**: API keys stored in OS keychain or `~/.overseer/secrets.enc` (encrypted).

### Permissions
- **Workdir**: Tasks run with `read` and `write` permissions only on the specific worktree.
- **Shell**: `allow_shell` flag required for tasks needing system commands; default is false.
- **Network**: Backend processes restricted to localhost or specific allowed hosts.

### Isolation
- **Worktree**: Each task runs in an isolated git worktree.
- **Branches**: Task branches are deleted after merge or failure.
- **No Root**: Orchestrator never runs as root; tasks inherit user permissions.

## 10. MVP Scope and Milestones

### In Scope
- Single user, single project support.
- Local model execution (ollama) and CLI wrappers (agy, claude).
- SQLite state management.
- CLI interface for full lifecycle.
- Basic review loop (1 reviewer).
- Hard escalation rules.

### Out of Scope
- Multi-project support.
- Web UI.
- Multi-agent collaboration.
- Auto-learning (manual skill updates only).
- Cloud hosting.

### Milestones
Each milestone is a set of small atomic commits and leaves `cargo test` and `nix build` green.

1. **M1: Skeleton and store**: Nix flake + workspace, `core` types, `store` (SQLite schema, task CRUD), `overseer init`, `task add|list|show`. Mock backend exists from here.
2. **M2: OpenAI-compatible backend and `run`**: `openai` backend (non-streaming, long timeout, `reasoning_content` ignored), worktree + branch per task, `overseer run <id>` builds a prompt from task + role prompt, runs it, stores transcript and usage, leaves the branch for inspection. A local llama.cpp model can now do a task.
3. **M3: `agy`/`claude` backends and fallback**: `agy` and `claude -p` backends, failure classification, per-profile `fallback` lists, `credits_low` mode. Gemini takes over when the local server is down.
   - **M2b: tool loop**: built-in read/write/edit/run tool loop for the `openai` backend (after M3; see section 0, decision 10).
4. **M4: Worker**: `overseer worker` loop with dependency-aware claim, one task at a time, graceful shutdown, crash recovery of `running` tasks.
5. **M5: Review loop**: reviewer and approver runs, `VERDICT:` parsing, round barrier, max 2 reworks, orchestrator-only merge after `git merge-tree` check.
6. **M6: Hard escalation**: diff-path globs from `escalation.toml`.
7. **M7: Lessons and skills**: retrospective run, proposed lessons as files, `lessons review|apply`, skill versions.
8. **M8: JSON API** (optional): read-only status plus task create/run.

## 11. Open Questions

1. **Skill Injection Format**: Should skills be injected as a separate file in the worktree or appended to the prompt string? (File is safer for long contexts).
2. **Review Parallelism**: How many reviewers should run in parallel? (MVP: 1, Future: configurable).
3. **Secret Injection**: Should secrets be passed as environment variables to the worktree process or injected into the prompt? (Env vars preferred for security).
4. **Conflict Resolution**: If a user manually modifies the worktree while the orchestrator is running, how to handle? (Locking should prevent this, but user override logic needed).
5. **Model Context Window**: Should the orchestrator truncate prompts or warn about context limits? (Warn only; no auto-truncation to preserve fidelity).
6. **Cost Tracking Granularity**: Should we track cost per task, per run, or per token? (Per run initially, aggregated per task).
7. **Git Merge Strategy**: Should the merge be `--no-ff` or `--ff`? (`--no-ff` preferred for history clarity).
8. **State Recovery**: If the orchestrator crashes mid-run, how to recover the lock? (Database transaction + check for temp files).
9. **Hard Escalation Rules**: Who defines the rules? (Config file `rules.yaml` editable by user).
10. **Review Threshold**: What constitutes a "pass"? (All reviewers must pass, or majority? MVP: All).

---

# Research Notes

## Paperclip Analysis
Paperclip is a multi-tenant, Node.js solution focused on "Company" metaphors (org charts, hiring). While useful for its adapter registry and atomic checkout logic, it is over-engineered for a single-user, offline-capable context.
- **Adopted**: Atomic checkout, per-run pipeline (budget -> secrets -> skills -> adapter), versioned skills, hard escalation lists.
- **Rejected**: Org charts, CEO framing, multi-tenant architecture, complex UI.

## Software Factory Analysis
The "Software Factory" approach (Zazencodes) uses a git-based workflow with parallel reviewers and bounded reworks. It is highly effective for code generation but lacks model agnosticism (locked to Opus/Sonnet) and local-first storage.
- **Adopted**: Verdict prefix convention (`VERDICT:`), parallel reviewers with round barrier, bounded rework (max 2), per-job worktree + branch, single-writer state (`job.json`).
- **Rejected**: Cloud-only dependency, specific model locking, auto-learning mechanisms.

## Synthesis
Overseer combines the robust state management and safety gates of Paperclip with the practical, git-centric workflow of the Software Factory, stripping away all multi-tenant and company-specific abstractions to serve a single developer's need for continuity and flexibility across diverse local and remote models.
