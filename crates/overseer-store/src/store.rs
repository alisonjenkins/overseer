use std::path::{Path, PathBuf};

use overseer_core::{NewTask, Run, RunFinish, RunId, RunStatus, Task, TaskId, TaskStatus, Usage};
use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row, params};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::StoreError;

const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("../migrations/0001_init.sql")),
    (2, include_str!("../migrations/0002_runs.sql")),
    (3, include_str!("../migrations/0003_settings.sql")),
];

/// Settings key holding the `credits_low` mode flag, stored as `"true"` or
/// `"false"`.
const CREDITS_LOW_KEY: &str = "credits_low";

const SELECT_TASK: &str =
    "SELECT id, title, body, status, priority, profile, round, created_at FROM tasks";

const SELECT_RUN: &str = "SELECT id, task_id, profile, status, started_at, finished_at, \
     input_tokens, output_tokens, failure, transcript_path, branch FROM runs";

fn now_rfc3339() -> Result<String, StoreError> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|source| StoreError::FormatTimestamp { source })
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let conn = Connection::open(path).map_err(|source| StoreError::OpenDatabase {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_connection(conn)
    }

    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn =
            Connection::open_in_memory().map_err(|source| StoreError::OpenInMemory { source })?;
        Self::from_connection(conn)
    }

    fn from_connection(mut conn: Connection) -> Result<Self, StoreError> {
        migrate(&mut conn)?;
        Ok(Self { conn })
    }

    pub fn add_task(&mut self, new: NewTask) -> Result<Task, StoreError> {
        let NewTask {
            title,
            body,
            priority,
            profile,
            mut depends_on,
            ready,
        } = new;
        depends_on.sort_unstable();
        depends_on.dedup();
        let created_at = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .map_err(|source| StoreError::FormatTimestamp { source })?;
        let status = if ready {
            TaskStatus::Ready
        } else {
            TaskStatus::Backlog
        };

        let tx = self
            .conn
            .transaction()
            .map_err(|source| StoreError::InsertTask {
                title: title.clone(),
                source,
            })?;

        for &dep in &depends_on {
            let exists: bool = tx
                .query_row(
                    "SELECT EXISTS (SELECT 1 FROM tasks WHERE id = ?1)",
                    params![dep.get()],
                    |row| row.get(0),
                )
                .map_err(|source| StoreError::CheckDependency {
                    depends_on: dep,
                    source,
                })?;
            if !exists {
                return Err(StoreError::UnknownDependency { depends_on: dep });
            }
        }

        tx.execute(
            "INSERT INTO tasks (title, body, status, priority, profile, round, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6)",
            params![title, body, status.as_str(), priority, profile, created_at],
        )
        .map_err(|source| StoreError::InsertTask {
            title: title.clone(),
            source,
        })?;
        let id = TaskId::new(tx.last_insert_rowid());

        for &dep in &depends_on {
            tx.execute(
                "INSERT INTO task_deps (task_id, depends_on) VALUES (?1, ?2)",
                params![id.get(), dep.get()],
            )
            .map_err(|source| StoreError::InsertDependency {
                depends_on: dep,
                source,
            })?;
        }

        tx.commit().map_err(|source| StoreError::InsertTask {
            title: title.clone(),
            source,
        })?;

        Ok(Task {
            id,
            title,
            body,
            status,
            priority,
            profile,
            round: 0,
            depends_on,
            created_at,
        })
    }

    pub fn get_task(&self, id: TaskId) -> Result<Task, StoreError> {
        let mut task = self
            .conn
            .query_row(
                &format!("{SELECT_TASK} WHERE id = ?1"),
                params![id.get()],
                task_from_row,
            )
            .optional()
            .map_err(|source| StoreError::LoadTask { id, source })?
            .ok_or(StoreError::UnknownTask { id })?;
        task.depends_on =
            load_deps(&self.conn, id).map_err(|source| StoreError::LoadTask { id, source })?;
        Ok(task)
    }

    pub fn list_tasks(&self, status: Option<TaskStatus>) -> Result<Vec<Task>, StoreError> {
        let status_text = status.map(TaskStatus::as_str);
        let list_err = |source| StoreError::ListTasks { source };

        let mut stmt = self
            .conn
            .prepare(&format!(
                "{SELECT_TASK} WHERE ?1 IS NULL OR status = ?1 ORDER BY priority ASC, id ASC"
            ))
            .map_err(list_err)?;
        let mut tasks = stmt
            .query_map(params![status_text], task_from_row)
            .and_then(|rows| rows.collect::<rusqlite::Result<Vec<Task>>>())
            .map_err(list_err)?;

        for task in &mut tasks {
            task.depends_on = load_deps(&self.conn, task.id).map_err(list_err)?;
        }
        Ok(tasks)
    }

    /// Pick the highest-priority eligible ready task whose dependencies are
    /// all `done`. Among ties, the lowest id is chosen. Tasks with unmet
    /// dependencies are skipped.
    pub fn pick_next_ready_task(&self) -> Result<Option<Task>, StoreError> {
        let sql = format!(
            "{SELECT_TASK} WHERE status = 'ready' AND NOT EXISTS ( \
                 SELECT 1 FROM task_deps d \
                 LEFT JOIN tasks dep ON dep.id = d.depends_on \
                 WHERE d.task_id = tasks.id AND (dep.status IS NULL OR dep.status != 'done') \
             ) ORDER BY priority ASC, id ASC LIMIT 1"
        );
        let mut task = self
            .conn
            .query_row(&sql, [], task_from_row)
            .optional()
            .map_err(|source| StoreError::PickNextReadyTask { source })?;
        if let Some(ref mut task) = task {
            task.depends_on = load_deps(&self.conn, task.id)
                .map_err(|source| StoreError::PickNextReadyTask { source })?;
        }
        Ok(task)
    }

    pub fn set_status(&mut self, id: TaskId, status: TaskStatus) -> Result<(), StoreError> {
        let changed = self
            .conn
            .execute(
                "UPDATE tasks SET status = ?1 WHERE id = ?2",
                params![status.as_str(), id.get()],
            )
            .map_err(|source| StoreError::SetStatus { id, source })?;
        if changed == 0 {
            return Err(StoreError::UnknownTask { id });
        }
        Ok(())
    }

    /// Atomically move a `backlog` or `blocked` task to `ready`. Any other
    /// status is an error.
    pub fn ready_task(&mut self, id: TaskId) -> Result<Task, StoreError> {
        let changed = self
            .conn
            .execute(
                "UPDATE tasks SET status = 'ready' \
                 WHERE id = ?1 AND status IN ('backlog', 'blocked')",
                params![id.get()],
            )
            .map_err(|source| StoreError::ReadyTask { id, source })?;
        if changed == 1 {
            return self.get_task(id);
        }
        let current = self.get_task(id)?;
        Err(StoreError::TaskNotReadyable {
            id,
            status: current.status,
        })
    }

    /// Atomically move a `backlog` or `ready` task to `running`. Any other
    /// status is an error, so a second claim of the same task fails.
    pub fn claim_task(&mut self, id: TaskId) -> Result<Task, StoreError> {
        let changed = self
            .conn
            .execute(
                "UPDATE tasks SET status = 'running' \
                 WHERE id = ?1 AND status IN ('backlog', 'ready')",
                params![id.get()],
            )
            .map_err(|source| StoreError::ClaimTask { id, source })?;
        if changed == 1 {
            return self.get_task(id);
        }
        let current = self.get_task(id)?;
        Err(StoreError::TaskNotClaimable {
            id,
            status: current.status,
        })
    }

    /// Record the start of an attempt at `task_id` on `profile`.
    pub fn start_run(&mut self, task_id: TaskId, profile: &str) -> Result<RunId, StoreError> {
        let started_at = now_rfc3339()?;
        self.conn
            .execute(
                "INSERT INTO runs (task_id, profile, started_at, status) \
                 VALUES (?1, ?2, ?3, 'running')",
                params![task_id.get(), profile, started_at],
            )
            .map_err(|source| StoreError::InsertRun { task_id, source })?;
        Ok(RunId::new(self.conn.last_insert_rowid()))
    }

    /// Record how a run ended. `RunStatus::Running` is not a valid final state.
    pub fn finish_run(&mut self, id: RunId, finish: RunFinish) -> Result<(), StoreError> {
        if finish.status == RunStatus::Running {
            return Err(StoreError::InvalidRunFinish { id });
        }
        let finished_at = now_rfc3339()?;
        let transcript = finish
            .transcript_path
            .as_deref()
            .map(|path| path.to_string_lossy().into_owned());
        let changed = self
            .conn
            .execute(
                "UPDATE runs SET status = ?1, finished_at = ?2, input_tokens = ?3, \
                 output_tokens = ?4, failure = ?5, transcript_path = ?6, branch = ?7 \
                 WHERE id = ?8",
                params![
                    finish.status.as_str(),
                    finished_at,
                    // Token counts stored as signed SQLite integers; a count
                    // above i64::MAX cannot come from a real model response.
                    i64::try_from(finish.usage.input_tokens).unwrap_or(i64::MAX),
                    i64::try_from(finish.usage.output_tokens).unwrap_or(i64::MAX),
                    finish.failure,
                    transcript,
                    finish.branch,
                    id.get(),
                ],
            )
            .map_err(|source| StoreError::UpdateRun { id, source })?;
        if changed == 0 {
            return Err(StoreError::UnknownRun { id });
        }
        Ok(())
    }

    /// All runs of a task, oldest first.
    pub fn list_runs(&self, task_id: TaskId) -> Result<Vec<Run>, StoreError> {
        let list_err = |source| StoreError::ListRuns { task_id, source };
        let mut stmt = self
            .conn
            .prepare(&format!("{SELECT_RUN} WHERE task_id = ?1 ORDER BY id ASC"))
            .map_err(list_err)?;
        let runs = stmt
            .query_map(params![task_id.get()], run_from_row)
            .and_then(|rows| rows.collect::<rusqlite::Result<Vec<Run>>>())
            .map_err(list_err)?;
        Ok(runs)
    }

    /// The stored value of `key`, or `None` if it was never set.
    pub fn get_setting(&self, key: &str) -> Result<Option<String>, StoreError> {
        self.conn
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(|source| StoreError::GetSetting {
                key: key.to_owned(),
                source,
            })
    }

    /// Store `value` under `key`, replacing any previous value.
    pub fn set_setting(&mut self, key: &str, value: &str) -> Result<(), StoreError> {
        self.conn
            .execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2) \
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map_err(|source| StoreError::SetSetting {
                key: key.to_owned(),
                source,
            })?;
        Ok(())
    }

    /// Whether `credits_low` mode is on. Off when never set.
    pub fn credits_low(&self) -> Result<bool, StoreError> {
        Ok(self.get_setting(CREDITS_LOW_KEY)?.as_deref() == Some("true"))
    }

    pub fn set_credits_low(&mut self, on: bool) -> Result<(), StoreError> {
        self.set_setting(CREDITS_LOW_KEY, if on { "true" } else { "false" })
    }
}

fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL PRIMARY KEY);",
    )
    .map_err(|source| StoreError::Migrate { version: 0, source })?;
    let applied: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |row| row.get(0),
        )
        .map_err(|source| StoreError::Migrate { version: 0, source })?;

    for &(version, sql) in MIGRATIONS {
        if version <= applied {
            continue;
        }
        let tx = conn
            .transaction()
            .map_err(|source| StoreError::Migrate { version, source })?;
        tx.execute_batch(sql)
            .map_err(|source| StoreError::Migrate { version, source })?;
        tx.execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            params![version],
        )
        .map_err(|source| StoreError::Migrate { version, source })?;
        tx.commit()
            .map_err(|source| StoreError::Migrate { version, source })?;
    }
    Ok(())
}

fn task_from_row(row: &Row<'_>) -> rusqlite::Result<Task> {
    let status_text: String = row.get(3)?;
    let status = status_text
        .parse::<TaskStatus>()
        .map_err(|err| rusqlite::Error::FromSqlConversionFailure(3, Type::Text, Box::new(err)))?;
    Ok(Task {
        id: TaskId::new(row.get(0)?),
        title: row.get(1)?,
        body: row.get(2)?,
        status,
        priority: row.get(4)?,
        profile: row.get(5)?,
        round: row.get(6)?,
        depends_on: Vec::new(),
        created_at: row.get(7)?,
    })
}

