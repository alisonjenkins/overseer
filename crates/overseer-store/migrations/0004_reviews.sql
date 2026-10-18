ALTER TABLE tasks ADD COLUMN approved INTEGER NOT NULL DEFAULT 0;

CREATE TABLE reviews (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id INTEGER NOT NULL,
    round INTEGER NOT NULL,
    profile TEXT NOT NULL,
    verdict TEXT NOT NULL,
    path TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE INDEX reviews_by_task ON reviews (task_id, round, id);
