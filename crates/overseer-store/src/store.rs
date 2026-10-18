use std::path::Path;

use overseer_core::{NewTask, Task, TaskId, TaskStatus};
use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row, params};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::StoreError;

const MIGRATIONS: &[(i64, &str)] = &[(1, include_str!("../migrations/0001_init.sql"))];

const SELECT_TASK: &str =
    "SELECT id, title, body, status, priority, profile, round, created_at FROM tasks";

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
        } = new;
        depends_on.sort_unstable();
        depends_on.dedup();
        let created_at = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .map_err(|source| StoreError::FormatTimestamp { source })?;
        let status = TaskStatus::Backlog;

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
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use overseer_core::{NewTask, TaskId, TaskStatus};

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
}
