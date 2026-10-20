# Agent monitor (`ovs`): design

- Date: 2026-10-20T06:00:00Z
- Status: Approved design, not yet planned
- Scope: sub-project 1 of the overseer redesign

## 1. Why

Overseer today is one binary that runs one fixed pipeline (sync, claim,
worktree, route, build, escalate, review, rework, split, retro, land). It has
no event log, keeps only the final output of each agent run, and cannot see
agents it did not launch. The user runs Claude Code, opencode (local models)
and agy (Gemini) on several machines and has no single place to see what each
agent is doing, what it costs, or when it needs them.

The redesign follows the unix philosophy: small commands that each do one job
and connect through a shared, line-oriented event stream. It is split into
three sub-projects, each with its own spec, plan and build:

1. **Agent monitor** (this document): collect events from every agent source
   on every machine into one store, with a live web view, history, cost and
   alerts. Read-only.
2. **Overseer emits events**: overseer's backends emit the same event schema
   (parse `claude --output-format stream-json`, keep the openai tool-loop
   messages, real token usage instead of the static per-profile numbers).
3. **Factory split**: break `runner.rs` / `review.rs` / `worker.rs` into
   composable `ovs` subcommands around a queue that is the only writer of task
   state.

Control of agents (stop, send message, resume) is v2 of the monitor, after the
read side exists.

## 2. Requirements

- Sources in v1: Claude Code (interactive and subagents), opencode (including
  `delegate-to-local` agent runs), agy.
- Several machines report to one central view over the tailnet. The central
  host (`ovs serve`) is ali-desktop.
- Live view, history with search, token and cost accounting, attention alerts.
- Alerts to ntfy, desktop (`notify-send`) and the web UI, through a pluggable
  notifier.
- Storage behind a trait with SQLite and PostgreSQL implementations.
- Web UI: server-rendered with htmx.

Non-goals for v1: controlling agents, shipping full transcripts centrally,
OTLP export (the schema is mappable; an exporter can come later), the factory
itself.

## 3. Components and data flow

```
 each machine                                     central host
 ─────────────                                    ────────────
 ovs collect claude ──┐                            ovs serve
 ovs collect opencode ┼─ NDJSON ─▶ ovs ship ─HTTP─▶  ├ ingest ─▶ EventStore (append-only events
 ovs collect agy ─────┤            (disk spool,      │                      + sessions projection)
 ovs claude-hook ─────┘             retry)           ├ GET /events/stream (SSE)
                                                     └ web UI (htmx + SSE)

 ovs tail | ovs alert | ovs ship          (alerts go back into the log)
 ovs tail --kind alert | ovs notify desktop|ntfy|<any executable>
```

One binary, `ovs`, with one subcommand per job:

| Subcommand | Job | Input | Output |
|---|---|---|---|
| `collect claude` | Tail `~/.claude/projects/**/*.jsonl` and `~/.claude/sessions/*.json` | Claude Code store | NDJSON events |
| `collect opencode` | Poll the opencode DB `event` table by rowid, tail `~/.cache/delegate-to-local/agent-runs/*.jsonl` | opencode store | NDJSON events |
| `collect agy` | Tail `~/.gemini/antigravity-cli/brain/*/.system_generated/logs/transcript_full.jsonl`, poll `conversation_summaries.db` | agy store | NDJSON events |
| `claude-hook` | Run from Claude Code hooks (`Notification`, `PreToolUse`, `Stop`); write one event to the local spool | hook JSON on stdin | spool entry |
| `ship` | Spool stdin to disk, POST batches to the central host, send `host.heartbeat` every 60 s | NDJSON events | HTTP |
| `serve` | `POST /events` (ingest), `GET /events/stream` (SSE), web UI | HTTP | EventStore, HTML |
| `tail` | Stream events from the central host | SSE | NDJSON events |
| `alert` | Apply alert rules | NDJSON events | NDJSON `alert.*` events |
| `notify <sink>` | Deliver alerts (`ntfy`, `desktop`) | NDJSON `alert.*` events | side effect |