fn run_from_row(row: &Row<'_>) -> rusqlite::Result<Run> {
    let status_text: String = row.get(3)?;
    let status = RunStatus::from_db_text(&status_text).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            3,
            Type::Text,
            format!("unknown run status {status_text:?}").into(),
        )
    })?;
    let token_count = |idx: usize| -> rusqlite::Result<u64> {
        let raw: i64 = row.get(idx)?;
        u64::try_from(raw).map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(idx, Type::Integer, Box::new(err))
        })
    };
    let transcript: Option<String> = row.get(9)?;
    Ok(Run {
        id: RunId::new(row.get(0)?),
        task_id: TaskId::new(row.get(1)?),
        profile: row.get(2)?,
        status,
        started_at: row.get(4)?,
        finished_at: row.get(5)?,
        usage: Usage {
            input_tokens: token_count(6)?,
            output_tokens: token_count(7)?,
        },
        failure: row.get(8)?,
        transcript_path: transcript.map(PathBuf::from),
        branch: row.get(10)?,
    })
}

fn load_deps(conn: &Connection, id: TaskId) -> rusqlite::Result<Vec<TaskId>> {
    let mut stmt =
        conn.prepare("SELECT depends_on FROM task_deps WHERE task_id = ?1 ORDER BY depends_on")?;
    let rows = stmt.query_map(params![id.get()], |row| {
        row.get::<_, i64>(0).map(TaskId::new)
    })?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use std::path::PathBuf;

    use overseer_core::{NewTask, RunFinish, RunId, RunStatus, TaskId, TaskStatus, Usage};

    use super::Store;
    use crate::StoreError;

    fn new_task(title: &str, priority: i32) -> NewTask {
        NewTask {
            title: title.to_owned(),
            priority,
            ..NewTask::default()
        }
    }

    fn memory_store() -> Store {
        Store::open_in_memory().unwrap()
    }

    #[test]
    fn add_task_assigns_id_and_defaults() {
        let mut store = memory_store();
        let task = store
            .add_task(NewTask {
                title: "write parser".to_owned(),
                body: "handle quotes".to_owned(),
                priority: 3,
                profile: Some("local".to_owned()),
                depends_on: Vec::new(),
                ready: false,
            })
            .unwrap();

        assert!(task.id.get() > 0);
        assert_eq!(task.title, "write parser");
        assert_eq!(task.body, "handle quotes");
        assert_eq!(task.status, TaskStatus::Backlog);
        assert_eq!(task.priority, 3);
        assert_eq!(task.profile.as_deref(), Some("local"));
        assert_eq!(task.round, 0);
        assert!(task.depends_on.is_empty());
        assert!(task.created_at.ends_with('Z'), "{}", task.created_at);
    }

    #[test]
    fn get_task_returns_the_added_task() {
        let mut store = memory_store();
        let added = store.add_task(new_task("first", 0)).unwrap();
        let fetched = store.get_task(added.id).unwrap();
        assert_eq!(fetched, added);
    }

    #[test]
    fn get_unknown_id_is_an_error() {
        let store = memory_store();
        let err = store.get_task(TaskId::new(999)).unwrap_err();
        assert!(
            matches!(err, StoreError::UnknownTask { id } if id == TaskId::new(999)),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn list_orders_by_priority_then_id() {
        let mut store = memory_store();
        let a = store.add_task(new_task("a", 5)).unwrap();
        let b = store.add_task(new_task("b", 1)).unwrap();
        let c = store.add_task(new_task("c", 1)).unwrap();

        let ids: Vec<TaskId> = store
            .list_tasks(None)
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(ids, vec![b.id, c.id, a.id]);
    }

    #[test]
    fn list_filters_by_status() {
        let mut store = memory_store();
        let a = store.add_task(new_task("a", 0)).unwrap();
        let b = store.add_task(new_task("b", 1)).unwrap();
        store.set_status(b.id, TaskStatus::Done).unwrap();

        let done: Vec<TaskId> = store
            .list_tasks(Some(TaskStatus::Done))
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(done, vec![b.id]);

        let backlog: Vec<TaskId> = store
            .list_tasks(Some(TaskStatus::Backlog))
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(backlog, vec![a.id]);

        assert_eq!(store.list_tasks(None).unwrap().len(), 2);
    }

    #[test]
    fn set_status_updates_the_row() {
        let mut store = memory_store();
        let task = store.add_task(new_task("a", 0)).unwrap();
        store.set_status(task.id, TaskStatus::NeedsHuman).unwrap();
        assert_eq!(
            store.get_task(task.id).unwrap().status,
            TaskStatus::NeedsHuman
        );
    }

    #[test]
    fn set_status_on_unknown_id_is_an_error() {
        let mut store = memory_store();
        let err = store
            .set_status(TaskId::new(42), TaskStatus::Ready)
            .unwrap_err();
        assert!(
            matches!(err, StoreError::UnknownTask { id } if id == TaskId::new(42)),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn dependencies_are_stored_and_returned() {
        let mut store = memory_store();
        let first = store.add_task(new_task("first", 0)).unwrap();
        let second = store.add_task(new_task("second", 0)).unwrap();
        let third = store
            .add_task(NewTask {
                depends_on: vec![second.id, first.id],
                ..new_task("third", 0)
            })
            .unwrap();

        let fetched = store.get_task(third.id).unwrap();
        assert_eq!(fetched.depends_on, vec![first.id, second.id]);

        let listed = store.list_tasks(None).unwrap();
        let listed_third = listed.iter().find(|t| t.id == third.id).unwrap();
        assert_eq!(listed_third.depends_on, vec![first.id, second.id]);
    }

    #[test]
    fn unknown_dependency_is_rejected_and_nothing_is_inserted() {
        let mut store = memory_store();
        let err = store
            .add_task(NewTask {
                depends_on: vec![TaskId::new(999)],
                ..new_task("orphan", 0)
            })
            .unwrap_err();
        assert!(
            matches!(err, StoreError::UnknownDependency { depends_on } if depends_on == TaskId::new(999)),
            "unexpected error: {err:?}"
        );
        assert!(store.list_tasks(None).unwrap().is_empty());
    }

    #[test]
    fn tasks_persist_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overseer.db");

        let added = {
            let mut store = Store::open(&path).unwrap();
            store.add_task(new_task("kept", 2)).unwrap()
        };

        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.get_task(added.id).unwrap(), added);
    }

    fn finished(status: RunStatus) -> RunFinish {
        RunFinish {
            status,
            usage: Usage {
                input_tokens: 120,
                output_tokens: 45,
            },
            failure: None,
            transcript_path: Some(PathBuf::from("/state/runs/1.md")),
            branch: Some("overseer/task-1".to_owned()),
        }
    }

    #[test]
    fn claim_moves_backlog_task_to_running_once() {
        let mut store = memory_store();
        let task = store.add_task(new_task("a", 0)).unwrap();

        let claimed = store.claim_task(task.id).unwrap();
        assert_eq!(claimed.status, TaskStatus::Running);

        let err = store.claim_task(task.id).unwrap_err();
        assert!(
            matches!(err, StoreError::TaskNotClaimable { id, status: TaskStatus::Running } if id == task.id),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn claim_accepts_ready_and_rejects_done() {
        let mut store = memory_store();
        let ready = store.add_task(new_task("ready", 0)).unwrap();
        store.set_status(ready.id, TaskStatus::Ready).unwrap();
        assert_eq!(
            store.claim_task(ready.id).unwrap().status,
            TaskStatus::Running
        );

        let done = store.add_task(new_task("done", 0)).unwrap();
        store.set_status(done.id, TaskStatus::Done).unwrap();
        let err = store.claim_task(done.id).unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::TaskNotClaimable {
                    status: TaskStatus::Done,
                    ..
                }
            ),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn ready_task_accepts_backlog_and_blocked_and_rejects_others() {
        let mut store = memory_store();
        // Backlog -> Ready
        let t1 = store.add_task(new_task("t1", 0)).unwrap();
        assert_eq!(t1.status, TaskStatus::Backlog);
        let readied = store.ready_task(t1.id).unwrap();
        assert_eq!(readied.status, TaskStatus::Ready);

        // Already Ready -> Error
        let err = store.ready_task(t1.id).unwrap_err();
        assert!(matches!(
            err,
            StoreError::TaskNotReadyable {
                status: TaskStatus::Ready,
                ..
            }
        ));

        // Blocked -> Ready
        store.set_status(t1.id, TaskStatus::Blocked).unwrap();
        let readied = store.ready_task(t1.id).unwrap();
        assert_eq!(readied.status, TaskStatus::Ready);

        // Running -> Error
        store.set_status(t1.id, TaskStatus::Running).unwrap();
        let err = store.ready_task(t1.id).unwrap_err();
        assert!(matches!(
            err,
            StoreError::TaskNotReadyable {
                status: TaskStatus::Running,
                ..
            }
        ));

        // Done -> Error
        store.set_status(t1.id, TaskStatus::Done).unwrap();
        let err = store.ready_task(t1.id).unwrap_err();
        assert!(matches!(
            err,
            StoreError::TaskNotReadyable {
                status: TaskStatus::Done,
                ..
            }
        ));
    }

    #[test]
    fn add_task_with_ready_true_sets_status_ready() {
        let mut store = memory_store();
        let task = store
            .add_task(NewTask {
                ready: true,
                ..new_task("ready task", 0)
            })
            .unwrap();
        assert_eq!(task.status, TaskStatus::Ready);
    }

    #[test]
    fn claim_unknown_task_is_an_error() {
        let mut store = memory_store();
        let err = store.claim_task(TaskId::new(7)).unwrap_err();
        assert!(
            matches!(err, StoreError::UnknownTask { .. }),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn start_run_records_a_running_row() {
        let mut store = memory_store();
        let task = store.add_task(new_task("a", 0)).unwrap();
        let run = store.start_run(task.id, "local").unwrap();

        let runs = store.list_runs(task.id).unwrap();
        assert_eq!(runs.len(), 1);
        let row = &runs[0];
        assert_eq!(row.id, run);
        assert_eq!(row.task_id, task.id);
        assert_eq!(row.profile, "local");
        assert_eq!(row.status, RunStatus::Running);
        assert!(row.started_at.ends_with('Z'), "{}", row.started_at);
        assert_eq!(row.finished_at, None);
        assert_eq!(row.usage, Usage::default());
        assert_eq!(row.failure, None);
        assert_eq!(row.transcript_path, None);
        assert_eq!(row.branch, None);
    }

    #[test]
    fn finish_run_records_usage_transcript_and_branch() {
        let mut store = memory_store();
        let task = store.add_task(new_task("a", 0)).unwrap();
        let run = store.start_run(task.id, "local").unwrap();
        store
            .finish_run(run, finished(RunStatus::Succeeded))
            .unwrap();

        let row = store.list_runs(task.id).unwrap().remove(0);
        assert_eq!(row.status, RunStatus::Succeeded);
        assert!(row.finished_at.is_some());
        assert_eq!(
            row.usage,
            Usage {
                input_tokens: 120,
                output_tokens: 45
            }
        );
        assert_eq!(row.transcript_path, Some(PathBuf::from("/state/runs/1.md")));
        assert_eq!(row.branch.as_deref(), Some("overseer/task-1"));
    }

    #[test]
    fn finish_run_records_failure_text() {
        let mut store = memory_store();
        let task = store.add_task(new_task("a", 0)).unwrap();
        let run = store.start_run(task.id, "local").unwrap();
        let failure = RunFinish {
            failure: Some("unreachable".to_owned()),
            ..finished(RunStatus::Failed)
        };
        store.finish_run(run, failure).unwrap();

        let row = store.list_runs(task.id).unwrap().remove(0);
        assert_eq!(row.status, RunStatus::Failed);
        assert_eq!(row.failure.as_deref(), Some("unreachable"));
    }

    #[test]
    fn finishing_as_running_or_unknown_run_is_rejected() {
        let mut store = memory_store();
        let task = store.add_task(new_task("a", 0)).unwrap();
        let run = store.start_run(task.id, "local").unwrap();

        let err = store
            .finish_run(run, finished(RunStatus::Running))
            .unwrap_err();
        assert!(
            matches!(err, StoreError::InvalidRunFinish { id } if id == run),
            "unexpected error: {err:?}"
        );

        let err = store
            .finish_run(RunId::new(999), finished(RunStatus::Failed))
            .unwrap_err();
        assert!(
            matches!(err, StoreError::UnknownRun { id } if id == RunId::new(999)),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn list_runs_returns_only_that_tasks_runs_oldest_first() {
        let mut store = memory_store();
        let first = store.add_task(new_task("first", 0)).unwrap();
        let second = store.add_task(new_task("second", 0)).unwrap();
        let r1 = store.start_run(first.id, "local").unwrap();
        store.start_run(second.id, "local").unwrap();
        let r2 = store.start_run(first.id, "agy").unwrap();

        let ids: Vec<RunId> = store
            .list_runs(first.id)
            .unwrap()
            .into_iter()
            .map(|run| run.id)
            .collect();
        assert_eq!(ids, vec![r1, r2]);
        assert!(store.list_runs(TaskId::new(999)).unwrap().is_empty());
    }

    #[test]
    fn runs_persist_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overseer.db");
        let task_id = {
            let mut store = Store::open(&path).unwrap();
            let task = store.add_task(new_task("kept", 0)).unwrap();
            let run = store.start_run(task.id, "local").unwrap();
            store
                .finish_run(run, finished(RunStatus::Succeeded))
                .unwrap();
            task.id
        };

        let reopened = Store::open(&path).unwrap();
        let runs = reopened.list_runs(task_id).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, RunStatus::Succeeded);
    }

    #[test]
    fn unset_setting_reads_as_none() {
        let store = memory_store();
        assert_eq!(store.get_setting("missing").unwrap(), None);
    }

    #[test]
    fn set_setting_overwrites_the_previous_value() {
        let mut store = memory_store();
        store.set_setting("theme", "dark").unwrap();
        store.set_setting("theme", "light").unwrap();
        assert_eq!(
            store.get_setting("theme").unwrap().as_deref(),
            Some("light")
        );
    }

    #[test]
    fn credits_low_is_off_until_set() {
        let mut store = memory_store();
        assert!(!store.credits_low().unwrap());
        store.set_credits_low(true).unwrap();
        assert!(store.credits_low().unwrap());
        store.set_credits_low(false).unwrap();
        assert!(!store.credits_low().unwrap());
    }

    #[test]
    fn settings_persist_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overseer.db");
        {
            let mut store = Store::open(&path).unwrap();
            store.set_credits_low(true).unwrap();
        }
        let reopened = Store::open(&path).unwrap();
        assert!(reopened.credits_low().unwrap());
    }

    #[test]
    fn pick_next_ready_task_none_eligible() {
        let mut store = memory_store();
        // Empty store
        assert_eq!(store.pick_next_ready_task().unwrap(), None);

        // Only backlog tasks
        store.add_task(new_task("backlog task", 0)).unwrap();
        assert_eq!(store.pick_next_ready_task().unwrap(), None);

        // Task with unmet dep
        let dep = store.add_task(new_task("dep", 0)).unwrap();
        let task = NewTask {
            depends_on: vec![dep.id],
            ..new_task("ready with unmet dep", 0)
        };
        let t = store.add_task(task).unwrap();
        store.set_status(t.id, TaskStatus::Ready).unwrap();
        assert_eq!(store.pick_next_ready_task().unwrap(), None);
    }

    #[test]
    fn pick_next_ready_task_deps_done() {
        let mut store = memory_store();
        let dep = store.add_task(new_task("dep", 0)).unwrap();
        store.set_status(dep.id, TaskStatus::Done).unwrap();

        let task = store
            .add_task(NewTask {
                depends_on: vec![dep.id],
                ..new_task("ready task", 0)
            })
            .unwrap();
        store.set_status(task.id, TaskStatus::Ready).unwrap();

        let picked = store.pick_next_ready_task().unwrap();
        assert!(picked.is_some());
        let picked = picked.unwrap();
        assert_eq!(picked.id, task.id);
        assert_eq!(picked.depends_on, vec![dep.id]);
    }

    #[test]
    fn pick_next_ready_task_deps_not_done() {
        let mut store = memory_store();
        let dep = store.add_task(new_task("dep", 0)).unwrap();
        // dep is still Backlog
        let task = store
            .add_task(NewTask {
                depends_on: vec![dep.id],
                ..new_task("ready task", 0)
            })
            .unwrap();
        store.set_status(task.id, TaskStatus::Ready).unwrap();

        assert_eq!(store.pick_next_ready_task().unwrap(), None);

        // Set dep to Running: still not done
        store.set_status(dep.id, TaskStatus::Running).unwrap();
        assert_eq!(store.pick_next_ready_task().unwrap(), None);

        // Set dep to Review: still not done
        store.set_status(dep.id, TaskStatus::Review).unwrap();
        assert_eq!(store.pick_next_ready_task().unwrap(), None);

        // Set dep to Blocked: still not done
        store.set_status(dep.id, TaskStatus::Blocked).unwrap();
        assert_eq!(store.pick_next_ready_task().unwrap(), None);

        // Set dep to Done: now eligible!
        store.set_status(dep.id, TaskStatus::Done).unwrap();
        let picked = store.pick_next_ready_task().unwrap();
        assert_eq!(picked.map(|t| t.id), Some(task.id));
    }

    #[test]
    fn pick_next_ready_task_cycle_free_chain() {
        let mut store = memory_store();
        let t1 = store.add_task(new_task("step 1", 0)).unwrap();
        let t2 = store
            .add_task(NewTask {
                depends_on: vec![t1.id],
                ..new_task("step 2", 0)
            })
            .unwrap();
        let t3 = store
            .add_task(NewTask {
                depends_on: vec![t2.id],
                ..new_task("step 3", 0)
            })
            .unwrap();

        // Mark all three Ready
        store.set_status(t1.id, TaskStatus::Ready).unwrap();
        store.set_status(t2.id, TaskStatus::Ready).unwrap();
        store.set_status(t3.id, TaskStatus::Ready).unwrap();

        // Initially only t1 has all deps done (0 deps)
        let picked1 = store.pick_next_ready_task().unwrap();
        assert_eq!(picked1.map(|t| t.id), Some(t1.id));

        // When t1 is Done, t2 becomes eligible
        store.set_status(t1.id, TaskStatus::Done).unwrap();
        let picked2 = store.pick_next_ready_task().unwrap();
        assert_eq!(picked2.map(|t| t.id), Some(t2.id));

        // When t2 is Done, t3 becomes eligible
        store.set_status(t2.id, TaskStatus::Done).unwrap();
        let picked3 = store.pick_next_ready_task().unwrap();
        assert_eq!(picked3.map(|t| t.id), Some(t3.id));

        // When t3 is Done, none eligible
        store.set_status(t3.id, TaskStatus::Done).unwrap();
        assert_eq!(store.pick_next_ready_task().unwrap(), None);
    }

    #[test]
    fn pick_next_ready_task_priority_ordering() {
        let mut store = memory_store();
        let t_p10 = store.add_task(new_task("low urgency", 10)).unwrap();
        let t_p5_a = store.add_task(new_task("medium urgency a", 5)).unwrap();
        let t_p5_b = store.add_task(new_task("medium urgency b", 5)).unwrap();
        let unmet = store.add_task(new_task("unmet dep", 0)).unwrap();
        let t_p1_blocked = store
            .add_task(NewTask {
                depends_on: vec![unmet.id],
                ..new_task("urgent but blocked by dep", 1)
            })
            .unwrap();

        store.set_status(t_p10.id, TaskStatus::Ready).unwrap();
        store.set_status(t_p5_a.id, TaskStatus::Ready).unwrap();
        store.set_status(t_p5_b.id, TaskStatus::Ready).unwrap();
        store
            .set_status(t_p1_blocked.id, TaskStatus::Ready)
            .unwrap();

        // Lowest priority value first (5 < 10); among tie (t_p5_a vs t_p5_b), lowest id first
        let picked = store.pick_next_ready_task().unwrap();
        assert_eq!(picked.map(|t| t.id), Some(t_p5_a.id));

        store.set_status(t_p5_a.id, TaskStatus::Done).unwrap();
        let picked = store.pick_next_ready_task().unwrap();
        assert_eq!(picked.map(|t| t.id), Some(t_p5_b.id));

        store.set_status(t_p5_b.id, TaskStatus::Done).unwrap();
        let picked = store.pick_next_ready_task().unwrap();
        assert_eq!(picked.map(|t| t.id), Some(t_p10.id));

        store.set_status(t_p10.id, TaskStatus::Done).unwrap();
        // t_p1_blocked still has unmet dep
        assert_eq!(store.pick_next_ready_task().unwrap(), None);
    }
}
