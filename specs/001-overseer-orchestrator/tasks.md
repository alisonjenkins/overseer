---
description: "Task list for Overseer task orchestrator implementation"
---

# Tasks: Overseer Task Orchestrator

**Input**: Design documents from `/specs/001-overseer-orchestrator/`
**Prerequisites**: [plan.md](plan.md), [spec.md](spec.md)

## Format: `[ID] [P?] [Story] Description`
- **[P]**: Can run in parallel (touches different files, no blocking dependency)
- **[Story]**: Associated user story (US1, US2, US3, US4)

---

## Phase 1: Setup & Workspace Foundation

- [x] T001 Setup multi-crate Cargo workspace and shared dependencies in Cargo.toml
- [x] T002 [P] Configure Nix flake devShell and package definitions in flake.nix
- [x] T003 [P] Implement core configuration schema and TOML parser in crates/overseer-core/src/config.rs

---

## Phase 2: Foundational Storage & Task Management

- [x] T004 Define core task, run, status, and permission types in crates/overseer-core/src/types.rs
- [x] T005 [P] Implement SQLite database schema migrations in crates/overseer-store/migrations
- [x] T006 Implement atomic SQLite store operations and CRUD queries in crates/overseer-store/src/store.rs
- [x] T007 Implement CLI argument parsing and state directory resolution in crates/overseer-cli/src/cli.rs
- [x] T008 Implement overseer init command creating starter configuration in crates/overseer-cli/src/commands.rs
- [x] T009 Implement overseer task add command with priority and dependencies in crates/overseer-cli/src/commands.rs
- [x] T010 Implement overseer task list command with status filtering in crates/overseer-cli/src/commands.rs
- [x] T011 Implement overseer task show command displaying task and review details in crates/overseer-cli/src/commands.rs
- [x] T012 Implement overseer task ready command transitioning tasks in crates/overseer-cli/src/commands.rs
- [x] T013 Implement overseer task discard command cleaning worktrees in crates/overseer-cli/src/commands.rs
- [x] T014 Implement overseer status command reporting queue summary in crates/overseer-cli/src/commands.rs
- [x] T015 Implement overseer config starter and config path commands in crates/overseer-cli/src/commands.rs

---

## Phase 3: User Story 1 - Local-First Task Execution & Model Fallback (Priority: P1) 🎯 MVP

- [x] T016 [US1] Define Backend and TaskSource traits and run requests in crates/overseer-core/src/lib.rs
- [x] T017 [P] [US1] Implement git worktree and branch lifecycle in crates/overseer-orchestrator/src/worktree.rs
- [x] T018 [P] [US1] Implement OpenAI-compatible HTTP chat client in crates/overseer-backends/src/openai.rs
- [x] T019 [US1] Implement worktree-confined agent tool loop in crates/overseer-backends/src/openai.rs
- [x] T020 [P] [US1] Implement command backend for harness CLIs in crates/overseer-backends/src/command.rs
- [x] T021 [P] [US1] Implement fallback routing engine and failure classification in crates/overseer-orchestrator/src/router.rs
- [x] T022 [US1] Implement credits-low mode skipping metered profiles in crates/overseer-orchestrator/src/router.rs
- [x] T023 [P] [US1] Implement overseer mode commands in crates/overseer-cli/src/commands.rs
- [x] T024 [P] [US1] Implement task runner executing attempt plans in crates/overseer-orchestrator/src/runner.rs
- [x] T025 [P] [US1] Implement overseer run and runs commands in crates/overseer-cli/src/commands.rs
- [x] T026 [P] [US1] Implement profile health checks and overseer doctor in crates/overseer-orchestrator/src/doctor.rs
- [ ] T027 [P] [US1] Implement token usage tracking for command profiles in crates/overseer-backends/src/command.rs

---

## Phase 4: User Story 2 - Automated Review & Bounded Rework Loop (Priority: P2)

- [x] T028 [US2] Implement reviewer prompt assembly with unified diff in crates/overseer-orchestrator/src/review.rs
- [x] T029 [US2] Implement first-line VERDICT parsing and retry in crates/overseer-orchestrator/src/review.rs
- [x] T030 [US2] Implement concurrent reviewer execution and round barrier in crates/overseer-orchestrator/src/review.rs
- [x] T031 [US2] Implement multi-round rework loop up to max_reworks in crates/overseer-orchestrator/src/review.rs
- [x] T032 [US2] Implement reviewer ESCALATE handling and escalation reason storage in crates/overseer-orchestrator/src/review.rs
- [x] T033 [P] [US2] Implement overseer diff command in crates/overseer-cli/src/commands.rs
- [x] T034 [US2] Implement overseer review command in crates/overseer-cli/src/commands.rs
- [ ] T035 [P] [US2] Implement approver step after reviewer pass in crates/overseer-orchestrator/src/review.rs

---

## Phase 5: User Story 3 - Safety Gates, Hard Escalation & Landing (Priority: P3)

