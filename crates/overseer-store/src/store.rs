use std::path::{Path, PathBuf};

use overseer_core::{
    Lesson, LessonId, LessonStatus, NewLesson, NewReview, NewTask, ReviewRecord, Run, RunFinish,
    RunId, RunStatus, Task, TaskId, TaskStatus, Usage, Verdict,
};
use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row, params};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::StoreError;

const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("../migrations/0001_init.sql")),
    (2, include_str!("../migrations/0002_runs.sql")),
    (3, include_str!("../migrations/0003_settings.sql")),
    (4, include_str!("../migrations/0004_reviews.sql")),
    (5, include_str!("../migrations/0005_lessons.sql")),
    (6, include_str!("../migrations/0006_reason.sql")),
    (7, include_str!("../migrations/0007_source_external_id.sql")),
];

/// Settings key holding the `credits_low` mode flag, stored as `"true"` or
/// `"false"`.
const CREDITS_LOW_KEY: &str = "credits_low";

const SELECT_TASK: &str = "SELECT id, title, body, status, priority, profile, round, approved, created_at, reason, source, external_id FROM tasks";

const SELECT_RUN: &str = "SELECT id, task_id, profile, status, started_at, finished_at, \
     input_tokens, output_tokens, failure, transcript_path, branch, skills FROM runs";

const SELECT_LESSON: &str =
    "SELECT id, task_id, skill, base_version, status, path, created_at FROM lessons";

