ALTER TABLE runs ADD COLUMN skills TEXT;

CREATE TABLE lessons (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id INTEGER NOT NULL,
    skill TEXT NOT NULL,
    base_version INTEGER NOT NULL,
    status TEXT NOT NULL,
    path TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE INDEX lessons_by_task ON lessons (task_id, id);
