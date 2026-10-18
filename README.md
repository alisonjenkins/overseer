# Overseer

Overseer is a single-user, local-first task orchestrator. It keeps a queue of
software tasks in SQLite and is meant to hand them to whichever model backend
is available: a local OpenAI-compatible server, the `agy` CLI, `claude -p`, or
a shell command. The design is in [docs/spec.md](docs/spec.md); section 10
lists the milestones. This repository currently implements M1: the task store
and the `overseer` CLI for `init`, `task add`, `task list` and `task show`.

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
