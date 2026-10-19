ALTER TABLE tasks ADD COLUMN parent_task_id INTEGER;

CREATE INDEX tasks_parent_task_id ON tasks (parent_task_id);