Every subcommand reads or writes NDJSON on stdin/stdout, so each can be tested
alone (`cat fixture.ndjson | ovs alert`). Collectors keep a cursor file and
resume after a restart. `ingest` and the web UI are separate modules served by
one process (one port, one unit).

The monitor lives in new crates in this workspace. The existing overseer
crates are left unchanged by this sub-project.

## 4. Event schema (v1)

One JSON object per line:

```json
{"v":1,"id":"<hex32>","ts":"2026-10-20T05:33:12.123Z","host":"ali-desktop",
 "source":"claude-code","session":"claude-code:6b3bd7d6-…","parent":null,
 "kind":"tool.called","data":{"call_id":"toolu_01…","name":"Bash","input":"cargo test …"}}
```

- `id`: first 32 hex characters of SHA-256 over `host`, `source`, native
  session id and native position (JSONL byte offset, opencode `event.rowid`,
  agy `step_index`). Re-reading a source yields the same ids.
- `ts`: ISO8601 UTC with milliseconds, taken from the source record.
- `source`: `claude-code` | `opencode` | `agy` | `ovs` (for `host.heartbeat`
  and alerts).
- `session`: `<source>:<native session id>`.
- `parent`: the parent session for subagents (Claude Code: the session whose
  `Agent` tool_use id equals `meta.toolUseId`; opencode: `session.parent_id`;
  agy: `parent_conversation_id`), otherwise `null`.
- `kind`: a closed set. Adding a kind bumps `v`.

| kind | data |
|---|---|
| `session.started` | `cwd`, `git_branch`, `title`, `agent`, `pid`, `model` (each nullable) |
| `session.ended` | `reason`: `exited` \| `archived` \| `killed` \| `stale` |
| `session.status` | `state`: `working` \| `idle` \| `waiting_input` \| `waiting_permission` |
| `prompt` | `text`, `by`: `user` \| `agent` |
| `message` | `text`, `model` |
| `tool.called` | `call_id`, `name`, `input` |
| `tool.result` | `call_id`, `ok`, `error`, `output` |
| `usage` | `model`, `input`, `output`, `cache_read`, `cache_write`, `reasoning`, `cost_usd` (nullable) |
| `error` | `class`: `rate_limit` \| `api` \| `context_overflow` \| `aborted` \| `parse` \| `other`, `message` |
| `host.heartbeat` | `collectors`: list of running collectors and their cursor lag |
| `alert.raised` | `alert_id`, `rule`, `severity`: `high` \| `normal`, `target`, `message` |
| `alert.cleared` | `alert_id` |

Content policy:

- Text fields (`text`, `input`, `output`, `message`) are truncated to 2 KB.
- Every event from a collector carries `ref`: `{host, path, offset}` pointing
  at the full record on the origin machine. Full transcripts are never shipped.
- Collectors redact before emitting: API-key shapes, `Authorization:` headers,
  and values of `*_TOKEN`, `*_KEY`, `*_SECRET`, `PASSWORD` assignments become
  `[redacted]`.
- When the source gives no cost, `serve` prices `usage` from a price table in
  its config; `cost_usd` stays `null` for unknown models.

Field names map onto OTel `gen_ai.*` attributes (`session` →
`gen_ai.conversation.id`, `usage.input` → `gen_ai.usage.input_tokens`, and so
on) so an OTLP exporter can be added without changing the schema.

### Per-source mapping notes

- **Claude Code.** One API message is split into one JSONL line per content
  block, each repeating `message.id` and `usage`: emit `usage` once per
  `message.id`. Subagents live in `<sessionId>/subagents/agent-<id>.jsonl`
  with `agent-<id>.meta.json`. Live sessions: `~/.claude/sessions/<pid>.json`
  (`status` idle/busy, `sessionId`, `cwd`, `startedAt`); a session ends when
  its pid is gone from `/proc` or its file is removed. Turn end:
  `system` records with subtype `turn_duration` or `stop_hook_summary`. Errors:
  `isApiErrorMessage` with `error`. Some files lack a trailing newline: buffer
  a partial last line until it is completed. Permission prompts and waits for
  input are not written to the transcript; `claude-hook` supplies them.