fn now_rfc3339() -> Result<String, StoreError> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|source| StoreError::FormatTimestamp { source })
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens the database at `path` and runs migrations.
    /// Returns a `StoreError` on failure.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let conn = Connection::open(path).map_err(|source| StoreError::OpenDatabase {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_connection(conn)
    }

    /// Opens an in-memory database and runs migrations.
    /// Returns a `StoreError` on failure.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn =
            Connection::open_in_memory().map_err(|source| StoreError::OpenInMemory { source })?;
        Self::from_connection(conn)
    }

    fn from_connection(mut conn: Connection) -> Result<Self, StoreError> {
        migrate(&mut conn)?;
        Ok(Self { conn })
    }

    /// Adds a new task to the store.
    pub fn add_task(&mut self, new: NewTask) -> Result<Task, StoreError> {
        let NewTask {
            title,
            body,
            priority,
            profile,
            mut depends_on,
            ready,
            source,
            external_id,
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
            "INSERT INTO tasks (title, body, status, priority, profile, round, created_at, source, external_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7, ?8)",
            params![
                title,
                body,
                status.as_str(),
                priority,
                profile,
                created_at,
                source,
                external_id,
            ],
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
            approved: false,
            depends_on,
            created_at,
            reason: None,
            source,
            external_id,
        })
    }

    /// Get a task by its id.
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

    /// List all tasks, optionally filtered by status.
    ///
    /// Returns tasks ordered by priority ascending, then by id ascending.
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

    /// Find a task by its source and external id.
    pub fn find_task_by_external_id(
        &self,
        source: &str,
        external_id: &str,
    ) -> Result<Option<Task>, StoreError> {
        let err_fn = |err| StoreError::FindTaskByExternalId {
            task_source: source.into(),
            external_id: external_id.into(),
            source: Box::new(err),
        };
        let mut task = self
            .conn
            .query_row(
                &format!("{SELECT_TASK} WHERE source = ?1 AND external_id = ?2"),
                params![source, external_id],
                task_from_row,
            )
            .optional()
            .map_err(err_fn)?;
        if let Some(ref mut task) = task {
            task.depends_on = load_deps(&self.conn, task.id).map_err(err_fn)?;
        }
        Ok(task)
    }

    /// List all tasks from a given source, ordered by id ascending.
    pub fn list_tasks_by_source(&self, source: &str) -> Result<Vec<Task>, StoreError> {
        let list_err = |err| StoreError::ListTasksBySource {
            task_source: source.into(),
            source: Box::new(err),
        };

        let mut stmt = self
            .conn
            .prepare(&format!("{SELECT_TASK} WHERE source = ?1 ORDER BY id ASC"))
            .map_err(list_err)?;
        let mut tasks = stmt
            .query_map(params![source], task_from_row)
            .and_then(|rows| rows.collect::<rusqlite::Result<Vec<Task>>>())
            .map_err(list_err)?;

        for task in &mut tasks {
            task.depends_on = load_deps(&self.conn, task.id).map_err(list_err)?;
        }
        Ok(tasks)
    }

    /// Returns the counts of tasks grouped by status for all task statuses.
    pub fn task_counts_by_status(&self) -> Result<Vec<(TaskStatus, usize)>, StoreError> {
        let mut counts = std::collections::HashMap::new();
        let mut stmt = self
            .conn
            .prepare("SELECT status, COUNT(*) FROM tasks GROUP BY status")
            .map_err(|source| StoreError::TaskCounts { source })?;
        let rows = stmt
            .query_map([], |row| {
                let status_str: String = row.get(0)?;
                let count: i64 = row.get(1)?;
                Ok((status_str, count))
            })
            .map_err(|source| StoreError::TaskCounts { source })?;

        for row in rows {
            let (status_str, count) = row.map_err(|source| StoreError::TaskCounts { source })?;
            if let Ok(status) = status_str.parse::<TaskStatus>() {
                let count_usize = usize::try_from(count.max(0)).unwrap_or(0);
                let _ = counts.insert(status, count_usize);
            }
        }

        let all_statuses = TaskStatus::ALL;

        Ok(all_statuses
            .iter()
            .map(|&status| (status, counts.get(&status).copied().unwrap_or(0)))
            .collect())
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

    pub fn set_status_with_reason(
        &mut self,
        id: TaskId,
        status: TaskStatus,
        reason: Option<&str>,
    ) -> Result<(), StoreError> {
        let changed = self
            .conn
            .execute(
                "UPDATE tasks SET status = ?1, reason = ?2 WHERE id = ?3",
                params![status.as_str(), reason, id.get()],
            )
            .map_err(|source| StoreError::SetStatus { id, source })?;
        if changed == 0 {
            return Err(StoreError::UnknownTask { id });
        }
        Ok(())
    }

    /// Set the status of a task to the given status.
    pub fn set_status(&mut self, id: TaskId, status: TaskStatus) -> Result<(), StoreError> {
        self.set_status_with_reason(id, status, None)
    }

    /// Update the title and body of a task.
    pub fn set_task_title_and_body(
        &mut self,
        id: TaskId,
        title: &str,
        body: &str,
    ) -> Result<(), StoreError> {
        let changed = self
            .conn
            .execute(
                "UPDATE tasks SET title = ?1, body = ?2 WHERE id = ?3",
                params![title, body, id.get()],
            )
            .map_err(|source| StoreError::SetTaskTitleAndBody {
                id,
                source: Box::new(source),
            })?;
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
    pub fn start_run(
        &mut self,
        task_id: TaskId,
        profile: &str,
        skills: Option<&str>,
    ) -> Result<RunId, StoreError> {
        let started_at = now_rfc3339()?;
        self.conn
            .execute(
                "INSERT INTO runs (task_id, profile, started_at, status, skills) \
                 VALUES (?1, ?2, ?3, 'running', ?4)",
                params![task_id.get(), profile, started_at, skills],
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

    /// Return the newest runs up to `limit`, newest first.
    pub fn list_recent_runs(&self, limit: usize) -> Result<Vec<Run>, StoreError> {
        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut stmt = self
            .conn
            .prepare(&format!("{SELECT_RUN} ORDER BY id DESC LIMIT ?1"))
            .map_err(|source| StoreError::ListRecentRuns { source })?;
        let runs = stmt
            .query_map(params![limit_i64], run_from_row)
            .and_then(|rows| rows.collect::<rusqlite::Result<Vec<Run>>>())
            .map_err(|source| StoreError::ListRecentRuns { source })?;
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

    /// Record a review for a task.
    pub fn record_review(&mut self, new: NewReview) -> Result<ReviewRecord, StoreError> {
        let NewReview {
            task_id,
            round,
            profile,
            verdict,
            path,
        } = new;
        let created_at = now_rfc3339()?;
        let path_str = path.to_string_lossy().into_owned();

        self.conn
            .execute(
                "INSERT INTO reviews (task_id, round, profile, verdict, path, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    task_id.get(),
                    round,
                    profile,
                    verdict.as_str(),
                    path_str,
                    created_at,
                ],
            )
            .map_err(|source| StoreError::InsertReview { task_id, source })?;

        let id = self.conn.last_insert_rowid();

        Ok(ReviewRecord {
            id,
            task_id,
            round,
            profile,
            verdict,
            path,
            created_at,
        })
    }

    /// All reviews of a task, oldest first.
    pub fn list_reviews(&self, task_id: TaskId) -> Result<Vec<ReviewRecord>, StoreError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, task_id, round, profile, verdict, path, created_at \
                 FROM reviews WHERE task_id = ?1 ORDER BY round ASC, id ASC",
            )
            .map_err(|source| StoreError::ListReviews { task_id, source })?;

        let rows = stmt
            .query_map(params![task_id.get()], review_from_row)
            .map_err(|source| StoreError::ListReviews { task_id, source })?;

        let mut reviews = Vec::new();
        for row in rows {
            let review = row.map_err(|source| StoreError::ListReviews { task_id, source })?;
            reviews.push(review);
        }
        Ok(reviews)
    }

    pub fn list_reviews_for_round(
        &self,
        task_id: TaskId,
        round: u8,
    ) -> Result<Vec<ReviewRecord>, StoreError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, task_id, round, profile, verdict, path, created_at \
                 FROM reviews WHERE task_id = ?1 AND round = ?2 ORDER BY id ASC",
            )
            .map_err(|source| StoreError::ListReviews { task_id, source })?;

        let rows = stmt
            .query_map(params![task_id.get(), round], review_from_row)
            .map_err(|source| StoreError::ListReviews { task_id, source })?;

        let mut reviews = Vec::new();
        for row in rows {
            let review = row.map_err(|source| StoreError::ListReviews { task_id, source })?;
            reviews.push(review);
        }
        Ok(reviews)
    }

    /// Set the approved status of a task.
    pub fn set_approved(&mut self, id: TaskId, approved: bool) -> Result<(), StoreError> {
        let changed = self
            .conn
            .execute(
                "UPDATE tasks SET approved = ?1 WHERE id = ?2",
                params![approved, id.get()],
            )
            .map_err(|source| StoreError::SetApproved {
                task_id: id,
                source,
            })?;
        if changed == 0 {
            return Err(StoreError::UnknownTask { id });
        }
        Ok(())
    }

    pub fn increment_round(&mut self, id: TaskId) -> Result<u8, StoreError> {
        let changed = self
            .conn
            .execute(
                "UPDATE tasks SET round = round + 1 WHERE id = ?1",
                params![id.get()],
            )
            .map_err(|source| StoreError::IncrementRound {
                task_id: id,
                source,
            })?;
        if changed == 0 {
            return Err(StoreError::UnknownTask { id });
        }
        let task = self.get_task(id)?;
        Ok(task.round)
    }

    pub fn add_lesson(&mut self, new: NewLesson) -> Result<Lesson, StoreError> {
        let created_at = now_rfc3339()?;
        let status = LessonStatus::Proposed;
        let path_str = new.path.to_string_lossy().into_owned();
        self.conn
            .execute(
                "INSERT INTO lessons (task_id, skill, base_version, status, path, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    new.task_id.get(),
                    new.skill,
                    new.base_version,
                    status.as_str(),
                    path_str,
                    created_at,
                ],
            )
            .map_err(|source| StoreError::InsertLesson {
                task_id: new.task_id,
                source,
            })?;
        let id = LessonId::new(self.conn.last_insert_rowid());
        Ok(Lesson {
            id,
            task_id: new.task_id,
            skill: new.skill,
            base_version: new.base_version,
            status,
            path: new.path,
            created_at,
        })
    }

    pub fn update_lesson_path(&mut self, id: LessonId, path: &Path) -> Result<(), StoreError> {
        let path_str = path.to_string_lossy().into_owned();
        self.conn
            .execute(
                "UPDATE lessons SET path = ?1 WHERE id = ?2",
                params![path_str, id.get()],
            )
            .map_err(|source| StoreError::UpdateLessonPath { id, source })?;
        Ok(())
    }

    pub fn get_lesson(&self, id: LessonId) -> Result<Lesson, StoreError> {
        self.conn
            .query_row(
                &format!("{SELECT_LESSON} WHERE id = ?1"),
                params![id.get()],
                lesson_from_row,
            )
            .optional()
            .map_err(|source| StoreError::LoadLesson { id, source })?
            .ok_or(StoreError::UnknownLesson { id })
    }

    pub fn list_lessons(&self, status: Option<LessonStatus>) -> Result<Vec<Lesson>, StoreError> {
        let list_err = |source| StoreError::ListLessons { source };
        let mut lessons = Vec::new();
        if let Some(status) = status {
            let mut stmt = self
                .conn
                .prepare(&format!(
                    "{SELECT_LESSON} WHERE status = ?1 ORDER BY id ASC"
                ))
                .map_err(list_err)?;
            let rows = stmt
                .query_map(params![status.as_str()], lesson_from_row)
                .map_err(list_err)?;
            for row in rows {
                lessons.push(row.map_err(list_err)?);
            }
        } else {
            let mut stmt = self
                .conn
                .prepare(&format!("{SELECT_LESSON} ORDER BY id ASC"))
                .map_err(list_err)?;
            let rows = stmt.query_map([], lesson_from_row).map_err(list_err)?;
            for row in rows {
                lessons.push(row.map_err(list_err)?);
            }
        }
        Ok(lessons)
    }

    pub fn set_lesson_status(
        &mut self,
        id: LessonId,
        status: LessonStatus,
    ) -> Result<(), StoreError> {
        let changed = self
            .conn
            .execute(
                "UPDATE lessons SET status = ?1 WHERE id = ?2",
                params![status.as_str(), id.get()],
            )
            .map_err(|source| StoreError::SetLessonStatus { id, source })?;
        if changed == 0 {
            return Err(StoreError::UnknownLesson { id });
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
    let approved: bool = row.get(7)?;
    let reason: Option<String> = row.get(9)?;
    let source: Option<String> = row.get(10)?;
    let external_id: Option<String> = row.get(11)?;
    Ok(Task {
        id: TaskId::new(row.get(0)?),
        title: row.get(1)?,
        body: row.get(2)?,
        status,
        priority: row.get(4)?,
        profile: row.get(5)?,
        round: row.get(6)?,
        approved,
        depends_on: Vec::new(),
        created_at: row.get(8)?,
        reason,
        source,
        external_id,
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
        skills: row.get(11)?,
    })
}

fn lesson_from_row(row: &Row<'_>) -> rusqlite::Result<Lesson> {
    let status_text: String = row.get(4)?;
    let status = LessonStatus::from_db_text(&status_text).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            4,
            Type::Text,
            format!("unknown lesson status {status_text:?}").into(),
        )
    })?;
    let path_str: String = row.get(5)?;
    let base_version_raw: i64 = row.get(3)?;
    let base_version = u32::try_from(base_version_raw).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(3, Type::Integer, Box::new(err))
    })?;
    Ok(Lesson {
        id: LessonId::new(row.get(0)?),
        task_id: TaskId::new(row.get(1)?),
        skill: row.get(2)?,
        base_version,
        status,
        path: PathBuf::from(path_str),
        created_at: row.get(6)?,
    })
}

