ALTER TABLE tasks ADD COLUMN source TEXT;
ALTER TABLE tasks ADD COLUMN external_id TEXT;

CREATE UNIQUE INDEX tasks_source_external_id ON tasks (source, external_id) WHERE external_id IS NOT NULL;
