# Overseer

Overseer is a single-user, local-first task orchestrator. It keeps a queue of
software tasks in SQLite and is meant to hand them to whichever model backend
is available: a local OpenAI-compatible server, the `agy` CLI, `claude -p`, or
a shell command. The design is in [docs/spec.md](docs/spec.md); section 10
lists the milestones. This repository implements M6 and M7: the task store,
the `openai` and `command` backends, per-task worktrees, fallback routing with
credits-low mode, the worker queue drainer with single-worker locking and crash
recovery, an automated review and bounded rework loop, hard escalation on sensitive
paths, skills injected into prompts, a learning loop proposing improvements from
finished tasks as reviewable lessons, and the `overseer` CLI for all operations.

## Installation

Run directly via the flake:

```sh
nix run github:alisonjenkins/overseer -- --help
```

Build the binary into `./result`:

```sh
nix build github:alisonjenkins/overseer
# or from a local checkout:
nix build
./result/bin/overseer --help
```

### Flake overlay and Home Manager

Add the input and apply `overlays.default`:

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    overseer.url = "github:alisonjenkins/overseer";
  };

  outputs = { self, nixpkgs, overseer, ... }: {
    # In a NixOS or Home Manager configuration:
    nixpkgs.overlays = [ overseer.overlays.default ];
    home.packages = [ pkgs.overseer ];
  };
}
```

## Agent monitor (`ovs`)

`ovs` is the start of overseer's redesign into small commands joined by one
NDJSON event stream (ADR 0011, design in
`docs/superpowers/specs/2026-10-20-agent-monitor-design.md`). Today it has one
subcommand:

```sh
ovs collect claude --once | jq -c '{session, kind}'
ovs collect claude --since 2026-10-20T00:00:00Z     # follow, skipping old transcripts
```

It reads `~/.claude/projects/**.jsonl` and `~/.claude/sessions/*.json`, prints
one event per line on stdout, and keeps its position in
`$XDG_STATE_HOME/ovs/collect-claude.json`, so a restart continues where it
stopped. Set `OVS_LOG=debug` to see per-poll counts on stderr.

## Build and test

The toolchain comes from the Nix dev shell. Run cargo through it:

```sh
nix develop -c cargo test --workspace
nix develop -c cargo clippy --workspace --all-targets -- -D warnings
nix develop -c cargo fmt --check
```

## Usage

```sh
overseer init
overseer doctor
overseer task add "write parser" --body "handle quotes" --priority 2 --ready
overseer task add "write tests" --priority 1 --depends-on 1
overseer task ready 2
overseer status
overseer worker --once
```

The state directory is chosen in this order: `--state-dir <DIR>`, the
`OVERSEER_STATE_DIR` environment variable, `$XDG_STATE_HOME/overseer`, then
`~/.local/state/overseer`. Set `OVERSEER_LOG=info` (or another `tracing`
filter) to see log output on stderr.

## Running tasks

`overseer init [--repo PATH]` creates the database and, if absent, a starter
`overseer.toml` in the state directory. The repository defaults to the current
directory. An existing `overseer.toml` is never overwritten.

```sh
overseer init --repo ~/src/project
overseer task add "write parser" --body "handle quotes" --profile local
overseer run 1
overseer runs 1
```

`overseer run <id>` claims the task (it must be `backlog` or `ready`), creates
the worktree `<state-dir>/worktrees/task-<id>` on the new branch
`overseer/task-<id>` from the repository's current HEAD, calls the profile's
backend, and writes the transcript to `<state-dir>/runs/<run-id>.md`. If the
run succeeds and the worktree has changes, they are committed to the branch and
the task moves to `review`. If it fails, the task moves to `blocked`, the
changes stay uncommitted in the worktree, and the failure kind is recorded on
the run. The branch and worktree are left in place for inspection.
`overseer runs <task-id>` lists every attempt with its profile, status, token
counts and failure.

## Reviewing and landing tasks

Once a task reaches `review`, you can inspect what it produced, run automated
reviews, merge it into your repository branch, or discard it back to the backlog.

```sh
overseer diff 1
overseer diff 1 --stat
overseer review 1
overseer task show 1
overseer approve 1
overseer merge 1
overseer land 1
overseer discard 1
```

- `overseer diff <id> [--stat]` prints the unified diff (or `--stat` summary)
  between the base branch tip and the task branch `overseer/task-<id>`,
  without invoking a pager.
- `overseer review <id>` manually runs the review and rework loop on a task in
  `review`.
- `overseer task show <id>` displays task details including priority, profile,
  round, whether the task is approved, reason (when set), and a table of all recorded reviews.
- `overseer approve <id>` approves a task halted at `needs-human` due to an
  escalation, moving it to `review` so it can be merged.
- `overseer merge <id>` checks that the repository working tree is clean and
  that HEAD is on the branch the task branched from. It runs
  `git merge-tree --write-tree HEAD <branch>` first and returns a conflict error
  without modifying the repository if conflicts exist. Otherwise it merges with
  `git merge --no-ff -m "overseer: merge task <id>: <title>" <branch>`, removes
  the task worktree, deletes the branch with `git branch -d`, sets the task
  status to `done`, and prints the merge commit sha. Tasks may only be merged
  from status `review`. If the task has recorded reviews but has not been
  approved, a warning is printed (`warning: task <id> has reviews and is not approved`)
  before merging.
- `overseer land <id>` rebases a reviewed task branch onto the remote base
  branch, runs the test command, pushes to the remote, opens or reuses a PR via
  `gh`, and optionally fast-forward merges if `auto_merge` is true.
- `overseer discard <id>` removes the task worktree and branch (`git branch -D`)
  and resets the task status to `backlog`. It refuses unless the task status is
  `review` or `blocked`.

## Automated review and rework loop

When `[review].profiles` is configured with one or more profile names, Overseer
runs an automated review and rework loop after each successful build run (in both
`overseer worker` and `overseer run`), or on demand via `overseer review <id>`.

```toml
[review]
profiles = ["local"]             # reviewers to run concurrently (empty disables review)
max_reworks = 2                  # maximum rework cycles before escalating (default 2)
# rework_profile = "local-edit"  # optional profile for rework runs (defaults to task profile)
```

### Reviewer execution and prompt

For the task's current round, all configured reviewer profiles run
concurrently using their normal attempt plan (including fallback chains and
credits-low skipping). Reviewers run with a permission ceiling of `read`.

Each reviewer receives a prompt containing:
- The task title and body.
- The unified diff of the task branch against the base branch (truncated to 60 KB
  with an explanatory notice if larger).
- Instructions to return a verdict on the first non-empty line followed by a
  bulleted list of concrete problems or reasons.
- Explicit instructions not to use tools, run shell commands, or edit files.

The first non-empty line of the reviewer output must be an exact verdict:
- `VERDICT: PASS`: changes are accepted as-is.
- `VERDICT: CHANGES`: modifications are requested before approval.
- `VERDICT: ESCALATE`: architectural concerns, policy issues, or problems
  requiring human judgment.

Any other initial line is unparseable. If a reviewer fails to run or returns an
unparseable verdict, Overseer retries it once. If the retry also fails or is
unparseable, the result is recorded as `ESCALATE` with an explanatory note.

Each reviewer's response is written to:
`<state-dir>/reviews/task-<id>/round-<n>/<profile>.md`
and recorded in the SQLite `reviews` table (`id`, `task_id`, `round`, `profile`,
`verdict`, `path`, `created_at`).

### Aggregation and rework

When all reviewers in a round finish (the round barrier), Overseer logs a summary
line:

```text
task 1 round 0: CHANGES (reviewers: local=CHANGES)
```

Verdicts are aggregated as follows:
1. **Any `ESCALATE`**: The task status transitions to `needs-human`.
2. **Any `CHANGES`**:
   - If `round < max_reworks`: Overseer builds a follow-up rework prompt
     concatenating the reviewer feedback, increments `round`, and runs the task
     again on the **existing** worktree and branch (without starting over) using
     `rework_profile` (or the task's original profile if unspecified). Changes
     are committed to the branch, and the review loop continues with the next
     round.
   - If `round >= max_reworks`: Rework budget is exhausted; the task status
     transitions to `needs-human`.
3. **All `PASS`**: The task remains in `review` and is marked `approved` (`approved = true`),
   ready for `overseer merge`.

## Hard escalation

Regardless of model confidence or reviewer verdicts, a task whose branch touches
sensitive paths must stop for human review. Overseer inspects the git diff of the
task branch against its base (`git diff --name-only <base>...<branch>`) after every
successful build or rework run. If any changed file matches the escalation rules, the
task immediately transitions to `needs-human` with an escalation reason **before** any
review runs, and the worker treats it as finished without retrying.

```toml
[escalation]
enabled = true          # whether sensitive path checks are active (default: true)
paths = [               # file globs triggering escalation (defaults shown below)
  ".github/**",
  ".gitlab-ci.yml",
  "**/migrations/**",
  "**/auth/**",
  "**/*.tf",
  "Dockerfile*",
  "**/Dockerfile*",
  "deploy/**",
  "Cargo.toml",
  "**/Cargo.toml",
  "package.json",
  "**/package.json",
  "flake.nix",
  ".env*",
  "**/secrets/**",
]
```

### Escalation behavior

1. **Trigger**: After any build or rework run that produced changes, the changed paths
   are matched against `[escalation].paths` using `globset`. Globs are validated at
   config load; invalid patterns produce an error immediately.
2. **Halt**: When any changed path matches, the task status is set to `needs-human` and
   the reason is recorded (e.g. `escalation: Cargo.toml`). No reviewer profiles are run.
3. **Worker logging**: The worker logs the escalation and does not retry the task:
   ```text
   task 1 modify dependencies: needs-human (escalation: Cargo.toml)
   ```
4. **Inspection and approval**:
   - `overseer task show <id>` displays the recorded `reason`.
   - `overseer merge <id>` refuses any task in `needs-human`.
   - `overseer approve <id>` validates that the task is in `needs-human` with an
     escalation reason, marks it approved (`approved = true`), and moves it to
     `review` with an approval record so that it can be merged with `overseer merge <id>`.

## Draining the queue with the worker

`overseer worker [--once] [--max-tasks N] [--poll-secs S]` drains eligible tasks
from the queue one at a time, allowing you to queue tasks and walk away:

```sh
overseer task add "refactor parser" --ready
overseer task add "write unit tests" --depends-on 1
overseer worker
```

### Task eligibility and ordering

Only tasks in `ready` status are run by the worker. Tasks in `backlog` are not run
until marked ready:

- `overseer task add <title> --ready`: creates a task directly in `ready` status.
- `overseer task ready <id>`: transitions a task from `backlog` or `blocked` to `ready`.

Among `ready` tasks, the worker picks the lowest `priority` value (most urgent),
then lowest id, whose dependencies are ALL in `done` status. Tasks with unmet
dependencies are skipped without failing.

### Worker options

- `--once`: drain any currently eligible ready tasks, exiting 0 as soon as none
  remain.
- `--max-tasks N`: stop after completing N task runs, then exit 0.
- `--poll-secs S`: seconds to sleep when no eligible tasks are ready (default 5 s).
  The sleep is interruptible by shutdown signals.

The worker prints one line per completed task:

```text
task 1 refactor parser: review (local-edit, 1 attempt(s))
```

### Single worker lock

The worker acquires an exclusive non-blocking lock on `<state-dir>/worker.lock`,
writing its process PID into the file. If a second worker attempts to start while
the lock is held, it exits immediately with exit code 3 and an explanatory message:

```text
overseer: another worker is already running (pid 12345)
```

The lock is released automatically when the worker exits or terminates.

### Crash recovery

When a worker starts and acquires the lock, it performs crash recovery before
processing the queue: any task left in `running` status (from a previous worker
that crashed or died) is moved to `blocked`, and its newest `running` run row is
finished as failed with failure text `interrupted`.

### Graceful shutdown

- On `SIGINT` (Ctrl+C) or `SIGTERM`, the worker stops picking new tasks, prints
  `stopping after current task`, and allows the currently executing task run to
  finish normally. It then exits with code 0.
- A second signal received during shutdown causes an immediate exit with code 130,
  leaving the current task in `running` status. The next worker will recover it on
  startup.

### Queue and worker status

`overseer status` prints an overview of the worker, mode, task status counts, and
the 5 most recent runs:

```sh
overseer status
```

```text
worker: not running
credits-low: off

