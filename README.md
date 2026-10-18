# Overseer

Overseer is a single-user, local-first task orchestrator. It keeps a queue of
software tasks in SQLite and is meant to hand them to whichever model backend
is available: a local OpenAI-compatible server, the `agy` CLI, `claude -p`, or
a shell command. The design is in [docs/spec.md](docs/spec.md); section 10
lists the milestones. This repository currently implements M3: the task store,
the `openai` and `command` backends, per-task worktrees, fallback routing with
credits-low mode, and the `overseer` CLI for `init`, `task`, `run`, `runs` and
`mode`.

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

A command profile's `permission_args` lists extra arguments per permission level
(`read`, `write-workdir`, `write-shell`). They go before the prompt, for the
level the run actually gets, which is the lower of what was asked and the
profile's `permissions` ceiling.

`overseer init` writes these profiles. Check each tool's flags against your
installed version before relying on them; the test suite does not run them.

- `local`: the default profile, an `openai` server on `127.0.0.1:8080`.
- `gemini`: `agy --model gemini-3.8-flash-medium`, `prompt_flag = "-p"`, with
  `permissions = "write-shell"` (`--dangerously-skip-permissions`). Falls back
  to `local`.
- `claude`: `claude -p`, `metered = true`, with `--permission-mode` flags.
  Falls back to `gemini`, then `local`.
- `opencode-local`: `opencode run`. Untested example, not in any fallback chain.

The starter `gemini` profile sets its ceiling to `write-shell` because headless
`agy` (`-p`) requires the "command" permission to inspect the directory. Under
`--mode accept-edits`, headless mode auto-denies that permission, exits 0 with
empty output, and creates no files. Passing `--dangerously-skip-permissions`
allows it to edit files, which is safe because runs take place in an isolated
task worktree.

The starter keeps `default_profile = "local"`. To try `claude` first, set
`default_profile = "claude"`; the chain then covers it when it cannot run.

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

`overseer run` exits `0` when the task ends in `review`, `2` when it ends
`blocked`, and `1` on an overseer error. It prints one line per attempt.

## Profile field reference

| Field | Type | Default | Meaning |
|---|---|---|---|
| `name` | string | required | Unique name; referenced by tasks, `default_profile` and `fallback`. |
| `backend` | `openai` or `command` | required | Which backend runs the profile. |
| `model` | string | required for `openai` | Model name sent to the server. |
| `base_url` | string | required for `openai` | Base URL, e.g. `http://127.0.0.1:8080/v1`. |
| `api_key_env` | string | none | Name of the environment variable holding the API key. |
| `command` | array of strings | required for `command` | argv, run in the worktree. |
| `prompt_flag` | string | none | Flag preceding the prompt argument for `command` (e.g. `"-p"`). |
| `prompt_stdin` | bool | `false` | Send the prompt on stdin instead of as the last argument. |
| `role_prompt` | string | none | System prompt, sent ahead of the task text. |
| `fallback` | array of profile names | `[]` | Profiles to try, in order, after this one fails routably. |
| `permissions` | `read`, `write-workdir` or `write-shell` | `read` | Ceiling. A run never gets more than this. |
| `permission_args` | table: level to array of strings | `{}` | Extra argv per effective permission level, before the prompt. |
| `metered` | bool | `false` | Spends paid credits. Skipped in credits-low mode. |
| `timeout_secs` | integer | `1500` | Seconds before an attempt is killed. |

Configs are validated on load: unknown keys, duplicate names, unknown
`default_profile` or `fallback` names, and fallback cycles are all rejected.
