# Overseer

Overseer is a single-user, local-first task orchestrator. It keeps a queue of
software tasks in SQLite and is meant to hand them to whichever model backend
is available: a local OpenAI-compatible server, the `agy` CLI, `claude -p`, or
a shell command. The design is in [docs/spec.md](docs/spec.md); section 10
lists the milestones. This repository currently implements M3: the task store,
the `openai` and `command` backends, per-task worktrees, fallback routing with
credits-low mode, and the `overseer` CLI for `init`, `task`, `run`, `runs`,
`mode`, `diff`, `merge` and `discard`.

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

## Reviewing and landing tasks

Once a task reaches `review`, you can inspect what it produced, merge it into
your repository branch, or discard it back to the backlog.

```sh
overseer diff 1
overseer diff 1 --stat
overseer merge 1
overseer discard 1
```

- `overseer diff <id> [--stat]` prints the unified diff (or `--stat` summary)
  between the base branch tip and the task branch `overseer/task-<id>`,
  without invoking a pager.
- `overseer merge <id>` checks that the repository working tree is clean and
  that HEAD is on the branch the task branched from. It runs
  `git merge-tree --write-tree HEAD <branch>` first and returns a conflict error
  without modifying the repository if conflicts exist. Otherwise it merges with
  `git merge --no-ff -m "overseer: merge task <id>: <title>" <branch>`, removes
  the task worktree, deletes the branch with `git branch -d`, sets the task
  status to `done`, and prints the merge commit sha. Tasks may only be merged
  from status `review`.
- `overseer discard <id>` removes the task worktree and branch (`git branch -D`)
  and resets the task status to `backlog`. It refuses unless the task status is
  `review` or `blocked`.

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
without tool calls. If the loop exceeds `max_steps` (default 40), the run fails.

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
  `max_steps = 40`, and `permissions = "write-workdir"`. Requires a llama-server
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

### Exit codes

- `overseer run` exits `0` when the task ends in `review`, `2` when it ends
  `blocked`, and `1` on an overseer error. It prints one line per attempt.
- `overseer merge` and `overseer discard` exit `0` on success, `2` on refusal or
  merge conflict, and `1` on other errors.

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
| `fallback` | array of profile names | `[]` | Profiles to try, in order, after this one fails routably. |
| `permissions` | `read`, `write-workdir` or `write-shell` | `read` | Ceiling. A run never gets more than this. |
| `permission_args` | table: level to array of strings | `{}` | Extra argv per effective permission level, before the prompt; substitutes `{workdir}`. |
| `metered` | bool | `false` | Spends paid credits. Skipped in credits-low mode. |
| `timeout_secs` | integer | `1500` | Seconds before an attempt is killed. |
| `agent_tools` | bool | `false` | Run an agent tool loop instead of a single request (`openai` backend only). |
| `max_steps` | integer | `40` | Maximum number of tool iterations before the run fails. |

Configs are validated on load: unknown keys, duplicate names, unknown
`default_profile` or `fallback` names, and fallback cycles are all rejected.

## Licence

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your option.