- [x] T036 [US3] Implement sensitive path escalation checking via globset in crates/overseer-orchestrator/src/escalation.rs
- [x] T037 [US3] Integrate hard escalation halt before review runs in crates/overseer-orchestrator/src/runner.rs
- [x] T038 [P] [US3] Implement overseer approve command in crates/overseer-cli/src/commands.rs
- [x] T039 [US3] Implement overseer merge command with git merge-tree in crates/overseer-cli/src/commands.rs
- [x] T040 [P] [US3] Implement automated landing pipeline with git rebase and test check in crates/overseer-orchestrator/src/landing.rs
- [x] T041 [US3] Implement GitHub PR creation and fast-forward auto-merge in crates/overseer-orchestrator/src/landing.rs
- [x] T042 [P] [US3] Implement overseer land command in crates/overseer-cli/src/commands.rs
- [x] T043 [US3] Implement branch reconciliation marking merged branches done in crates/overseer-cli/src/commands.rs
- [ ] T044 [P] [US3] Implement landing commit speckit checkbox ticking for completed task lines in crates/overseer-orchestrator/src/landing.rs
- [ ] T045 [P] [US3] Run live end-to-end test of escalation against a real local model in tests/e2e/test_escalation.sh

---

## Phase 6: User Story 4 - Continuous Draining, SpecKit Sync & Retrospective Learning (Priority: P4)

- [x] T046 [US4] Implement single-worker PID file lock in crates/overseer-orchestrator/src/lock.rs
- [x] T047 [P] [US4] Implement worker startup crash recovery in crates/overseer-orchestrator/src/worker.rs
- [x] T048 [US4] Implement continuous queue drainer loop with signal handling in crates/overseer-orchestrator/src/worker.rs
- [x] T049 [P] [US4] Implement overseer worker command in crates/overseer-cli/src/commands.rs
- [x] T050 [P] [US4] Implement disk-based versioned skills management in crates/overseer-orchestrator/src/skills.rs
- [x] T051 [P] [US4] Implement overseer skills CLI subcommands in crates/overseer-cli/src/commands.rs
- [x] T052 [P] [US4] Inject active skills into profile prompts and record on runs in crates/overseer-orchestrator/src/runner.rs
- [x] T053 [P] [US4] Implement retrospective analysis and proposed lesson diffs in crates/overseer-orchestrator/src/learning.rs
- [x] T054 [P] [US4] Implement overseer lessons and overseer retro commands in crates/overseer-cli/src/commands.rs
- [x] T055 [P] [US4] Implement SpecKit tasks.md markdown parser in crates/overseer-orchestrator/src/sources/speckit.rs
- [x] T056 [P] [US4] Implement source synchronization engine with add/update/cancel/revive in crates/overseer-orchestrator/src/sync.rs
- [x] T057 [P] [US4] Implement asynchronous filesystem watcher with debouncing in crates/overseer-orchestrator/src/watch.rs
- [x] T058 [P] [US4] Implement overseer sync command in crates/overseer-cli/src/commands.rs
- [x] T059 [US4] Implement SPECS progress reporting in overseer status in crates/overseer-cli/src/commands.rs
- [ ] T060 [P] [US4] Run live end-to-end test of lessons and retro against a real local model
- [ ] T061 [P] [US4] Run live synchronization test of speckit sync against a real spec directory
- [ ] T062 [P] [US4] Implement host system service module for continuous worker execution

---

## Phase 7: Future Milestones & API Extensions

- [ ] T063 [P] Implement HTTP JSON API for remote status inspection and task submission

---

## Dependencies & Execution Order

### Phase Dependencies
- **Phase 1 (Setup)**: Baseline workspace setup and TOML parsing.
- **Phase 2 (Foundational)**: Depends on Phase 1; establishes SQLite storage, entities, and basic CLI task commands.
- **Phase 3 (User Story 1)**: Depends on Phase 2; implements model execution, tool calling, and fallback routing.
- **Phase 4 (User Story 2)**: Depends on Phase 3; implements automated reviewer loops and rework.
- **Phase 5 (User Story 3)**: Depends on Phase 4; implements sensitive path safety gates and branch landing.
- **Phase 6 (User Story 4)**: Depends on Phase 5; implements background queue draining, learning, and SpecKit sync.
- **Phase 7 (Future)**: Depends on Phase 6; optional HTTP API for remote automation.

## Phase 8: Monitoring and Loose Coupling

- [ ] T072 Write the component boundary rules from plan.md into a short doc under docs/ and check that the doc matches the dependency rules
- [ ] T073 Add a migration for an append-only run_events table (run_id, seq, kind, detail, created_at) in crates/overseer-store/migrations, with store functions append_event and list_events_since
- [ ] T074 Emit run events from the runner, review and landing code through the store functions, for step start, tool call, verdict, and landing outcome
- [ ] T075 Add overseer events with --run, --follow and --json flags that print one JSON object per line and follow the log like tail -f
- [ ] T076 Add overseer ps with a --json flag that lists running tasks with profile, step count and elapsed time
- [ ] T077 Check the prompt size against the profile context limit before a run is sent, and fail with a clear message instead of an HTTP 400 from the server
- [ ] T078 Bound the reviewer context by a token budget tied to the review profile, so changed-file contents cannot overflow the reviewer's context
- [ ] T079 Add an overseer-web binary that serves read-only JSON from the store, with no dependency on overseer-orchestrator
- [ ] T080 Add a single static HTML page that polls the overseer-web JSON endpoints, with no build step
- [ ] T081 Add a CI check that fails if overseer-web or a monitoring command depends on overseer-orchestrator (cargo tree or cargo metadata)
- [ ] T082 Mark the phase 7 HTTP API task (T063) as superseded by T079 in the spec, once T079 is merged

