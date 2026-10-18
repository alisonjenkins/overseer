# Overseer

Overseer is a single-user, local-first task orchestrator. It keeps a queue of
software tasks in SQLite and is meant to hand them to whichever model backend
is available: a local OpenAI-compatible server, the `agy` CLI, `claude -p`, or
a shell command. The design is in [docs/spec.md](docs/spec.md); section 10
lists the milestones. This repository currently implements M2: the task store,
the `openai` and `command` backends, per-task worktrees, and the `overseer`
CLI for `init`, `task`, `run` and `runs`.

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
overseer task add "write parser" --body "handle quotes" --priority 2
overseer task add "write tests" --priority 1 --depends-on 1
overseer task list --status backlog
overseer task show 2
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

## Profiles

Profiles live in `<state-dir>/overseer.toml`. Each task names a profile with
`--profile`, or uses `default_profile`.

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

The `openai` backend sends one non-streaming request per run. A system message
comes from `role_prompt`, then the task title and body as the user message.
Only `message.content` is kept; `reasoning_content` is ignored. A chat-completions
backend returns text only and does not edit files. Use a `command` profile for
work that must change files.

### Command profiles (harness CLIs)

A `command` profile runs an external program in the task worktree. The prompt is
the last argument, or stdin when `prompt_stdin = true`. A role prompt is put
ahead of the task text, since a plain argv has no system channel. Command
backends report no token usage, so `run` shows zeros for them.

The two examples below are **untested**. They show the shape of the profile and
have not been run against the current CLI versions. Check each tool's flags
before relying on it.

```toml
# Untested example: opencode's non-interactive mode, prompt as last argument.
[[profiles]]
name = "opencode"
backend = "command"
command = ["opencode", "run"]
permissions = "write-workdir"
timeout_secs = 1500

# Untested example: Antigravity CLI (Gemini) in print mode. No permission flag
# is passed yet, so the CLI may stop at its own edit approval prompt; the
# permission mapping for `agy` is planned for M3.
[[profiles]]
name = "agy"
backend = "command"
command = ["agy", "-p"]
permissions = "write-workdir"
timeout_secs = 1500
```

Add a `fallback` list to a profile to name the profiles to try after it. Fallback
routing is not active until M3, so for now each run uses only its own profile.