TASKS
STATUS        COUNT
backlog           1
ready             0
running           0
review            2
blocked           0
needs-human       0
done              3

RECENT RUNS
   ID   TASK  PROFILE       STATUS            TOKENS
    6      1  local-edit    succeeded         450/120
    5      2  gemini        succeeded          890/45
```

## Profiles

Profiles live in `<state-dir>/overseer.toml`. Each task names a profile with
`--profile`, or uses `default_profile`. Run `overseer config path` to print the
path to `overseer.toml`. Run `overseer config starter [--repo PATH]` to print the
starter configuration template to stdout without creating or requiring a state
directory, useful for inspecting starter profiles or diffing against your current
config.

```toml
repo = "/home/me/src/project"
default_profile = "local"

[[profiles]]
name = "local"
backend = "openai"              # any OpenAI-compatible /chat/completions server
base_url = "http://127.0.0.1:8080/v1"
model = "local"
api_key_env = "LOCAL_API_KEY"   # optional; the key is read from this variable
role_prompt = "You are a careful senior engineer."
permissions = "write-workdir"   # ceiling: read, write-workdir or write-shell
timeout_secs = 1500
```

The `openai` backend sends non-streaming requests to any OpenAI-compatible server.
A system message comes from `role_prompt`, followed by the task title and body as
the user message. Only `message.content` is kept; `reasoning_content` is ignored.

When `agent_tools = false` (the default), the backend sends a single request and
returns text only without editing files.

### Agent tool loop (`agent_tools = true`)

When `agent_tools = true`, the `openai` backend runs a multi-turn agent tool loop
over the task worktree. It provides OpenAI-compatible function tools described by
JSON Schema, confined strictly to the worktree root:

- `list_dir(path)`: lists directory entries, omitting `.git`.
- `read_file(path, offset?, limit?)`: reads lines with line numbers, capped to 400
  lines and 20 KB per call, appending a truncation notice when truncated.
- `write_file(path, content)`: creates parent directories and writes/overwrites
  file content (offered when effective permissions are at least `write-workdir`).
- `replace_in_file(path, old, new)`: replaces text that occurs exactly once in the
  file, failing with an explanatory message otherwise (requires at least `write-workdir`).
- `run_command(command)`: executes `sh -c` in the worktree with a 120-second timeout,
  combined stdout and stderr truncated to 8 KB, and cleared environment except
  `PATH` and `HOME=<workdir>` (offered only when effective permissions are `write-shell`).

All paths are resolved relative to the worktree. Escaping via `..`, absolute paths
outside the worktree, symlinks targeting outside the worktree, and any access
touching `.git` are rejected with `error:` messages returned to the model.
The loop executes each tool call, feeds the result back as a `tool` role message,
sums token usage across steps, and finishes when the model produces a final answer
without tool calls. If the loop exceeds `max_steps` (default 100), the run fails.

### Command profiles (harness CLIs)

A `command` profile runs an external program in the task worktree. The prompt is
the last argument, or stdin when `prompt_stdin = true`. A role prompt is put
ahead of the task text, since a plain argv has no system channel. Command
backends report no token usage, so `run` shows zeros for them.

Before spawning, every `command` element and every `permission_args` element
substitutes `{workdir}` (as a whole argument or substring) with the absolute
path of the run worktree. A leading `~/` in `command[0]` expands to `$HOME`.

Environment variables for the spawned process can be set with `env` (a table of
string key-value pairs). They are set on the child process only, without
modifying the parent environment. Keys must be non-empty and contain no `=`.
Values are plain strings, not secrets.

A command profile's `permission_args` lists extra arguments per permission level
(`read`, `write-workdir`, `write-shell`). They go before the prompt, for the
level the run actually gets, which is the lower of what was asked and the
profile's `permissions` ceiling.

`overseer init` writes these profiles. Check each tool's flags against your
installed version before relying on them; the test suite does not run them.

- `local`: the default profile, an `openai` server on `127.0.0.1:8080`. A
  chat-only profile that returns text and cannot edit files.
- `local-edit`: an `openai` profile on `127.0.0.1:8080` with `agent_tools = true`,
  `max_steps = 100`, and `permissions = "write-workdir"`. Requires a llama-server
  started with tool calling (`--jinja`) and a tool-capable chat template.
- `gemini`: `agy --model gemini-3.8-flash-medium`, `prompt_flag = "-p"`, with
  `permissions = "write-shell"` (`--dangerously-skip-permissions`). Falls back
  to `local-agent`.
- `claude`: `claude -p`, `metered = true`, with `--permission-mode` flags.
  Falls back to `gemini`, then `local-agent`.
- `local-agent`: runs the active local llama.cpp profile as an opencode agent
  via `~/.claude/skills/delegation/scripts/delegate-to-local-agent.sh` in the
  worktree with `LOCAL_LLM_AGENT_EDIT = "1"`. Edits files in place. No fallback.

The starter `gemini` profile sets its ceiling to `write-shell` because headless
`agy` (`-p`) requires the "command" permission to inspect the directory. Under
`--mode accept-edits`, headless mode auto-denies that permission, exits 0 with
empty output, and creates no files. Passing `--dangerously-skip-permissions`
allows it to edit files, which is safe because runs take place in an isolated
task worktree.

The starter keeps `default_profile = "local"`. To try `claude` first, set
`default_profile = "claude"`; the chain then covers it when it cannot run.
Because a chat-only `local` profile cannot edit files, editing profiles like
`claude` and `gemini` fall back to `local-agent` instead of `local`.

## Routing and fallback

A task's attempt plan is its profile followed by that profile's `fallback` list,
in order. Fallbacks are followed depth-first and each profile appears once. The
plan is fixed when `run` starts.

Each attempt is its own run row with its own transcript (`overseer runs <id>`
lists them). All attempts share the task's worktree and branch. Before each
retry, uncommitted changes are discarded inside the worktree with
`git checkout -- .` and `git clean -fd`, so every attempt starts from the
branch's last commit. The main repository is never touched. On the first
success the changes are committed and the task moves to `review`.

Only some failures move on to the next profile:

| Failure | Meaning | Next profile? |
|---|---|---|
| `quota-exhausted` | quota, rate limit or credits used up | yes |
| `auth-required` | not signed in, or API key missing or rejected | yes |
| `unreachable` | server down, connection or request timeout, program not found | yes |
| `other` | anything else, including a command timeout or empty output | no, task blocked |

For `openai` profiles the status code decides. For `command` profiles the
output decides, matched case-insensitively. Auth phrases: "authentication
required", "not logged in", "please run /login", "invalid api key",
"unauthorized". Quota phrases: "quota", "rate limit", "usage limit", "credit
balance is too low", "out of credits", "resource exhausted", "429", "limit
reached". This is a heuristic: a non-zero exit whose output quotes one of these
phrases is routed as well.

If every attempt fails, the task is `blocked` and the last failure is recorded on
its run.

### Credits-low mode

```sh
overseer mode show
overseer mode credits-low on    # or off
```

The setting is stored in the database. While it is on, every profile with
`metered = true` is left out of the plan. A metered profile that fails with
`quota-exhausted` during a run turns credits-low on by itself and logs a
warning. The remaining metered profiles in that task's plan are then skipped.

## Checking profile health with doctor

`overseer doctor [--deep]` checks which profiles can run right now, prints
their health and meter status, shows the default profile's attempt plan, and
reports whether any profile in that plan is usable:

```sh
overseer doctor
overseer doctor --deep
```

```text
PROFILE        BACKEND  HEALTH     METERED  DETAIL
local          openai   ok         no       gpt-4o
local-edit     openai   ok         no       gpt-4o
gemini         command  ok         no       agy
claude         command  ok         yes      claude
local-agent    command  ok         no       /home/user/.claude/skills/delegation/scripts/delegate-to-local-agent.sh