- **opencode.** Primary store `~/.local/share/opencode/opencode-stable.db`,
  opened read-only. Tail `event` by rowid; order per session by
  `(aggregate_id, seq)`. Tool calls are `part` rows with `type = 'tool'`
  (`callID`, `state.status`, `state.input`, `state.output`/`state.error`).
  Usage and cost per `step-finish` part. A message with no `time.completed`
  is in progress. `delegate-to-local` runs write their own event JSONL under
  `~/.cache/delegate-to-local/agent-runs/`, which the collector also tails.
- **agy.** `transcript_full.jsonl` records carry `step_index`, `source`,
  `type` (`USER_INPUT`, `PLANNER_RESPONSE`, `GENERIC`, `SYSTEM_MESSAGE`,
  `CHECKPOINT`), `status`, `created_at`. Tool calls are in
  `PLANNER_RESPONSE.tool_calls`; results are the following `GENERIC` records,
  paired by step order (verify against fixtures before relying on it). Usage
  is per `PLANNER_RESPONSE`. cwd, status, `killed` and parent come from
  `conversation_summaries.db`; the model comes from `log/cli.log`
  ("Model ID … resolved") and may be `null`. Permission waits are not
  decodable (protobuf); agy gets the heuristic in section 5 only.

## 5. Session state and alerts

Session state is a pure function `fold(state, event) -> state` in a shared
library, used by `serve` (to maintain the `sessions` projection in the same
transaction as the append) and by `alert`.

| State | Entered on |
|---|---|
| `working` | `prompt`, `tool.called`, `message`, or `session.status` working |
| `idle` | `session.status` idle (turn ended) |
| `waiting_permission` | `session.status` from `claude-hook`; for sources without hooks, a `tool.called` with no `tool.result` for over 30 s while the source reports idle |
| `waiting_input` | an open question tool call (Claude `AskUserQuestion`, opencode `question`) |
| `errored` | the last event is `error`; cleared by the next activity |
| `ended` | `session.ended`, or 24 h with no events |

Alert rules (thresholds in TOML). Each fires once per `(rule, session)` and
emits `alert.cleared` when its condition ends:

1. `waiting_permission` over 60 s: high.
2. `waiting_input` over 2 min: normal.
3. `error` with class `context_overflow`: high; `rate_limit` or `api`: normal.
4. Runaway: the same tool and input 5 or more times in a row, or more than N
   tool calls without a user prompt, or tokens per 10 min above a threshold:
   high.
5. Stuck: `working` with no events for over 10 min: normal.
6. Host silent: no `host.heartbeat` for over 5 min: high.
7. Daily spend over budget (per host, per source, total): normal.
8. Collector parse errors above a rate (a source format changed): normal.

Routing by severity: high goes to ntfy and desktop, normal to desktop; every
alert shows in the web UI. `alert` rebuilds its open-alert set from the log on
start so a restart does not re-fire.

### Notifiers

A notifier is anything that reads `alert.*` NDJSON on stdin. Built-in sinks
implement a Rust trait:

```rust
#[async_trait]
pub trait Notifier: Send + Sync {
    fn name(&self) -> &str;
    async fn notify(&self, alert: &Alert) -> Result<(), NotifyError>;
}
```

`ovs notify ntfy` and `ovs notify desktop` are the v1 implementations. Any
other executable can sit in the same pipe position.

## 6. Storage

```rust
#[async_trait]
pub trait EventStore: Send + Sync {
    /// Idempotent on `Event::id`; updates the sessions projection in the same transaction.
    async fn append(&self, batch: &[Event]) -> Result<AppendReport, StoreError>;
    async fn events(&self, q: &EventQuery) -> Result<Vec<Stored<Event>>, StoreError>;
    async fn subscribe(&self, after: Seq) -> Result<BoxStream<'static, Result<Stored<Event>, StoreError>>, StoreError>;
    async fn sessions(&self, q: &SessionQuery) -> Result<Vec<SessionState>, StoreError>;
    async fn usage(&self, q: &UsageQuery) -> Result<Vec<UsageRow>, StoreError>;
}
```