fn review_from_row(row: &Row<'_>) -> rusqlite::Result<ReviewRecord> {
    let verdict_text: String = row.get(4)?;
    let verdict = verdict_text
        .parse::<Verdict>()
        .map_err(|err| rusqlite::Error::FromSqlConversionFailure(4, Type::Text, Box::new(err)))?;
    let path_text: String = row.get(5)?;
    Ok(ReviewRecord {
        id: row.get(0)?,
        task_id: TaskId::new(row.get(1)?),
        round: row.get(2)?,
        profile: row.get(3)?,
        verdict,
        path: PathBuf::from(path_text),
        created_at: row.get(6)?,
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

    use overseer_core::{
        LessonId, LessonStatus, NewLesson, NewReview, NewTask, RunFinish, RunId, RunStatus, TaskId,
        TaskStatus, Usage, Verdict,
    };

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
                source: None,
                external_id: None,
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
        assert_eq!(task.source, None);
        assert_eq!(task.external_id, None);
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
        let run = store.start_run(task.id, "local", None).unwrap();

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
        assert_eq!(row.skills, None);
    }

    #[test]
    fn finish_run_records_usage_transcript_and_branch() {
        let mut store = memory_store();
        let task = store.add_task(new_task("a", 0)).unwrap();
        let run = store.start_run(task.id, "local", None).unwrap();
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
        let run = store.start_run(task.id, "local", None).unwrap();
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
        let run = store.start_run(task.id, "local", None).unwrap();

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
        let r1 = store.start_run(first.id, "local", None).unwrap();
        store.start_run(second.id, "local", None).unwrap();
        let r2 = store.start_run(first.id, "agy", None).unwrap();

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
            let run = store.start_run(task.id, "local", None).unwrap();
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

    #[test]
    fn task_counts_by_status_reports_all_statuses() {
        let mut store = memory_store();
        let t1 = store.add_task(new_task("t1", 0)).unwrap();
        let _t2 = store.add_task(new_task("t2", 0)).unwrap();
        store.set_status(t1.id, TaskStatus::Ready).unwrap();

        let counts = store.task_counts_by_status().unwrap();
        let map: std::collections::HashMap<_, _> = counts.into_iter().collect();
        assert_eq!(map.get(&TaskStatus::Backlog), Some(&1));
        assert_eq!(map.get(&TaskStatus::Ready), Some(&1));
        assert_eq!(map.get(&TaskStatus::Running), Some(&0));
        assert_eq!(map.get(&TaskStatus::Review), Some(&0));
        assert_eq!(map.get(&TaskStatus::Blocked), Some(&0));
        assert_eq!(map.get(&TaskStatus::NeedsHuman), Some(&0));
        assert_eq!(map.get(&TaskStatus::Done), Some(&0));
        assert_eq!(map.get(&TaskStatus::Cancelled), Some(&0));
    }

    #[test]
    fn list_recent_runs_orders_newest_first_and_limits() {
        let mut store = memory_store();
        let t = store.add_task(new_task("t", 0)).unwrap();
        let r1 = store.start_run(t.id, "local", None).unwrap();
        let r2 = store.start_run(t.id, "local", None).unwrap();
        let r3 = store.start_run(t.id, "local", None).unwrap();

        let recent = store.list_recent_runs(2).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].id, r3);
        assert_eq!(recent[1].id, r2);

        let all = store.list_recent_runs(10).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].id, r3);
        assert_eq!(all[1].id, r2);
        assert_eq!(all[2].id, r1);
    }

    #[test]
    fn record_and_list_reviews() {
        let mut store = memory_store();
        let t = store.add_task(new_task("t", 0)).unwrap();

        let r1 = store
            .record_review(NewReview {
                task_id: t.id,
                round: 0,
                profile: "gemini".to_owned(),
                verdict: Verdict::Changes,
                path: PathBuf::from("/state/reviews/task-1/round-0/gemini.md"),
            })
            .unwrap();

        let r2 = store
            .record_review(NewReview {
                task_id: t.id,
                round: 0,
                profile: "local".to_owned(),
                verdict: Verdict::Pass,
                path: PathBuf::from("/state/reviews/task-1/round-0/local.md"),
            })
            .unwrap();

        let r3 = store
            .record_review(NewReview {
                task_id: t.id,
                round: 1,
                profile: "gemini".to_owned(),
                verdict: Verdict::Pass,
                path: PathBuf::from("/state/reviews/task-1/round-1/gemini.md"),
            })
            .unwrap();

        let reviews = store.list_reviews(t.id).unwrap();
        assert_eq!(reviews.len(), 3);
        assert_eq!(reviews[0].id, r1.id);
        assert_eq!(reviews[0].round, 0);
        assert_eq!(reviews[0].profile, "gemini");
        assert_eq!(reviews[0].verdict, Verdict::Changes);

        assert_eq!(reviews[1].id, r2.id);
        assert_eq!(reviews[1].round, 0);
        assert_eq!(reviews[1].profile, "local");
        assert_eq!(reviews[1].verdict, Verdict::Pass);

        assert_eq!(reviews[2].id, r3.id);
        assert_eq!(reviews[2].round, 1);
        assert_eq!(reviews[2].profile, "gemini");
        assert_eq!(reviews[2].verdict, Verdict::Pass);

        // Filter by round
        let round_0 = store.list_reviews_for_round(t.id, 0).unwrap();
        assert_eq!(round_0.len(), 2);
        assert_eq!(round_0[0].profile, "gemini");
        assert_eq!(round_0[1].profile, "local");

        let round_1 = store.list_reviews_for_round(t.id, 1).unwrap();
        assert_eq!(round_1.len(), 1);
        assert_eq!(round_1[0].profile, "gemini");

        let round_2 = store.list_reviews_for_round(t.id, 2).unwrap();
        assert!(round_2.is_empty());
    }

    #[test]
    fn set_approved_and_increment_round() {
        let mut store = memory_store();
        let t = store.add_task(new_task("t", 0)).unwrap();

        assert!(!t.approved);
        assert_eq!(t.round, 0);

        store.set_approved(t.id, true).unwrap();
        let loaded = store.get_task(t.id).unwrap();
        assert!(loaded.approved);

        let new_round = store.increment_round(t.id).unwrap();
        assert_eq!(new_round, 1);
        let loaded = store.get_task(t.id).unwrap();
        assert_eq!(loaded.round, 1);

        let new_round = store.increment_round(t.id).unwrap();
        assert_eq!(new_round, 2);
        let loaded = store.get_task(t.id).unwrap();
        assert_eq!(loaded.round, 2);

        // Unknown task returns error
        assert!(matches!(
            store.set_approved(TaskId::new(999), true).unwrap_err(),
            StoreError::UnknownTask { .. }
        ));
        assert!(matches!(
            store.increment_round(TaskId::new(999)).unwrap_err(),
            StoreError::UnknownTask { .. }
        ));
    }

    #[test]
    fn reviews_and_approval_persist_across_reopen() {
        let root = tempfile::tempdir().unwrap();
        let db_path = root.path().join("overseer.db");

        let task_id = {
            let mut store = Store::open(&db_path).unwrap();
            let t = store.add_task(new_task("persist test", 1)).unwrap();
            store.set_approved(t.id, true).unwrap();
            store.increment_round(t.id).unwrap();
            store
                .record_review(NewReview {
                    task_id: t.id,
                    round: 0,
                    profile: "local".to_owned(),
                    verdict: Verdict::Pass,
                    path: PathBuf::from("/path/to/review.md"),
                })
                .unwrap();
            t.id
        };

        let reopened = Store::open(&db_path).unwrap();
        let task = reopened.get_task(task_id).unwrap();
        assert!(task.approved);
        assert_eq!(task.round, 1);

        let reviews = reopened.list_reviews(task_id).unwrap();
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].profile, "local");
        assert_eq!(reviews[0].verdict, Verdict::Pass);
        assert_eq!(reviews[0].path, PathBuf::from("/path/to/review.md"));
    }

    #[test]
    fn start_run_records_skills_column() {
        let mut store = memory_store();
        let task = store.add_task(new_task("a", 0)).unwrap();
        let run = store
            .start_run(task.id, "local", Some("rust:v1,testing:v2"))
            .unwrap();
        let row = store.list_runs(task.id).unwrap().remove(0);
        assert_eq!(row.id, run);
        assert_eq!(row.skills.as_deref(), Some("rust:v1,testing:v2"));
    }

    #[test]
    fn add_lesson_assigns_id_and_sets_proposed() {
        let mut store = memory_store();
        let task = store.add_task(new_task("a", 0)).unwrap();
        let lesson = store
            .add_lesson(NewLesson {
                task_id: task.id,
                skill: "rust".to_owned(),
                base_version: 1,
                path: PathBuf::from("/state/lessons/1.md"),
            })
            .unwrap();

        assert_eq!(lesson.id, LessonId::new(1));
        assert_eq!(lesson.task_id, task.id);
        assert_eq!(lesson.skill, "rust");
        assert_eq!(lesson.base_version, 1);
        assert_eq!(lesson.status, LessonStatus::Proposed);
        assert_eq!(lesson.path, PathBuf::from("/state/lessons/1.md"));
        assert!(lesson.created_at.ends_with('Z'));

        store
            .update_lesson_path(lesson.id, &PathBuf::from("/state/lessons/final.md"))
            .unwrap();
        let loaded = store.get_lesson(lesson.id).unwrap();
        assert_eq!(loaded.path, PathBuf::from("/state/lessons/final.md"));
    }

    #[test]
    fn get_lesson_returns_stored_lesson() {
        let mut store = memory_store();
        let task = store.add_task(new_task("a", 0)).unwrap();
        let added = store
            .add_lesson(NewLesson {
                task_id: task.id,
                skill: "testing".to_owned(),
                base_version: 2,
                path: PathBuf::from("/state/lessons/2.md"),
            })
            .unwrap();

        let fetched = store.get_lesson(added.id).unwrap();
        assert_eq!(fetched, added);
    }

    #[test]
    fn get_unknown_lesson_is_an_error() {
        let store = memory_store();
        let err = store.get_lesson(LessonId::new(999)).unwrap_err();
        assert!(matches!(err, StoreError::UnknownLesson { id } if id == LessonId::new(999)));
    }

    #[test]
    fn list_lessons_filters_by_status() {
        let mut store = memory_store();
        let task = store.add_task(new_task("a", 0)).unwrap();
        let l1 = store
            .add_lesson(NewLesson {
                task_id: task.id,
                skill: "rust".to_owned(),
                base_version: 1,
                path: PathBuf::from("/state/lessons/1.md"),
            })
            .unwrap();
        let l2 = store
            .add_lesson(NewLesson {
                task_id: task.id,
                skill: "testing".to_owned(),
                base_version: 1,
                path: PathBuf::from("/state/lessons/2.md"),
            })
            .unwrap();

        store
            .set_lesson_status(l1.id, LessonStatus::Applied)
            .unwrap();

        let all = store.list_lessons(None).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, l1.id);
        assert_eq!(all[1].id, l2.id);

        let applied = store.list_lessons(Some(LessonStatus::Applied)).unwrap();
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].id, l1.id);

        let proposed = store.list_lessons(Some(LessonStatus::Proposed)).unwrap();
        assert_eq!(proposed.len(), 1);
        assert_eq!(proposed[0].id, l2.id);

        let rejected = store.list_lessons(Some(LessonStatus::Rejected)).unwrap();
        assert!(rejected.is_empty());
    }

    #[test]
    fn set_lesson_status_updates_row_and_rejects_unknown() {
        let mut store = memory_store();
        let task = store.add_task(new_task("a", 0)).unwrap();
        let l = store
            .add_lesson(NewLesson {
                task_id: task.id,
                skill: "rust".to_owned(),
                base_version: 1,
                path: PathBuf::from("/state/lessons/1.md"),
            })
            .unwrap();

        store
            .set_lesson_status(l.id, LessonStatus::Rejected)
            .unwrap();
        assert_eq!(
            store.get_lesson(l.id).unwrap().status,
            LessonStatus::Rejected
        );

        let err = store
            .set_lesson_status(LessonId::new(999), LessonStatus::Applied)
            .unwrap_err();
        assert!(matches!(err, StoreError::UnknownLesson { id } if id == LessonId::new(999)));
    }

    #[test]
    fn lessons_persist_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overseer.db");
        let lesson_id = {
            let mut store = Store::open(&path).unwrap();
            let task = store.add_task(new_task("kept", 0)).unwrap();
            let l = store
                .add_lesson(NewLesson {
                    task_id: task.id,
                    skill: "rust".to_owned(),
                    base_version: 3,
                    path: PathBuf::from("/state/lessons/1.md"),
                })
                .unwrap();
            l.id
        };

        let reopened = Store::open(&path).unwrap();
        let l = reopened.get_lesson(lesson_id).unwrap();
        assert_eq!(l.skill, "rust");
        assert_eq!(l.base_version, 3);
        assert_eq!(l.status, LessonStatus::Proposed);
    }

    #[test]
    fn set_status_with_reason_updates_status_and_reason() {
        let mut store = Store::open_in_memory().unwrap();
        let task = store.add_task(new_task("escalate me", 0)).unwrap();
        assert_eq!(task.reason, None);

        store
            .set_status_with_reason(
                task.id,
                TaskStatus::NeedsHuman,
                Some("escalation: Cargo.toml"),
            )
            .unwrap();

        let updated = store.get_task(task.id).unwrap();
        assert_eq!(updated.status, TaskStatus::NeedsHuman);
        assert_eq!(updated.reason.as_deref(), Some("escalation: Cargo.toml"));

        // set_status clears the reason
        store.set_status(task.id, TaskStatus::Review).unwrap();
        let cleared = store.get_task(task.id).unwrap();
        assert_eq!(cleared.status, TaskStatus::Review);
        assert_eq!(cleared.reason, None);
    }

    #[test]
    fn reason_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overseer.db");
        let task_id = {
            let mut store = Store::open(&path).unwrap();
            let task = store.add_task(new_task("persisted reason", 0)).unwrap();
            store
                .set_status_with_reason(
                    task.id,
                    TaskStatus::NeedsHuman,
                    Some("escalation: .github/workflows/ci.yml"),
                )
                .unwrap();
            task.id
        };

        let reopened = Store::open(&path).unwrap();
        let task = reopened.get_task(task_id).unwrap();
        assert_eq!(task.status, TaskStatus::NeedsHuman);
        assert_eq!(
            task.reason.as_deref(),
            Some("escalation: .github/workflows/ci.yml")
        );
    }

    #[test]
    fn find_task_by_external_id_finds_task() {
        let mut store = memory_store();
        let added = store
            .add_task(NewTask {
                source: Some("specs/auth.md".to_owned()),
                external_id: Some("task-42".to_owned()),
                ..new_task("auth task", 0)
            })
            .unwrap();

        let found = store
            .find_task_by_external_id("specs/auth.md", "task-42")
            .unwrap();
        assert_eq!(found, Some(added));

        let not_found = store
            .find_task_by_external_id("specs/auth.md", "missing")
            .unwrap();
        assert_eq!(not_found, None);

        let wrong_source = store
            .find_task_by_external_id("specs/other.md", "task-42")
            .unwrap();
        assert_eq!(wrong_source, None);
    }

    #[test]
    fn add_task_with_duplicate_source_and_external_id_fails() {
        let mut store = memory_store();
        store
            .add_task(NewTask {
                source: Some("specs/feature.md".to_owned()),
                external_id: Some("id-1".to_owned()),
                ..new_task("first", 0)
            })
            .unwrap();

        let err = store
            .add_task(NewTask {
                source: Some("specs/feature.md".to_owned()),
                external_id: Some("id-1".to_owned()),
                ..new_task("duplicate", 0)
            })
            .unwrap_err();

        assert!(
            matches!(err, StoreError::InsertTask { .. }),
            "expected InsertTask error, got: {err:?}"
        );
    }

    #[test]
    fn add_task_null_external_ids_can_coexist() {
        let mut store = memory_store();
        let t1 = store
            .add_task(NewTask {
                source: Some("specs/feature.md".to_owned()),
                ..new_task("first null ext id", 0)
            })
            .unwrap();

        let t2 = store
            .add_task(NewTask {
                source: Some("specs/feature.md".to_owned()),
                ..new_task("second null ext id", 0)
            })
            .unwrap();

        let t3 = store.add_task(new_task("both null", 0)).unwrap();

        assert_ne!(t1.id, t2.id);
        assert_ne!(t2.id, t3.id);
    }

    #[test]
    fn list_tasks_by_source_returns_only_matching_tasks() {
        let mut store = memory_store();
        let t1 = store
            .add_task(NewTask {
                source: Some("specs/a.md".to_owned()),
                external_id: Some("1".to_owned()),
                ..new_task("a1", 10)
            })
            .unwrap();
        let _t_other = store
            .add_task(NewTask {
                source: Some("specs/b.md".to_owned()),
                external_id: Some("1".to_owned()),
                ..new_task("b1", 0)
            })
            .unwrap();
        let t2 = store
            .add_task(NewTask {
                source: Some("specs/a.md".to_owned()),
                external_id: Some("2".to_owned()),
                ..new_task("a2", 5)
            })
            .unwrap();

        let tasks = store.list_tasks_by_source("specs/a.md").unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].id, t1.id);
        assert_eq!(tasks[1].id, t2.id);
        assert_eq!(tasks[0].title, "a1");
        assert_eq!(tasks[1].title, "a2");

        let empty = store.list_tasks_by_source("specs/empty.md").unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn set_task_title_and_body_changes_stored_title_and_body() {
        let mut store = memory_store();
        let task = store.add_task(new_task("initial title", 0)).unwrap();
        assert_eq!(task.body, "");

        store
            .set_task_title_and_body(task.id, "updated title", "updated body")
            .unwrap();

        let updated = store.get_task(task.id).unwrap();
        assert_eq!(updated.title, "updated title");
        assert_eq!(updated.body, "updated body");

        let err = store
            .set_task_title_and_body(TaskId::new(999), "title", "body")
            .unwrap_err();
        assert!(
            matches!(err, StoreError::UnknownTask { id } if id == TaskId::new(999)),
            "unexpected error: {err:?}"
        );
    }
}