default plan: local
credits-low: off
verdict: usable
```

Each profile check runs with a 5-second timeout:

- `openai`: sends a `GET {base_url}/models` request, including the API key
  header when `api_key_env` is set. A 2xx response is `ok` and shows the first
  model id in detail. A missing environment variable reports
  `api key variable X is not set`. HTTP 401 and 403 report `auth required`.
  Timeouts and connection failures report `unreachable`.
- `command`: resolves `command[0]` after expanding a leading `~/`. An absolute
  or relative path must exist and be executable; bare names are resolved in
  `PATH`. If missing or not executable, doctor reports `not found: <name>`.
  Shallow checks do not execute the program.
- `--deep`: additionally runs each profile once in an isolated temporary directory
  with the test prompt `Reply with the single word ok.` at `read` permission and a
  120-second timeout. Because this consumes model quota, it is opt-in.

The report annotates unhealthy profiles in the default plan, for example
`claude(unhealthy) -> gemini -> local-agent`. When credits-low mode is on,
metered profiles are omitted from the effective plan.

The verdict is `usable` if at least one profile in the default plan is healthy,
and `NOTHING USABLE` otherwise.

### Exit codes

- `overseer run` exits `0` when the task ends in `review` or `needs-human`, `2`
  when it ends `blocked`, and `1` on an overseer error. It prints one line per attempt.
- `overseer review` exits `0` when the review loop finishes (approved in `review`
  or escalated to `needs-human`), `2` on refusal (if the task is not in status `review`),
  and `1` on an overseer error.
- `overseer doctor` exits `0` when the default attempt plan is `usable`, `2`
  when the verdict is `NOTHING USABLE`, and `1` on an overseer error.
- `overseer merge` and `overseer discard` exit `0` on success, `2` on refusal or
  merge conflict, and `1` on other errors.
- `overseer worker` exits `0` on normal completion (including `--once` and
  `--max-tasks`) or when cleanly stopped by a signal, `3` when another worker
  holds the lock, `130` when terminated immediately by a second shutdown
  signal, and `1` on an overseer error.

## Review field reference

The optional `[review]` table in `overseer.toml` configures automated code review:

| Field | Type | Default | Meaning |
|---|---|---|---|
| `profiles` | array of strings | `[]` | Profile names run concurrently as reviewers. If empty, review is disabled. |
| `max_reworks` | integer | `2` | Maximum rework cycles permitted before escalating to `needs-human`. |
| `rework_profile` | string | none | Profile used for rework runs. Defaults to the task's own profile. |

Review configuration is validated on load: reviewer profile names and `rework_profile` must exist in `[[profiles]]`.

## Landing field reference

The optional `[landing]` table in `overseer.toml` configures automated branch landing:

| Field | Type | Default | Meaning |
|---|---|---|---|
| `remote` | string | `"origin"` | Remote to fetch and push to. |
| `base` | string | `"main"` | Base branch to rebase onto and target for pull requests. |
| `auto_merge` | bool | `false` | When true, fast-forward merges the branch if all gates pass. |
| `test_command` | string | none | Command run via `sh -c` before pushing; required for auto-merge. |

## Escalation field reference

The optional `[escalation]` table in `overseer.toml` configures sensitive path escalation:

| Field | Type | Default | Meaning |
|---|---|---|---|
| `enabled` | bool | `true` | When false, path escalation checks are skipped entirely. |
| `paths` | array of strings | default 15 globs | Glob patterns checked against changed files in the branch diff. |

Globs are validated on config load: invalid glob syntax produces a configuration error immediately.

## Skills

Skills are modular, versioned guidelines stored on disk in the state directory
under `<state-dir>/skills/<name>/v<N>.md`, with an active version pointer file
`<state-dir>/skills/<name>/current`. Skill names must match `[a-z0-9-]+`.

### Skills CLI

```sh
overseer skills add general /path/to/guidelines.md    # creates v1 or next version
overseer skills list                                  # lists name, active version, size
overseer skills show general                          # shows active version content
overseer skills show general --version 1              # shows specific version
overseer skills use-version general 1                 # rollback or roll forward active version
```

### Prompt injection

Profiles specify skills to include using `skills`:

```toml
[[profiles]]
name = "claude"
skills = ["rust-guidelines", "general"]
```

When building the system prompt for any backend, overseer appends each listed
skill that exists on disk:

```markdown
## Skill: <name>
<current content>
```

Unknown skill names produce a tracing warning without halting execution. When an
attempt runs, the active skill names and versions (e.g. `rust-guidelines:v1,general:v2`)
are recorded on the run row (`runs.skills`) for full traceability.

## Learning and lessons

Overseer implements a retrospective learning loop that analyzes past task
outcomes and **proposes** improvements to skills. No lesson or skill update is
ever applied automatically without human review.

### Configuration

Add a `[learning]` section to `overseer.toml`:

```toml
[learning]
profile = "claude"          # profile used to analyze tasks (omitted = learning disabled)
skill = "general"           # target skill to propose improvements for (default: general)
min_signal = true           # only retrospect on rework, blocked, or needs-human (default: true)
```

When `min_signal = true` (the default), a clean first-pass `PASS` (a task that
succeeded on its first attempt with round 0 and no failures) is skipped, because
routine success offers no corrective signal. Tasks that required rework (multiple
runs, retries, or non-zero rounds), ended in `blocked`, or were escalated to
`needs-human` produce signal.

### Retrospectives and lessons

After a task reaches a terminal state (`review`, `blocked`, or `needs-human`), the
worker checks if `[learning].profile` is configured and the signal rule passes.
When triggered, it runs a retrospective:

1. Assembles an analysis prompt containing the task title and body, the final diff
   against the merge base (truncated to 40 KB), all run failures, and the current
   content of the target skill.
2. Prompts the learning profile at `read` permission through its standard attempt plan.
3. The learning profile replies EITHER with exactly `NO CHANGE` OR with the complete
   revised markdown for the target skill (imperative rules, at most 60 lines, no secrets
   or task-specific paths).
4. If a revision is proposed, overseer records a new row in the `lessons` table
   (status `proposed`), writes `<state-dir>/lessons/<id>.md`, computes a unified
   diff against the base version using `git diff --no-index`, and saves it to
   `<state-dir>/lessons/<id>.diff`.

### Lessons CLI

Inspect, apply, or reject proposed lessons:

```sh
overseer lessons list                                  # list all proposed lessons
overseer lessons list --status proposed               # filter by status (proposed, applied, rejected)
overseer lessons show 1                               # print the unified diff
overseer lessons apply 1                              # create next skill version and mark applied
overseer lessons reject 1                             # mark lesson rejected
overseer retro 1                                      # manually run retrospective on task 1
```

When applying a lesson, overseer verifies that the skill's current version on disk
matches the lesson's `base_version`. If the skill has been modified since the lesson
was proposed, the application is safely refused with an explanatory message.

## Profile field reference

| Field | Type | Default | Meaning |
|---|---|---|---|
| `name` | string | required | Unique name; referenced by tasks, `default_profile` and `fallback`. |
| `backend` | `openai` or `command` | required | Which backend runs the profile. |
| `model` | string | required for `openai` | Model name sent to the server. |
| `base_url` | string | required for `openai` | Base URL, e.g. `http://127.0.0.1:8080/v1`. |
| `api_key_env` | string | none | Name of the environment variable holding the API key. |
| `command` | array of strings | required for `command` | argv, run in the worktree. Expands leading `~/` in `command[0]` and substitutes `{workdir}`. |
| `env` | table: string to string | `{}` | Environment variables set on the spawned command process. |
| `prompt_flag` | string | none | Flag preceding the prompt argument for `command` (e.g. `"-p"`). |
| `prompt_stdin` | bool | `false` | Send the prompt on stdin instead of as the last argument. |
| `role_prompt` | string | none | System prompt, sent ahead of the task text. |
| `skills` | array of strings | `[]` | Names of skills from `<state-dir>/skills` to append to the system prompt. |
| `fallback` | array of profile names | `[]` | Profiles to try, in order, after this one fails routably. |
| `permissions` | `read`, `write-workdir` or `write-shell` | `read` | Ceiling. A run never gets more than this. |
| `permission_args` | table: level to array of strings | `{}` | Extra argv per effective permission level, before the prompt; substitutes `{workdir}`. |
| `metered` | bool | `false` | Spends paid credits. Skipped in credits-low mode. |
| `timeout_secs` | integer | `1500` | Seconds before an attempt is killed. |
| `agent_tools` | bool | `false` | Run an agent tool loop instead of a single request (`openai` backend only). |
| `max_steps` | integer | `100` | Maximum number of tool iterations before the run fails. |

Configs are validated on load: unknown keys, duplicate names, invalid skill
names, unknown `default_profile` or `fallback` names, and fallback cycles are all
rejected.

## Licence

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your option.