- The store assigns a strictly increasing `seq` on insert (SQLite rowid,
  Postgres `bigserial`). All cursors use `seq`.
- `subscribe`: Postgres uses `LISTEN/NOTIFY`; SQLite polls by `seq` every
  250 ms. Both work across processes.
- Implementations: `SqliteStore` and `PostgresStore`, both on sqlx, written
  separately (not `AnyPool`) because JSON functions and notification differ.
- The backend is chosen by a URL in config: `sqlite://…` or `postgres://…`.
- Tables: `events` (seq, id unique, ts, host, source, session, parent, kind,
  data JSON, ref JSON), `sessions` (projection), `alerts` (open/cleared,
  acknowledged).

## 7. Web UI and serving

`ovs serve` on the central host: axum, maud or askama templates, htmx with its
SSE extension, assets embedded in the binary.

Pages:

1. Live board: sessions grouped by host; state chip, one-line current
   activity, age, tokens; subagents nested under their parent.
2. Session detail: event timeline (prompts, tool calls and results, errors),
   subagent tree, usage totals, transcript pointer `host:path#offset`.
3. History: filter by source, host, project, date; full-text search over
   snippets.
4. Cost: per day by source, model and host, with the budget line.
5. Alerts: open and cleared, with an acknowledge button.

Security: `serve` binds to the tailnet address only. Each host authenticates
`POST /events` with a bearer token read from a file (managed by sops or
agenix). UI access relies on Tailscale identity headers via `tailscale serve`;
no login of its own.

## 8. Failure handling

- **Collectors.** The cursor file is written atomically (temp file, then
  rename) only after the event is emitted: a crash duplicates, never loses;
  `ingest` drops duplicates. An unparseable record becomes an `error` event
  with class `parse` and the collector continues.
- **`ship`.** Appends to the spool before sending. Retries with exponential
  backoff capped at 60 s. Spool capped at 500 MB; beyond that, the oldest
  events lose their text fields first, metadata is kept. Exit codes: 0 stdin
  closed and spool drained, 2 bad config, 3 authentication rejected.
- **`ingest`.** One transaction per batch. A malformed event is rejected
  individually (`400` listing its index); the rest commit.
- **`alert` / `notify`.** A failing notifier is logged and retried 3 times; it
  never blocks the pipeline.

## 9. Testing

- Collectors: secret-scrubbed recorded fixtures from each source, run through
  the collector and diffed against expected NDJSON.
- Projection: property tests that `fold` is deterministic for a given `seq`
  order and that duplicate events do not change state.
- Store: one conformance suite run against SQLite and an ephemeral Postgres
  from the Nix dev shell.
- End to end: `cat fixtures | ovs ship` into `ovs serve` (SQLite), read back
  with `ovs tail`.
- Alerts: `cat events | ovs alert` diffed against expected alerts.

## 10. Milestones

Each ends usable and leaves `cargo test` and `nix build` green.

1. Schema crate, `fold`, `ovs collect claude` to stdout
   (`ovs collect claude | jq` works).
2. `EventStore` trait with `SqliteStore`; `ovs serve` ingest; `ovs ship`.
   One machine end to end.
3. Web live board and session detail.
4. `ovs collect opencode` and `ovs collect agy`.
5. `ovs claude-hook`, `ovs alert`, `ovs notify ntfy`, `ovs notify desktop`.
6. `PostgresStore`, history and cost pages, home-manager module running
   `collect | ship` as systemd user units on each machine.

## 11. Open points

- agy permission waits stay heuristic until its step protobuf can be decoded.
- The opencode DB was last written 2026-10-07 while agent runs continued to
  2026-10-18; milestone 4 confirms where current opencode versions write.
